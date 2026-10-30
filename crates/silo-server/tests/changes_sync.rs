//! Changes blocks push, pull and fetch against a local bare repo, resolve
//! conflicts (regions, whole sides, mark resolved, continue and abort) and
//! read pull requests through a stand-in `gh`.

use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use silo_client::{Control, Events};
use silo_proto::{
    control::{
        ChangesConflictParams, ChangesPr, ChangesPrCreateParams, ChangesPushParams, ChangesPushed, ChangesResolveParams, ChangesStatus,
        ChangesStatusParams, ConflictFile, Created, Event, GhState, Method, PullRequest, RepoOp, RpcError, SpawnSpec, SyncOp, Take,
    },
    model::{Axis, BlockKind, TermSize},
    paths,
};
use tokio::task::JoinHandle;

const TIMEOUT: Duration = Duration::from_secs(15);

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").current_dir(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn write(dir: &Path, rel: &str, text: &str) {
    std::fs::write(dir.join(rel), text).unwrap();
}

fn init(repo: &Path) {
    git(repo, &["init", "-q", "-b", "main"]);
    for (key, value) in
        [("user.name", "Silo Test"), ("user.email", "test@silo.invalid"), ("commit.gpgsign", "false"), ("core.hooksPath", ".git/hooks")]
    {
        git(repo, &["config", key, value]);
    }
}

/// A home, a repo with one commit and a bare repo as its `origin`.
fn temp(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let dir = PathBuf::from(format!("/tmp/silo-sync-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let (repo, remote) = (dir.join("repo"), dir.join("remote.git"));
    std::fs::create_dir_all(&repo).unwrap();
    git(&dir, &["init", "-q", "--bare", "-b", "main", "remote.git"]);
    init(&repo);
    write(&repo, "a.txt", "one\ntwo\nthree\n");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "first"]);
    git(&repo, &["remote", "add", "origin", &remote.display().to_string()]);
    (dir.join("home"), repo.canonicalize().unwrap(), remote)
}

