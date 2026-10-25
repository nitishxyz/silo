//! Changes blocks: status and diffs of every scope in a temporary git
//! repo, staging, unstaging and reverting files and hunks, commits and
//! their hooks, read-only scopes, path safety, `changes.changed`, reads
//! that never take the index lock, and state that survives a restart.

use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use silo_client::{Control, Events};
use silo_proto::{
    control::{
        ChangedFile, ChangesCommitParams, ChangesCommitted, ChangesDiffParams, ChangesHunksParams, ChangesRootParams, ChangesSetParams,
        ChangesStatus, ChangesStatusParams, Created, Event, FileDiff, FileStatus, LineKind, Method, RpcError, SpawnSpec,
    },
    model::{AgentLink, Axis, BlockInfo, BlockKind, ChangeMark, ChangesScope, ChangesState, ReviewComment, Side, TermSize},
    paths,
};
use tokio::task::JoinHandle;

const TIMEOUT: Duration = Duration::from_secs(15);

/// `git args` in `dir`; its stdout.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").current_dir(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

fn write(dir: &Path, rel: &str, text: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// Lines `line 1` … `line n`, `changed` ones replaced.
fn lines(n: usize, changed: &[usize]) -> String {
    (1..=n).map(|i| if changed.contains(&i) { format!("changed {i}\n") } else { format!("line {i}\n") }).collect()
}

/// A fresh home for the server and a repo with one commit on `main`.
fn temp(name: &str) -> (PathBuf, PathBuf) {
    let dir = PathBuf::from(format!("/tmp/silo-changes-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    init(&repo);
    write(&repo, "a.txt", &lines(30, &[]));
    write(&repo, "b.txt", "bee\n");
    write(&repo, "c.txt", "one\ntwo\n");
    write(&repo, "old.txt", &lines(20, &[]).replace("line", "moved"));
    std::fs::write(repo.join("bin.dat"), b"\x00\x01\x02binary").unwrap();
    write(&repo, "src/lib.rs", "pub fn f() {}\n");
    write(&repo, ".gitignore", "target/\n");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "first"]);
    (dir.join("home"), repo.canonicalize().unwrap())
}

fn init(repo: &Path) {
    git(repo, &["init", "-q", "-b", "main"]);
    for (key, value) in [
        ("user.name", "Silo Test"),
        ("user.email", "test@silo.invalid"),
        ("commit.gpgsign", "false"),
        ("core.hooksPath", ".git/hooks"),
        ("core.autocrlf", "false"),
    ] {
        git(repo, &["config", key, value]);
    }
}

fn config(home: &Path) -> silo_server::Config {
    silo_server::Config {
        home: home.to_owned(),
        idle_exit: None,
        checkpoint_interval: Duration::from_millis(200),
        relay: silo_server::RelayConfig::Disabled,
        mdns: false,
        ..Default::default()
    }
}

async fn start(home: &Path) -> JoinHandle<anyhow::Result<()>> {
    let socket = paths::socket(home);
    let inode = |path: &Path| std::fs::metadata(path).ok().map(|m| std::os::unix::fs::MetadataExt::ino(&m));
    let stale = inode(&socket);
    let task = tokio::spawn(silo_server::run(config(home)));
    for _ in 0..300 {
        if inode(&socket).is_some_and(|ino| Some(ino) != stale) && silo_client::server_running(home).await {
            return task;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("server did not start");
}

async fn connect(home: &Path) -> (Control, Events) {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        if let Ok(pair) = Control::connect(&paths::socket(home), "changes-test").await {
            return pair;
        }
        assert!(tokio::time::Instant::now() < deadline, "no server");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn stop(control: Control, task: JoinHandle<anyhow::Result<()>>) {
    control.call_value(Method::ServerShutdown { force: false }).await.unwrap();
    drop(control);
    tokio::time::timeout(TIMEOUT, task).await.unwrap().unwrap().unwrap();
}

/// A session whose terminal (in `cwd`) is split with a changes block
/// opened in `dir` (`None`: the terminal's folder).
async fn changes(control: &Control, cwd: &Path, dir: Option<&Path>) -> Created {
    let spawn = SpawnSpec { command: vec!["/bin/sh".into()], cwd: Some(cwd.display().to_string()), size: Some(TermSize::new(80, 24)) };
    let main: Created = control.call(Method::SessionCreate { name: None, spawn, kind: BlockKind::Terminal, url: None }).await.unwrap();
    let split = Method::BlockSplit {
        block_id: main.block_id,
        axis: Axis::Horizontal,
        spawn: SpawnSpec { cwd: dir.map(|d| d.display().to_string()), ..SpawnSpec::default() },
        kind: BlockKind::Changes,
        url: None,
        agent: None,
    };
    control.call(split).await.unwrap()
}

async fn info(control: &Control, created: &Created) -> BlockInfo {
    let view: silo_proto::model::SessionView = control.call(Method::SessionView { session_id: created.session_id.clone() }).await.unwrap();
    view.blocks.into_iter().find(|b| b.id == created.block_id).expect("changes block")
}

fn code(error: &anyhow::Error) -> &str {
    &error.downcast_ref::<RpcError>().unwrap_or_else(|| panic!("not an rpc error: {error:#}")).code
}

fn set(block: &str) -> ChangesSetParams {
    ChangesSetParams { block_id: block.into(), ..ChangesSetParams::default() }
}

async fn scope(control: &Control, block: &str, scope: ChangesScope) -> ChangesState {
    control.call(Method::ChangesSet(ChangesSetParams { scope: Some(scope), ..set(block) })).await.unwrap()
}

async fn status(control: &Control, block: &str) -> anyhow::Result<ChangesStatus> {
    control.call(Method::ChangesStatus(ChangesStatusParams { block_id: block.into() })).await
}

async fn diff(control: &Control, block: &str, path: &str) -> anyhow::Result<FileDiff> {
    control.call(Method::ChangesDiff(ChangesDiffParams { block_id: block.into(), path: path.into(), context: None })).await
}

fn hunks(block: &str, path: &str, ids: Option<Vec<String>>) -> ChangesHunksParams {
    ChangesHunksParams { block_id: block.into(), path: path.into(), hunks: ids }
}

/// `(path, status, staged, unstaged)` of each file.
fn summary(status: &ChangesStatus) -> Vec<(&str, FileStatus, bool, bool)> {
    status.files.iter().map(|f| (f.path.as_str(), f.status, f.staged, f.unstaged)).collect()
}

fn file<'a>(status: &'a ChangesStatus, path: &str) -> &'a ChangedFile {
    status.files.iter().find(|f| f.path == path).unwrap_or_else(|| panic!("{path} not in {:?}", summary(status)))
}

#[tokio::test]
async fn status_of_every_scope() {
    let (home, repo) = temp("status");
    // Every kind of change: modified, deleted, a staged rename, a staged
    // new file, untracked, binary, staged and changed again.
    write(&repo, "a.txt", &lines(30, &[2]));
    std::fs::remove_file(repo.join("b.txt")).unwrap();
    git(&repo, &["mv", "old.txt", "new.txt"]);
    write(&repo, "d.txt", "dee\n");
    git(&repo, &["add", "d.txt"]);
    write(&repo, "u.txt", "you\ntoo\n");
    write(&repo, "target/build.o", "ignored");
    std::fs::write(repo.join("bin.dat"), b"\x00\x01\x02binary, changed").unwrap();
    write(&repo, "c.txt", "one\ntwo\nthree\n");
    git(&repo, &["add", "c.txt"]);
    write(&repo, "c.txt", "zero\none\ntwo\nthree\n");

    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    // Opened in a subfolder: the root is the repo's top level.
    let created = changes(&control, &repo, Some(&repo.join("src"))).await;
    let id = created.block_id.clone();
    let block = info(&control, &created).await;
    assert_eq!((block.kind, block.title.as_str()), (BlockKind::Changes, "repo"));
    let state = block.changes.as_deref().unwrap().clone();
    assert_eq!(state, ChangesState::new(repo.display().to_string()), "working tree, list shown, 3 lines of context");

    use FileStatus::*;
    let working = status(&control, &id).await.unwrap();
    assert!(working.repo);
    assert_eq!((working.branch.as_deref(), working.upstream.as_deref(), working.ahead), (Some("main"), None, 0));
    assert_eq!(working.head.as_deref(), Some(git(&repo, &["rev-parse", "--short", "HEAD"]).trim()));
    assert_eq!(
        summary(&working),
        [
            ("a.txt", Modified, false, true),
            ("b.txt", Deleted, false, true),
            ("bin.dat", Modified, false, true),
            ("c.txt", Modified, true, true),
            ("d.txt", Added, true, false),
            ("new.txt", Renamed, true, false),
            ("u.txt", Untracked, false, true),
        ]
    );
    assert_eq!(file(&working, "new.txt").old_path.as_deref(), Some("old.txt"));
    assert_eq!((file(&working, "a.txt").added, file(&working, "a.txt").removed), (1, 1));
    assert_eq!((file(&working, "c.txt").added, file(&working, "c.txt").removed), (2, 0), "against HEAD");
    assert_eq!((file(&working, "u.txt").added, file(&working, "u.txt").binary), (2, false));
    assert!(file(&working, "bin.dat").binary);
    assert_eq!((working.totals.files, working.truncated), (7, false));
    assert_eq!(working.totals.added, working.files.iter().map(|f| f.added).sum::<u32>());

    scope(&control, &id, ChangesScope::Staged).await;
    let staged = status(&control, &id).await.unwrap();
    assert_eq!(summary(&staged), [("c.txt", Modified, true, true), ("d.txt", Added, true, false), ("new.txt", Renamed, true, false)]);
    assert_eq!(file(&staged, "c.txt").added, 1, "the index against HEAD");

    scope(&control, &id, ChangesScope::Unstaged).await;
    let unstaged = status(&control, &id).await.unwrap();
    let paths: Vec<_> = unstaged.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["a.txt", "b.txt", "bin.dat", "c.txt", "u.txt"]);

    // Two trees (otto's turn snapshots): HEAD's and the index's.
    let base = git(&repo, &["rev-parse", "HEAD^{tree}"]).trim().to_owned();
    let index = git(&repo, &["write-tree"]).trim().to_owned();
    let tree = ChangesScope::Tree { base: base.clone(), head: Some(index), label: "Turn 1".into() };
    assert_eq!(scope(&control, &id, tree.clone()).await.scope, tree);
    let turn = status(&control, &id).await.unwrap();
    assert_eq!(summary(&turn), [("c.txt", Modified, false, false), ("d.txt", Added, false, false), ("new.txt", Renamed, false, false)]);
    // The work tree against a tree counts untracked files too.
    scope(&control, &id, ChangesScope::Tree { base, head: None, label: "This session".into() }).await;
    let now = status(&control, &id).await.unwrap();
    assert!(now.files.iter().any(|f| f.path == "u.txt" && f.status == Untracked) && now.files.iter().any(|f| f.path == "a.txt"));

    // Branches: what `feature` adds since it left `main`.
    git(&repo, &["stash", "-u", "-q"]);
    git(&repo, &["switch", "-q", "-c", "feature"]);
    write(&repo, "src/lib.rs", "pub fn f() {}\npub fn g() {}\n");
    git(&repo, &["commit", "-q", "-am", "g"]);
    git(&repo, &["switch", "-q", "main"]);
    write(&repo, "b.txt", "bee on main\n");
    git(&repo, &["commit", "-q", "-am", "main moves on"]);
    scope(&control, &id, ChangesScope::Range { base: "main".into(), head: Some("feature".into()) }).await;
    let range = status(&control, &id).await.unwrap();
    assert_eq!(summary(&range), [("src/lib.rs", Modified, false, false)], "from the merge base, as a pull request shows");
    let lib = diff(&control, &id, "src/lib.rs").await.unwrap();
    assert_eq!(lib.hunks[0].lines.last().map(|l| (l.kind, l.text.as_str(), l.new)), Some((LineKind::Added, "pub fn g() {}", Some(2))));
    // A revision that doesn't exist, or looks like an option, is refused.
    scope(&control, &id, ChangesScope::Range { base: "nope".into(), head: None }).await;
    assert_eq!(code(&status(&control, &id).await.unwrap_err()), RpcError::GIT);
    let bad = ChangesSetParams { scope: Some(ChangesScope::Range { base: "--output=/tmp/x".into(), head: None }), ..set(&id) };
    assert_eq!(code(&control.call_value(Method::ChangesSet(bad)).await.unwrap_err()), "bad_request");

    stop(control, task).await;
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}

#[tokio::test]
async fn conflicts_and_folders_outside_a_repo() {
    let (home, repo) = temp("conflict");
    git(&repo, &["switch", "-q", "-c", "other"]);
    write(&repo, "b.txt", "theirs\n");
    git(&repo, &["commit", "-q", "-am", "theirs"]);
    git(&repo, &["switch", "-q", "main"]);
    write(&repo, "b.txt", "ours\n");
    git(&repo, &["commit", "-q", "-am", "ours"]);
    let merge = Command::new("git").current_dir(&repo).args(["merge", "-q", "other"]).output().unwrap();
    assert!(!merge.status.success(), "the merge conflicts");
    let plain = home.parent().unwrap().join("plain");
    std::fs::create_dir_all(&plain).unwrap();

    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let id = changes(&control, &repo, None).await.block_id;
    let working = status(&control, &id).await.unwrap();
    assert_eq!(summary(&working), [("b.txt", FileStatus::Conflicted, true, true)]);

    // Not a repo: the block opens on the folder and says so.
    let created = changes(&control, &plain, None).await;
    let block = info(&control, &created).await;
    assert_eq!(block.changes.unwrap().root, plain.canonicalize().unwrap().display().to_string());
    let none = status(&control, &created.block_id).await.unwrap();
    assert!(!none.repo && none.files.is_empty());
    // Re-rooted on a folder of the repo: its top level.
    let path = repo.join("src").display().to_string();
    let state: ChangesState = control.call(Method::ChangesRoot(ChangesRootParams { block_id: created.block_id.clone(), path })).await.unwrap();
    assert_eq!(state.root, repo.display().to_string());
    assert!(status(&control, &created.block_id).await.unwrap().repo);
    // Changes calls on another kind of block are refused.
    let terminal = info(&control, &created).await.session_id;
    let view: silo_proto::model::SessionView = control.call(Method::SessionView { session_id: terminal }).await.unwrap();
    let shell = view.blocks.iter().find(|b| b.kind == BlockKind::Terminal).unwrap().id.clone();
    assert_eq!(code(&status(&control, &shell).await.unwrap_err()), "bad_request");

    stop(control, task).await;
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}

#[tokio::test]
async fn hunks_stage_unstage_and_revert() {
    let (home, repo) = temp("hunks");
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let id = changes(&control, &repo, Some(&repo)).await.block_id;
    write(&repo, "a.txt", &lines(30, &[2, 12, 25]));

    let a = diff(&control, &id, "a.txt").await.unwrap();
    assert_eq!((a.status, a.binary, a.truncated, a.hunks.len()), (FileStatus::Modified, false, false, 3));
    let second = &a.hunks[1];
    assert_eq!((second.old_start, second.old_lines, second.new_start, second.new_lines), (9, 7, 9, 7));
    let numbered: Vec<_> = second.lines.iter().filter(|l| l.kind != LineKind::Context).map(|l| (l.kind, l.old, l.new, l.text.as_str())).collect();
    assert_eq!(numbered, [(LineKind::Removed, Some(12), None, "line 12"), (LineKind::Added, None, Some(12), "changed 12")]);
    let ids: Vec<String> = a.hunks.iter().map(|h| h.id.clone()).collect();

    // Stage the middle hunk: the index has it, the work tree the others.
    let after: ChangesStatus = control.call(Method::ChangesStage(hunks(&id, "a.txt", Some(vec![ids[1].clone()])))).await.unwrap();
    assert_eq!(summary(&after), [("a.txt", FileStatus::Modified, true, true)]);
    assert_eq!(git(&repo, &["show", ":a.txt"]), lines(30, &[12]));
    scope(&control, &id, ChangesScope::Staged).await;
    let staged = diff(&control, &id, "a.txt").await.unwrap();
    assert_eq!(staged.hunks.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(), [ids[1].as_str()], "the same hunk, the same id");
    scope(&control, &id, ChangesScope::Unstaged).await;
    let rest = diff(&control, &id, "a.txt").await.unwrap();
    assert_eq!(
        rest.hunks.iter().map(|h| h.id.as_str()).collect::<Vec<_>>(),
        [ids[0].as_str(), ids[2].as_str()],
        "ids follow the lines, not their place"
    );

    // Unstage it again (from the staged scope).
    scope(&control, &id, ChangesScope::Staged).await;
    control.call_value(Method::ChangesUnstage(hunks(&id, "a.txt", Some(vec![ids[1].clone()])))).await.unwrap();
    assert_eq!(git(&repo, &["show", ":a.txt"]), lines(30, &[]));
    // Revert in the staged scope is refused: staged changes are not reverted.
    let refused = control.call_value(Method::ChangesRevert(hunks(&id, "a.txt", None))).await.unwrap_err();
    assert_eq!(code(&refused), "bad_request");

    // Revert the last hunk: only its lines go back.
    scope(&control, &id, ChangesScope::Working).await;
    control.call_value(Method::ChangesRevert(hunks(&id, "a.txt", Some(vec![ids[2].clone()])))).await.unwrap();
    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), lines(30, &[2, 12]));

    // A hunk the file no longer has is a conflict; nothing changes.
    write(&repo, "a.txt", &lines(30, &[3, 12]));
    let stale = control.call_value(Method::ChangesStage(hunks(&id, "a.txt", Some(vec![ids[0].clone()])))).await.unwrap_err();
    assert_eq!(code(&stale), RpcError::CONFLICT, "{stale:#}");
    assert_eq!(git(&repo, &["diff", "--cached", "--name-only"]), "");
    let empty = control.call_value(Method::ChangesStage(hunks(&id, "a.txt", Some(vec![])))).await.unwrap_err();
    assert_eq!(code(&empty), "bad_request");

    // Whole files: stage, unstage, revert.
    control.call_value(Method::ChangesStage(hunks(&id, "a.txt", None))).await.unwrap();
    assert_eq!(git(&repo, &["diff", "--cached", "--name-only"]), "a.txt\n");
    control.call_value(Method::ChangesUnstage(hunks(&id, "a.txt", None))).await.unwrap();
    assert_eq!(git(&repo, &["diff", "--cached", "--name-only"]), "");
    let after: ChangesStatus = control.call(Method::ChangesRevert(hunks(&id, "a.txt", None))).await.unwrap();
    assert!(after.files.is_empty(), "{:?}", summary(&after));
    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), lines(30, &[]));
    // An untracked file's one hunk stages it.
    write(&repo, "n.txt", "new\nfile\n");
    let n = diff(&control, &id, "n.txt").await.unwrap();
    assert_eq!((n.status, n.hunks.len(), n.hunks[0].new_lines), (FileStatus::Untracked, 1, 2));
    control.call_value(Method::ChangesStage(hunks(&id, "n.txt", Some(vec![n.hunks[0].id.clone()])))).await.unwrap();
    assert_eq!(git(&repo, &["diff", "--cached", "--name-status"]), "A\tn.txt\n");

    // A turn's base: reverting a hunk takes the lines back to it.
    let base = git(&repo, &["rev-parse", "HEAD^{tree}"]).trim().to_owned();
    git(&repo, &["reset", "-q"]);
    std::fs::remove_file(repo.join("n.txt")).unwrap();
    write(&repo, "a.txt", &lines(30, &[2, 25]));
    git(&repo, &["commit", "-q", "-am", "turn 1"]);
    write(&repo, "a.txt", &lines(30, &[2, 12, 25]));
    scope(&control, &id, ChangesScope::Tree { base: base.clone(), head: None, label: "This session".into() }).await;
    let turn = diff(&control, &id, "a.txt").await.unwrap();
    assert_eq!(turn.hunks.len(), 3);
    control.call_value(Method::ChangesRevert(hunks(&id, "a.txt", Some(vec![turn.hunks[0].id.clone()])))).await.unwrap();
    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), lines(30, &[12, 25]), "back to the base, past HEAD");
    control.call_value(Method::ChangesRevert(hunks(&id, "a.txt", None))).await.unwrap();
    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), lines(30, &[]), "the whole file as the base had it");

    stop(control, task).await;
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}

#[tokio::test]
async fn read_only_scopes_and_paths_outside_the_root_are_refused() {
    let (home, repo) = temp("refuse");
    let outside = home.parent().unwrap().join("outside");
    write(&outside, "secret.txt", "secret\n");
    std::os::unix::fs::symlink(&outside, repo.join("escape")).unwrap();
    write(&repo, "a.txt", &lines(30, &[2]));
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let id = changes(&control, &repo, None).await.block_id;

    for path in ["../outside/secret.txt", "src/../../outside/secret.txt", "escape/secret.txt"] {
        assert_eq!(code(&diff(&control, &id, path).await.unwrap_err()), RpcError::FORBIDDEN, "{path}");
        let stage = control.call_value(Method::ChangesStage(hunks(&id, path, None))).await.unwrap_err();
        assert_eq!(code(&stage), RpcError::FORBIDDEN, "{path}");
    }
    for path in ["/etc/passwd", "~/x", ""] {
        assert_eq!(code(&diff(&control, &id, path).await.unwrap_err()), "bad_request", "{path}");
    }
    let bad = ChangesSetParams { open: Some(vec!["../x".into()]), ..set(&id) };
    assert_eq!(code(&control.call_value(Method::ChangesSet(bad)).await.unwrap_err()), RpcError::FORBIDDEN);

    let head = git(&repo, &["rev-parse", "HEAD"]).trim().to_owned();
    for read_only in [
        ChangesScope::Tree { base: head.clone(), head: Some(head.clone()), label: String::new() },
        ChangesScope::Range { base: "main".into(), head: Some("main".into()) },
    ] {
        scope(&control, &id, read_only).await;
        for method in [Method::ChangesStage, Method::ChangesUnstage, Method::ChangesRevert] {
            let error = control.call_value(method(hunks(&id, "a.txt", None))).await.unwrap_err();
            assert_eq!(code(&error), "bad_request");
        }
    }
    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), lines(30, &[2]), "nothing was written");
    assert_eq!(git(&repo, &["diff", "--cached", "--name-only"]), "");

    stop(control, task).await;
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}