async fn start(home: &Path) -> JoinHandle<anyhow::Result<()>> {
    let config = silo_server::Config {
        home: home.to_owned(),
        idle_exit: None,
        checkpoint_interval: Duration::from_millis(200),
        relay: silo_server::RelayConfig::Disabled,
        mdns: false,
        ..Default::default()
    };
    let socket = paths::socket(home);
    let task = tokio::spawn(silo_server::run(config));
    for _ in 0..300 {
        if socket.exists() && silo_client::server_running(home).await {
            return task;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("server did not start");
}

async fn connect(home: &Path) -> (Control, Events) {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        if let Ok(pair) = Control::connect(&paths::socket(home), "sync-test").await {
            return pair;
        }
        assert!(tokio::time::Instant::now() < deadline, "no server");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn stop(control: Control, task: JoinHandle<anyhow::Result<()>>, dir: &Path) {
    control.call_value(Method::ServerShutdown { force: false }).await.unwrap();
    drop(control);
    tokio::time::timeout(TIMEOUT, task).await.unwrap().unwrap().unwrap();
    let _ = std::fs::remove_dir_all(dir);
}

/// A changes block on `repo`, split off a terminal there.
async fn block(control: &Control, repo: &Path) -> String {
    let spawn = SpawnSpec { command: vec!["/bin/sh".into()], cwd: Some(repo.display().to_string()), size: Some(TermSize::new(80, 24)) };
    let main: Created = control.call(Method::SessionCreate { name: None, spawn, kind: BlockKind::Terminal, url: None }).await.unwrap();
    let split = Method::BlockSplit {
        block_id: main.block_id,
        axis: Axis::Horizontal,
        spawn: SpawnSpec::default(),
        kind: BlockKind::Changes,
        url: None,
        agent: None,
    };
    control.call::<Created>(split).await.unwrap().block_id
}

fn p(block: &str) -> ChangesStatusParams {
    ChangesStatusParams { block_id: block.into() }
}

async fn status(control: &Control, block: &str) -> ChangesStatus {
    control.call(Method::ChangesStatus(p(block))).await.unwrap()
}

async fn push(control: &Control, block: &str) -> anyhow::Result<ChangesPushed> {
    control.call(Method::ChangesPush(ChangesPushParams { block_id: block.into() })).await
}

fn rpc(error: &anyhow::Error) -> &RpcError {
    error.downcast_ref::<RpcError>().unwrap_or_else(|| panic!("not an rpc error: {error:#}"))
}

#[tokio::test]
async fn push_sets_the_upstream_then_pull_and_fetch_follow_the_remote() {
    let (home, repo, remote) = temp("push");
    let task = start(&home).await;
    let (control, mut events) = connect(&home).await;
    control.subscribe().await.unwrap();
    let id = block(&control, &repo).await;
    let before = status(&control, &id).await;
    assert_eq!((before.remote.as_deref(), before.upstream.as_deref(), before.op), (Some("origin"), None, None));

    // The first push sets the upstream.
    let pushed = push(&control, &id).await.unwrap();
    assert_eq!(pushed, ChangesPushed { remote: "origin".into(), branch: "main".into(), set_upstream: true, up_to_date: false });
    assert_eq!(git(&remote, &["rev-parse", "main"]), git(&repo, &["rev-parse", "HEAD"]));
    let after = status(&control, &id).await;
    assert_eq!((after.upstream.as_deref(), after.ahead, after.behind), (Some("origin/main"), 0, 0));
    // Progress came as changes.progress of this block.
    let mut progress = Vec::new();
    while let Ok(Some((_, event))) = tokio::time::timeout(Duration::from_millis(300), events.recv()).await {
        if let Event::ChangesProgress { block_id, op: SyncOp::Push, text } = event {
            assert_eq!(block_id, id);
            progress.push(text);
        }
    }
    assert!(!progress.is_empty(), "git's progress is reported");

    write(&repo, "a.txt", "one\ntwo\nthree\nfour\n");
    git(&repo, &["commit", "-q", "-am", "second"]);
    assert_eq!(status(&control, &id).await.ahead, 1);
    let again = push(&control, &id).await.unwrap();
    assert!(!again.set_upstream && !again.up_to_date);
    assert!(push(&control, &id).await.unwrap().up_to_date);

    // Someone else pushes: ours is refused, fetch counts it, pull merges it.
    let other = home.parent().unwrap().join("other");
    git(home.parent().unwrap(), &["clone", "-q", &remote.display().to_string(), "other"]);
    init_user(&other);
    write(&other, "b.txt", "bee\n");
    git(&other, &["add", "b.txt"]);
    git(&other, &["commit", "-q", "-m", "theirs"]);
    git(&other, &["push", "-q", "origin", "main"]);
    write(&repo, "c.txt", "sea\n");
    git(&repo, &["add", "c.txt"]);
    git(&repo, &["commit", "-q", "-m", "ours"]);
    let refused = push(&control, &id).await.unwrap_err();
    assert_eq!((rpc(&refused).code.as_str(), rpc(&refused).message.as_str()), ("git", "The remote has commits this branch lacks: pull first"));
    let fetched: ChangesStatus = control.call(Method::ChangesFetch(p(&id))).await.unwrap();
    assert_eq!((fetched.ahead, fetched.behind), (1, 1));
    let pulled: ChangesStatus = control.call(Method::ChangesPull(p(&id))).await.unwrap();
    assert_eq!((pulled.ahead, pulled.behind, pulled.op), (2, 0, None), "merged");
    assert!(repo.join("b.txt").exists());
    push(&control, &id).await.unwrap();

    // A remote that can't be reached fails at once, saying so.
    git(&repo, &["remote", "set-url", "origin", "http://127.0.0.1:9/nothing.git"]);
    let unreachable = push(&control, &id).await.unwrap_err();
    assert_eq!(rpc(&unreachable).message, "Couldn't reach origin: check the network");
    stop(control, task, home.parent().unwrap()).await;
}

fn init_user(repo: &Path) {
    for (key, value) in [("user.name", "Other"), ("user.email", "other@silo.invalid"), ("commit.gpgsign", "false")] {
        git(repo, &["config", key, value]);
    }
}

#[tokio::test]
async fn pushing_without_a_branch_or_remote_is_refused() {
    let (home, repo, _) = temp("refuse");
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let id = block(&control, &repo).await;
    let head = git(&repo, &["rev-parse", "HEAD"]);
    git(&repo, &["checkout", "-q", "--detach", head.trim()]);
    let detached = push(&control, &id).await.unwrap_err();
    assert_eq!((rpc(&detached).code.as_str(), rpc(&detached).message.as_str()), ("bad_request", "Check out a branch to push: HEAD is detached"));
    git(&repo, &["checkout", "-q", "main"]);
    git(&repo, &["remote", "remove", "origin"]);
    assert_eq!(status(&control, &id).await.remote, None);
    assert_eq!(rpc(&push(&control, &id).await.unwrap_err()).message, "This repo has no remote to push to");
    let pull = control.call::<ChangesStatus>(Method::ChangesPull(p(&id))).await.unwrap_err();
    assert_eq!(rpc(&pull).message, "This branch has no upstream: push it first");
    stop(control, task, home.parent().unwrap()).await;
}

/// `repo` stopped in a merge of `other` with two conflicting regions in
/// a.txt and one in b.txt.
fn conflicted(repo: &Path) {
    write(repo, "a.txt", "1\n2\n3\n4\n5\n6\n7\n8\n");
    write(repo, "b.txt", "bee\n");
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "base"]);
    git(repo, &["switch", "-q", "-c", "other"]);
    write(repo, "a.txt", "1\ntheirs 2\n3\n4\n5\n6\ntheirs 7\n8\n");
    write(repo, "b.txt", "their bee\n");
    git(repo, &["commit", "-q", "-am", "theirs"]);
    git(repo, &["switch", "-q", "main"]);
    write(repo, "a.txt", "1\nours 2\n3\n4\n5\n6\nours 7\n8\n");
    write(repo, "b.txt", "our bee\n");
    git(repo, &["commit", "-q", "-am", "ours"]);
    let merge = Command::new("git").current_dir(repo).args(["merge", "-q", "other"]).output().unwrap();
    assert!(!merge.status.success(), "the merge conflicts");
}

async fn resolve(control: &Control, block: &str, path: &str, take: Option<Take>, regions: Option<Vec<String>>) -> anyhow::Result<ChangesStatus> {
    control.call(Method::ChangesResolve(ChangesResolveParams { block_id: block.into(), path: path.into(), take, regions })).await
}

#[tokio::test]
async fn conflicts_resolve_by_region_or_side_then_the_merge_continues() {
    let (home, repo, _) = temp("conflicts");
    conflicted(&repo);
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let id = block(&control, &repo).await;
    assert_eq!(status(&control, &id).await.op, Some(RepoOp::Merge));

    let file: ConflictFile =
        control.call(Method::ChangesConflict(ChangesConflictParams { block_id: id.clone(), path: "a.txt".into() })).await.unwrap();
    let sides: Vec<_> =
        file.regions.iter().map(|r| (r.line, r.ours.clone(), r.theirs.clone(), r.ours_label.as_str(), r.theirs_label.as_str())).collect();
    assert_eq!(
        sides,
        [
            (2, vec!["ours 2".to_owned()], vec!["theirs 2".to_owned()], "HEAD", "other"),
            (11, vec!["ours 7".into()], vec!["theirs 7".into()], "HEAD", "other")
        ]
    );
    // Not resolved while markers are left; continuing needs every file.
    let early = resolve(&control, &id, "a.txt", None, None).await.unwrap_err();
    assert_eq!(rpc(&early).message, "2 conflicts are still in the file: take a side first");
    let cont = control.call::<ChangesStatus>(Method::ChangesContinue(p(&id))).await.unwrap_err();
    assert_eq!(rpc(&cont).message, "Resolve the 2 conflicted files first");

    // One region each way, then marked resolved.
    let (first, second) = (file.regions[0].id.clone(), file.regions[1].id.clone());
    resolve(&control, &id, "a.txt", Some(Take::Theirs), Some(vec![first.clone()])).await.unwrap();
    let stale = resolve(&control, &id, "a.txt", Some(Take::Ours), Some(vec![first])).await.unwrap_err();
    assert_eq!(rpc(&stale).code, "conflict");
    resolve(&control, &id, "a.txt", Some(Take::Both), Some(vec![second])).await.unwrap();
    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "1\ntheirs 2\n3\n4\n5\n6\nours 7\ntheirs 7\n8\n");
    resolve(&control, &id, "a.txt", None, None).await.unwrap();
    // A whole side of b.txt resolves it.
    let left = resolve(&control, &id, "b.txt", Some(Take::Ours), None).await.unwrap();
    assert_eq!(std::fs::read_to_string(repo.join("b.txt")).unwrap(), "our bee\n");
    assert!(left.files.iter().all(|f| f.status != silo_proto::control::FileStatus::Conflicted), "{:?}", left.files);

    let done: ChangesStatus = control.call(Method::ChangesContinue(p(&id))).await.unwrap();
    assert_eq!(done.op, None);
    assert_eq!(git(&repo, &["log", "-1", "--format=%P"]).split_whitespace().count(), 2, "a merge commit");
    stop(control, task, home.parent().unwrap()).await;
}