#[tokio::test]
async fn commits_run_hooks() {
    let (home, repo) = temp("commit");
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let id = changes(&control, &repo, None).await.block_id;
    let commit = |message: &str, amend: bool| Method::ChangesCommit(ChangesCommitParams { block_id: id.clone(), message: message.into(), amend });

    write(&repo, "a.txt", &lines(30, &[1]));
    control.call_value(Method::ChangesStage(hunks(&id, "a.txt", None))).await.unwrap();
    // A failing hook fails the commit, with what it said.
    let hook = repo.join(".git/hooks/pre-commit");
    write(&repo, ".git/hooks/pre-commit", "#!/bin/sh\necho 'lint failed: a.txt' >&2\nexit 1\n");
    std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    let error = control.call_value(commit("Change a", false)).await.unwrap_err();
    let rpc = error.downcast_ref::<RpcError>().unwrap();
    assert_eq!(rpc.code, RpcError::GIT);
    assert!(rpc.message.contains("lint failed: a.txt"), "{}", rpc.message);
    assert_eq!(git(&repo, &["log", "--format=%s"]), "first\n");

    // A passing one runs too.
    write(&repo, ".git/hooks/pre-commit", "#!/bin/sh\ntouch hook-ran\n");
    let done: ChangesCommitted = control.call(commit("Change a\n\nWith a body.", false)).await.unwrap();
    assert_eq!(done.sha, git(&repo, &["rev-parse", "HEAD"]).trim());
    assert_eq!(done.summary, "Change a");
    assert!(repo.join("hook-ran").exists());
    assert_eq!(git(&repo, &["log", "-1", "--format=%B"]), "Change a\n\nWith a body.\n\n");
    let amended: ChangesCommitted = control.call(commit("Change a, amended", true)).await.unwrap();
    assert_eq!((amended.summary.as_str(), git(&repo, &["rev-list", "--count", "HEAD"]).trim()), ("Change a, amended", "2"));
    // Nothing staged, or no message: refused.
    std::fs::remove_file(repo.join("hook-ran")).unwrap();
    assert_eq!(code(&control.call_value(commit("Nothing", false)).await.unwrap_err()), RpcError::GIT);
    assert_eq!(code(&control.call_value(commit("  \n", false)).await.unwrap_err()), "bad_request");

    stop(control, task).await;
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}

#[tokio::test]
async fn reads_never_take_the_index_lock() {
    let (home, repo) = temp("lock");
    write(&repo, "a.txt", &lines(30, &[2]));
    write(&repo, "u.txt", "u\n");
    // Another git (an agent's) holds the lock.
    let lock = repo.join(".git/index.lock");
    std::fs::write(&lock, "held").unwrap();
    let index = std::fs::read(repo.join(".git/index")).unwrap();
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let id = changes(&control, &repo, None).await.block_id;

    for read_scope in [ChangesScope::Working, ChangesScope::Staged, ChangesScope::Unstaged] {
        scope(&control, &id, read_scope).await;
        status(&control, &id).await.unwrap();
        diff(&control, &id, "a.txt").await.unwrap();
    }
    assert_eq!(std::fs::read_to_string(&lock).unwrap(), "held");
    assert_eq!(std::fs::read(repo.join(".git/index")).unwrap(), index, "the index is untouched");
    // A write needs the lock, and says why it can't have it.
    let error = control.call_value(Method::ChangesStage(hunks(&id, "a.txt", None))).await.unwrap_err();
    assert!(code(&error) == RpcError::GIT && format!("{error:#}").contains("index.lock"), "{error:#}");
    std::fs::remove_file(&lock).unwrap();

    stop(control, task).await;
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}