#[tokio::test]
async fn an_aborted_merge_goes_back() {
    let (home, repo, _) = temp("abort");
    conflicted(&repo);
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let id = block(&control, &repo).await;
    let back: ChangesStatus = control.call(Method::ChangesAbort(p(&id))).await.unwrap();
    assert_eq!((back.op, back.files.len()), (None, 0));
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head);
    let nothing = control.call::<ChangesStatus>(Method::ChangesAbort(p(&id))).await.unwrap_err();
    assert_eq!(rpc(&nothing).message, "Nothing to abort");
    stop(control, task, home.parent().unwrap()).await;
}

/// A stand-in `gh` in `dir`: `pr view` answers `dir/pr.json` (none: no
/// pull request), `pr create` records its args and body and makes #7.
fn fake_gh(dir: &Path) {
    let d = dir.display();
    let script = format!(
        "#!/bin/sh\ncase \"$1 $2\" in\n\"pr view\") if [ -f {d}/pr.json ]; then cat {d}/pr.json; else echo 'no pull requests found for branch \"feature\"' >&2; exit 1; fi;;\n\
         \"pr create\") echo \"$@\" > {d}/args; cat > {d}/body; echo '{{\"number\":7,\"url\":\"https://github.com/o/r/pull/7\",\"title\":\"T\",\"state\":\"OPEN\",\"isDraft\":true,\"baseRefName\":\"main\"}}' > {d}/pr.json; echo https://github.com/o/r/pull/7;;\n\
         *) exit 2;;\nesac\n"
    );
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("gh"), script).unwrap();
    Command::new("chmod").arg("755").arg(dir.join("gh")).status().unwrap();
    let path = format!("{}:{}", dir.display(), std::env::var("PATH").unwrap_or_default());
    // SAFETY: nextest runs each test in its own process; nothing else here reads PATH concurrently.
    unsafe { std::env::set_var("PATH", path) };
}