/// Whether a `changes.changed` of `block` arrives within `wait`.
async fn changed(events: &mut Events, block: &str, wait: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + wait;
    while let Ok(event) = tokio::time::timeout_at(deadline, events.recv()).await {
        if let (_, Event::ChangesChanged { block_id }) = event.expect("events")
            && block_id == block
        {
            return true;
        }
    }
    false
}

/// Until no `changes.changed` of `block` came for a second.
async fn settle(events: &mut Events, block: &str) {
    while changed(events, block, Duration::from_secs(1)).await {}
}

#[tokio::test]
async fn edits_commits_and_branch_switches_are_reported() {
    let (home, repo) = temp("watch");
    let task = start(&home).await;
    let (control, mut events) = connect(&home).await;
    control.subscribe().await.unwrap();
    let id = changes(&control, &repo, None).await.block_id;

    // An edit (once the watch runs: FSEvents can take seconds to start).
    let deadline = tokio::time::Instant::now() + 3 * TIMEOUT;
    for n in 0.. {
        write(&repo, "a.txt", &lines(30, &[n % 30 + 1]));
        if changed(&mut events, &id, Duration::from_millis(700)).await {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "no changes.changed for an edit");
    }
    settle(&mut events, &id).await;
    // Ignored files are not.
    write(&repo, "target/out.o", "build");
    assert!(!changed(&mut events, &id, Duration::from_secs(1)).await, "target/ is ignored");
    // A commit (the index and the branch's ref).
    git(&repo, &["commit", "-q", "-am", "edit"]);
    assert!(changed(&mut events, &id, TIMEOUT).await, "no changes.changed for a commit");
    settle(&mut events, &id).await;
    // A branch switch (HEAD).
    git(&repo, &["switch", "-q", "-c", "other"]);
    assert!(changed(&mut events, &id, TIMEOUT).await, "no changes.changed for a branch switch");
    settle(&mut events, &id).await;

    control.call_value(Method::BlockClose { block_id: id.clone(), force: false }).await.unwrap();
    write(&repo, "after.txt", "1");
    assert!(!changed(&mut events, &id, Duration::from_secs(1)).await, "a closed block's watch stops");
    stop(control, task).await;
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}

/// A changes block with every part of its state set.
async fn prepare(control: &Control, repo: &Path) -> (Created, ChangesState) {
    let created = changes(control, repo, None).await;
    let id = created.block_id.clone();
    let comment = |id: &str, line: u32, end_line: Option<u32>, text: &str| ReviewComment {
        id: id.into(),
        path: "./a.txt".into(),
        side: Side::New,
        line,
        end_line,
        text: text.into(),
        created_at: 1_761_300_000_000,
    };
    let params = ChangesSetParams {
        scope: Some(ChangesScope::Tree { base: "4b825dc642cb6eb9a060e54bf8d69288fbee4904".into(), head: None, label: "Turn 4".into() }),
        open: Some(vec!["a.txt".into(), "a.txt".into(), "src/lib.rs".into()]),
        active: Some("b.txt".into()),
        list: Some(false),
        split: Some(true),
        context: Some(5),
        comments: Some(vec![comment("c1", 20, Some(14), "  why not 80?  "), comment("c2", 3, Some(3), "ok")]),
        agent: Some(Some(AgentLink { otto_session: "ses_01".into(), block_id: None })),
        marks: Some(vec![ChangeMark { path: "a.txt".into(), line: 2, note: Some(" the risky part ".into()) }]),
        ..set(&id)
    };
    let state: ChangesState = control.call(Method::ChangesSet(params)).await.unwrap();
    assert_eq!(state.open, ["a.txt", "src/lib.rs", "b.txt"], "deduplicated; the active file opened");
    assert_eq!((state.active.as_deref(), state.list, state.split, state.context), (Some("b.txt"), false, true, 5));
    let c = &state.comments;
    assert_eq!((c[0].path.as_str(), c[0].line, c[0].end_line, c[0].text.as_str()), ("a.txt", 14, Some(20), "why not 80?"));
    assert_eq!((c[1].line, c[1].end_line), (3, None));
    assert_eq!(state.marks[0].note.as_deref(), Some("the risky part"));
    assert_eq!(info(control, &created).await.changes.as_deref().unwrap().clone(), state);
    // Omitted fields stay; `agent: null` unlinks.
    let same: ChangesState = control.call(Method::ChangesSet(set(&id))).await.unwrap();
    assert_eq!(same, state);
    let raw = serde_json::json!({"id": 1, "method": "changes.set", "params": {"block_id": id, "agent": null}});
    let request: silo_proto::control::Request = serde_json::from_value(raw).unwrap();
    let unlinked: ChangesState = control.call(request.method).await.unwrap();
    assert_eq!(unlinked.agent, None);
    let relink = ChangesSetParams { agent: state.agent.clone().map(Some), ..set(&id) };
    let state: ChangesState = control.call(Method::ChangesSet(relink)).await.unwrap();
    // Bad comments and marks are refused.
    for bad in [
        ChangesSetParams { comments: Some(vec![comment("c", 0, None, "x")]), ..set(&id) },
        ChangesSetParams { comments: Some(vec![comment("c", 1, None, "x"), comment("c", 2, None, "y")]), ..set(&id) },
        ChangesSetParams { comments: Some(vec![comment("c", 1, None, " ")]), ..set(&id) },
        ChangesSetParams { marks: Some(vec![ChangeMark { path: "a.txt".into(), line: 0, note: None }]), ..set(&id) },
        ChangesSetParams { marks: Some(vec![ChangeMark { path: "a.txt".into(), line: 1, note: None }; ChangeMark::MAX + 1]), ..set(&id) },
        ChangesSetParams { context: Some(ChangesState::CONTEXT_MAX + 1), ..set(&id) },
    ] {
        assert_eq!(code(&control.call_value(Method::ChangesSet(bad)).await.unwrap_err()), "bad_request");
    }
    (created, state)
}