#[tokio::test]
async fn pull_requests_through_gh() {
    let (home, repo, remote) = temp("pr");
    let bin = home.parent().unwrap().join("bin");
    fake_gh(&bin);
    git(&repo, &["push", "-q", "-u", "origin", "main"]);
    git(&repo, &["remote", "set-head", "origin", "main"]);
    git(&repo, &["switch", "-q", "-c", "fix-login_bug"]);
    for n in ["one", "two"] {
        write(&repo, "a.txt", &format!("{n}\n"));
        git(&repo, &["commit", "-q", "-am", &format!("Change {n}")]);
    }
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let id = block(&control, &repo).await;

    let fresh: ChangesPr = control.call(Method::ChangesPr(p(&id))).await.unwrap();
    assert_eq!(fresh.gh, GhState::Ready);
    assert_eq!(
        (fresh.pr, fresh.bases, fresh.title.as_str(), fresh.body.as_str()),
        (None, vec!["main".to_owned()], "Fix login bug", "- Change one\n- Change two")
    );

    // Creating pushes the branch first.
    let params =
        ChangesPrCreateParams { block_id: id.clone(), title: "Fix login".into(), body: "Why".into(), base: Some("main".into()), draft: true };
    let made: PullRequest = control.call(Method::ChangesPrCreate(params)).await.unwrap();
    assert_eq!((made.number, made.url.as_str(), made.draft), (7, "https://github.com/o/r/pull/7", true));
    assert_eq!(git(&remote, &["rev-parse", "fix-login_bug"]), git(&repo, &["rev-parse", "HEAD"]));
    let args = std::fs::read_to_string(bin.join("args")).unwrap();
    assert!(
        args.contains("--title Fix login") && args.contains("--head fix-login_bug") && args.contains("--base main") && args.contains("--draft"),
        "{args}"
    );
    assert_eq!(std::fs::read_to_string(bin.join("body")).unwrap(), "Why");
    let shown: ChangesPr = control.call(Method::ChangesPr(p(&id))).await.unwrap();
    assert_eq!(shown.pr.map(|pr| (pr.number, pr.state, pr.base)), Some((7, "open".to_owned(), "main".to_owned())));
    stop(control, task, home.parent().unwrap()).await;
}