#[tokio::test]
async fn state_survives_restart_and_takeover() {
    let (home, repo) = temp("persist");
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let (created, before) = prepare(&control, &repo).await;
    stop(control, task).await;

    let task = start(&home).await;
    let (control, mut events) = connect(&home).await;
    control.subscribe().await.unwrap();
    let block = info(&control, &created).await;
    assert_eq!((block.kind, block.changes.as_deref()), (BlockKind::Changes, Some(&before)));
    // The watch resumes.
    let deadline = tokio::time::Instant::now() + 3 * TIMEOUT;
    for n in 0.. {
        write(&repo, "a.txt", &lines(30, &[n % 30 + 1]));
        if changed(&mut events, &created.block_id, Duration::from_millis(700)).await {
            break;
        }
        assert!(tokio::time::Instant::now() < deadline, "no changes.changed after a restart");
    }

    let new = tokio::spawn(silo_server::takeover(config(&home), false));
    tokio::time::timeout(TIMEOUT, task).await.unwrap().unwrap().expect("old server commits and exits");
    drop((control, events));
    let (control, _) = connect(&home).await;
    assert_eq!(info(&control, &created).await.changes.as_deref(), Some(&before));
    assert!(status(&control, &created.block_id).await.unwrap().repo);
    stop(control, new).await;
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}

#[tokio::test]
async fn a_review_split_off_an_agent_goes_back_to_it() {
    let (home, repo) = temp("agent");
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let spawn = SpawnSpec { cwd: Some(repo.display().to_string()), ..SpawnSpec::default() };
    let agent: Created = control.call(Method::SessionCreate { name: None, spawn, kind: BlockKind::Agent, url: None }).await.unwrap();
    control
        .call_value(Method::AgentSet { block_id: agent.block_id.clone(), session_id: Some("ses_42".into()), title: None, cwd: None })
        .await
        .unwrap();
    let split = Method::BlockSplit {
        block_id: agent.block_id.clone(),
        axis: Axis::Horizontal,
        spawn: SpawnSpec::default(),
        kind: BlockKind::Changes,
        url: None,
        agent: None,
    };
    let created: Created = control.call(split).await.unwrap();
    let state = info(&control, &created).await.changes.unwrap();
    assert_eq!(state.root, repo.display().to_string(), "in the agent's folder");
    assert_eq!(state.agent, Some(AgentLink { otto_session: "ses_42".into(), block_id: Some(agent.block_id.clone()) }));
    stop(control, task).await;
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}
