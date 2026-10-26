//! Search (docs/FINDER.md §2–3): `search.files` ranking, query syntax and
//! frecency, `search.grep`, the root rule, the picker cache and the
//! `indexing` flag; `files.find` through the same picker in a repo.

use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use silo_client::Control;
use silo_proto::{
    changes::FileStatus,
    compat::feature,
    control::{Created, FileFound, FilesFindParams, Method, RpcError, SpawnSpec},
    model::{Axis, BlockKind, TermSize},
    paths,
    search::{SearchFiles, SearchFilesParams, SearchGrep, SearchGrepMode, SearchGrepParams, SearchOpenedParams},
};
use tokio::task::JoinHandle;

const TIMEOUT: Duration = Duration::from_secs(15);

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git").current_dir(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn write(dir: &Path, rel: &str, text: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/silo-search-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// A home, a repo (one commit, then `src/lib.rs` modified and `new.rs`
/// untracked) and a folder outside it.
fn temp(name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let dir = temp_dir(name);
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    for (key, value) in
        [("user.name", "Silo Test"), ("user.email", "test@silo.invalid"), ("commit.gpgsign", "false"), ("core.hooksPath", ".git/hooks")]
    {
        git(&repo, &["config", key, value]);
    }
    write(&repo, "src/main.rs", "fn main() {\n    println!(\"hi\");\n}\n");
    write(&repo, "src/lib.rs", "// lib\npub fn alpha() {}\npub fn beta() {}\n");
    write(&repo, "src/search.rs", "pub fn search() {}\n");
    write(&repo, "src/research_notes.rs", "// notes\n");
    write(&repo, "tests/search_test.rs", "#[test]\nfn t() {}\n");
    write(&repo, "docs/search.md", "# Search\n\nFind things: héllo wörld.\n");
    write(&repo, "README.md", "readme\n");
    write(&repo, ".github/ci.yml", "on: push\n");
    write(&repo, ".gitignore", "target/\n*.log\n");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "first"]);
    write(&repo, "src/lib.rs", "// lib\npub fn alpha() {}\npub fn beta() {}\npub fn gamma() {}\n");
    write(&repo, "new.rs", "fn new() {}\n");
    write(&repo, "target/debug/out.rs", "fn ignored() {}\n");
    write(&repo, "debug.log", "fn main() {}\n");
    let outside = dir.join("outside");
    write(&outside, "secret.rs", "fn main() {}\n");
    (dir.join("home"), repo, outside)
}

async fn start(home: &Path, search: silo_server::search::Limits) -> JoinHandle<anyhow::Result<()>> {
    let config = silo_server::Config {
        home: home.to_owned(),
        idle_exit: None,
        checkpoint_interval: Duration::from_millis(200),
        relay: silo_server::RelayConfig::Disabled,
        mdns: false,
        search,
        ..Default::default()
    };
    let socket = paths::socket(home);
    let task = tokio::spawn(silo_server::run(config));
    for _ in 0..500 {
        if socket.exists() && silo_client::server_running(home).await {
            return task;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("server did not start");
}

async fn connect(home: &Path) -> Control {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        if let Ok((control, _)) = Control::connect(&paths::socket(home), "search-test").await {
            return control;
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

/// A session working in `root`: a terminal there split with a files block
/// at it. Returns the files block.
async fn workspace(control: &Control, root: &Path) -> Created {
    let cwd = Some(root.display().to_string());
    let spawn = SpawnSpec { command: vec!["/bin/sh".into()], cwd: cwd.clone(), size: Some(TermSize::new(80, 24)) };
    let main: Created = control.call(Method::SessionCreate { name: None, spawn, kind: BlockKind::Terminal, url: None }).await.unwrap();
    let split = Method::BlockSplit {
        block_id: main.block_id,
        axis: Axis::Horizontal,
        spawn: SpawnSpec { cwd, ..SpawnSpec::default() },
        kind: BlockKind::Files,
        url: None,
        agent: None,
    };
    control.call(split).await.unwrap()
}

fn code(error: &anyhow::Error) -> &str {
    &error.downcast_ref::<RpcError>().unwrap_or_else(|| panic!("not an rpc error: {error:#}")).code
}

async fn files_raw(control: &Control, root: &Path, query: &str) -> anyhow::Result<SearchFiles> {
    control.call(Method::SearchFiles(SearchFilesParams { root: root.display().to_string(), query: query.into(), limit: None })).await
}

/// `search.files`, once the folder is indexed.
async fn files(control: &Control, root: &Path, query: &str) -> SearchFiles {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let found = files_raw(control, root, query).await.unwrap();
        if !found.indexing {
            return found;
        }
        assert!(tokio::time::Instant::now() < deadline, "{} still indexing", root.display());
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn paths_of(found: &SearchFiles) -> Vec<&str> {
    found.items.iter().map(|i| i.path.as_str()).collect()
}

async fn grep(control: &Control, root: &Path, query: &str, mode: Option<SearchGrepMode>) -> anyhow::Result<SearchGrep> {
    control.call(Method::SearchGrep(SearchGrepParams { root: root.display().to_string(), query: query.into(), mode, limit: None })).await
}

async fn opened(control: &Control, root: &Path, path: &str) -> anyhow::Result<serde_json::Value> {
    control.call_value(Method::SearchOpened(SearchOpenedParams { root: root.display().to_string(), path: path.into() })).await
}

#[tokio::test(flavor = "multi_thread")]
async fn files_are_ranked_filtered_and_learn_from_opens() {
    let (home, repo, _) = temp("files");
    let task = start(&home, silo_server::search::Limits::default()).await;
    let control = connect(&home).await;
    assert!(control.peer_supports(feature::SEARCH));
    let block = workspace(&control, &repo).await;

    // Everything not ignored; dotfiles of a repo included.
    let all = files(&control, &repo, "").await;
    let everything = paths_of(&all);
    for path in ["src/main.rs", "src/lib.rs", "new.rs", "README.md", ".github/ci.yml", ".gitignore"] {
        assert!(everything.contains(&path), "{path}: {everything:?}");
    }
    for path in ["target/debug/out.rs", "debug.log"] {
        assert!(!everything.contains(&path), "{path}: {everything:?}");
    }
    assert_eq!(all.total, all.items.len() as u64);
    let lib = all.items.iter().find(|i| i.path == "src/lib.rs").unwrap();
    assert_eq!((lib.name.as_str(), lib.git), ("lib.rs", Some(FileStatus::Modified)));
    assert_eq!(all.items.iter().find(|i| i.path == "new.rs").unwrap().git, Some(FileStatus::Untracked));
    assert_eq!(all.items.iter().find(|i| i.path == "src/main.rs").unwrap().git, None);

    // The exact name first.
    let exact = files(&control, &repo, "search.rs").await;
    assert_eq!(exact.items.first().map(|i| i.path.as_str()), Some("src/search.rs"), "{:?}", paths_of(&exact));
    let found = files(&control, &repo, "main").await;
    assert_eq!(found.items.first().map(|i| i.path.as_str()), Some("src/main.rs"), "{:?}", paths_of(&found));
    assert!(found.items.windows(2).all(|w| w[0].score >= w[1].score), "best first");

    // Globs, excludes, git status, path:line.
    let rs = files(&control, &repo, "*.rs").await;
    assert!(!rs.items.is_empty() && rs.items.iter().all(|i| i.path.ends_with(".rs")), "{:?}", paths_of(&rs));
    assert!(!paths_of(&rs).contains(&"target/debug/out.rs"));
    let no_src = files(&control, &repo, "search !src").await;
    assert!(!no_src.items.is_empty() && no_src.items.iter().all(|i| !i.path.starts_with("src/")), "{:?}", paths_of(&no_src));
    let modified = files(&control, &repo, "git:modified").await;
    let modified_paths = paths_of(&modified);
    assert!(modified_paths.contains(&"src/lib.rs"), "{modified_paths:?}");
    assert!(!modified_paths.contains(&"src/main.rs") && !modified_paths.contains(&"README.md"), "{modified_paths:?}");
    let located = files(&control, &repo, "main.rs:2").await;
    assert_eq!(located.items.first().map(|i| i.path.as_str()), Some("src/main.rs"), "{:?}", paths_of(&located));

    // Opening a file moves it up.
    let before = paths_of(&files(&control, &repo, "search").await).iter().position(|p| *p == "tests/search_test.rs").unwrap();
    assert!(before > 0, "starts below the best match");
    for _ in 0..5 {
        opened(&control, &repo, "tests/search_test.rs").await.unwrap();
    }
    let after = paths_of(&files(&control, &repo, "search").await).iter().position(|p| *p == "tests/search_test.rs").unwrap();
    assert!(after < before, "opened: {before} → {after}");
    // files.find uses the same picker: it learned too.
    let find: FileFound =
        control.call(Method::FilesFind(FilesFindParams { block_id: block.block_id.clone(), query: "search".into(), limit: None })).await.unwrap();
    assert_eq!(find.paths.iter().position(|p| p == "tests/search_test.rs"), Some(after), "{:?}", find.paths);
    assert!(!find.truncated);
    // Frecency is stored in the server's home.
    assert!(paths::search(&home).join("frecency").is_dir());

    assert_eq!(code(&opened(&control, &repo, "nope.rs").await.unwrap_err()), "not_found");
    assert_eq!(code(&opened(&control, &repo, "../outside/secret.rs").await.unwrap_err()), RpcError::FORBIDDEN);
    assert_eq!(code(&opened(&control, &repo, "src").await.unwrap_err()), "bad_request");

    // limit caps the items, not the total.
    let one: SearchFiles =
        control.call(Method::SearchFiles(SearchFilesParams { root: repo.display().to_string(), query: "rs".into(), limit: Some(1) })).await.unwrap();
    assert_eq!(one.items.len(), 1);
    assert!(one.total > 1);
    stop(control, task).await;
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn grep_finds_lines_in_plain_and_regex_modes() {
    let (home, repo, _) = temp("grep");
    let task = start(&home, silo_server::search::Limits::default()).await;
    let control = connect(&home).await;
    workspace(&control, &repo).await;
    files(&control, &repo, "").await;

    let plain = grep(&control, &repo, "fn main", None).await.unwrap();
    assert_eq!(plain.items.len(), 1, "ignored files are not searched: {:?}", plain.items);
    let hit = &plain.items[0];
    assert_eq!((hit.path.as_str(), hit.line, hit.col, hit.text.as_str()), ("src/main.rs", 1, 1, "fn main() {"));
    assert_eq!(hit.ranges, [[0, 7]]);
    assert_eq!(plain.total, 1);
    assert!(!plain.indexing && !plain.truncated);

    let regex = grep(&control, &repo, r"pub fn \w+\(", Some(SearchGrepMode::Regex)).await.unwrap();
    let lib: Vec<(u64, &str)> = regex.items.iter().filter(|i| i.path == "src/lib.rs").map(|i| (i.line, i.text.as_str())).collect();
    assert_eq!(lib, [(2, "pub fn alpha() {}"), (3, "pub fn beta() {}"), (4, "pub fn gamma() {}")], "the modified file as on disk");
    assert!(regex.items.iter().any(|i| i.path == "src/search.rs" && i.line == 1));
    assert_eq!(code(&grep(&control, &repo, "fn (", Some(SearchGrepMode::Regex)).await.unwrap_err()), "bad_request");

    // Columns count characters; ranges are bytes of the text.
    let wide = grep(&control, &repo, "wörld", None).await.unwrap();
    let hit = &wide.items[0];
    assert_eq!((hit.path.as_str(), hit.line), ("docs/search.md", 3));
    assert_eq!(hit.col, 20);
    let [start, end] = hit.ranges[0];
    assert_eq!(&hit.text[start as usize..end as usize], "wörld");

    // Smart case; constraints narrow the files.
    assert!(grep(&control, &repo, "FN MAIN", None).await.unwrap().items.is_empty());
    let narrowed = grep(&control, &repo, "pub fn src/**/*.rs", None).await.unwrap();
    assert!(!narrowed.items.is_empty() && narrowed.items.iter().all(|i| i.path.starts_with("src/")), "{:?}", narrowed.items);
    assert!(grep(&control, &repo, "  ", None).await.unwrap().items.is_empty());
    stop(control, task).await;
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn roots_must_be_under_a_viewable_block_folder() {
    let (home, repo, outside) = temp("roots");
    let task = start(&home, silo_server::search::Limits::default()).await;
    let control = connect(&home).await;
    // No block works anywhere yet.
    assert_eq!(code(&files_raw(&control, &repo, "").await.unwrap_err()), RpcError::FORBIDDEN);
    workspace(&control, &repo).await;
    assert!(files(&control, &repo, "main").await.items.iter().any(|i| i.path == "src/main.rs"));
    // Under a block's folder is fine; paths are relative to the root asked.
    let sub = files(&control, &repo.join("src"), "main").await;
    assert_eq!(sub.items.first().map(|i| i.path.as_str()), Some("main.rs"));

    assert_eq!(code(&files_raw(&control, &outside, "").await.unwrap_err()), RpcError::FORBIDDEN);
    assert_eq!(code(&grep(&control, &outside, "main", None).await.unwrap_err()), RpcError::FORBIDDEN);
    assert_eq!(code(&opened(&control, &outside, "secret.rs").await.unwrap_err()), RpcError::FORBIDDEN);
    // A link inside the folder to one outside it is outside.
    std::os::unix::fs::symlink(&outside, repo.join("escape")).unwrap();
    assert_eq!(code(&files_raw(&control, &repo.join("escape"), "").await.unwrap_err()), RpcError::FORBIDDEN);
    assert_eq!(code(&files_raw(&control, &repo.join("src/../../outside"), "").await.unwrap_err()), RpcError::FORBIDDEN);
    let relative = Method::SearchFiles(SearchFilesParams { root: "repo".into(), query: String::new(), limit: None });
    assert_eq!(code(&control.call_value(relative).await.unwrap_err()), "bad_request");
    assert_eq!(code(&files_raw(&control, &repo.join("missing"), "").await.unwrap_err()), "not_found");
    assert_eq!(code(&files_raw(&control, &repo.join("README.md"), "").await.unwrap_err()), "bad_request");
    stop(control, task).await;
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}

/// `n` files under `dir`, so a scan takes a moment.
fn tree(dir: &Path, n: usize) {
    for i in 0..n {
        write(dir, &format!("d{}/f{i}.txt", i % 20), "x\n");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pickers_index_in_the_background_and_are_evicted() {
    let dir = temp_dir("cache");
    let home = dir.join("home");
    let roots: Vec<PathBuf> = (0..3).map(|i| dir.join(format!("r{i}"))).collect();
    for root in &roots {
        tree(root, 600);
    }
    let limits = silo_server::search::Limits { pickers: 2, idle: Duration::from_secs(600), first_wait: Duration::ZERO };
    let task = start(&home, limits).await;
    let control = connect(&home).await;
    workspace(&control, &dir).await;

    // A new picker answers at once, indexing; then complete.
    let first = files_raw(&control, &roots[0], "").await.unwrap();
    assert!(first.indexing, "the first search does not wait for the scan");
    let done = files(&control, &roots[0], "f1").await;
    assert!(done.total >= 1 && !done.indexing);
    assert_eq!(files(&control, &roots[0], "").await.total, 600);
    let grep_first = grep(&control, &roots[1], "x", None).await.unwrap();
    assert!(grep_first.indexing, "grep on a new picker says so too");
    files(&control, &roots[1], "").await;

    // Kept: no new scan.
    assert!(!files_raw(&control, &roots[0], "").await.unwrap().indexing);
    // A third root evicts the least recently used (roots[1]).
    files(&control, &roots[2], "").await;
    assert!(!files_raw(&control, &roots[0], "").await.unwrap().indexing, "recently used: kept");
    assert!(files_raw(&control, &roots[1], "").await.unwrap().indexing, "evicted: indexed afresh");
    stop(control, task).await;

    // Idle pickers go.
    let limits = silo_server::search::Limits { pickers: 8, idle: Duration::from_millis(400), first_wait: Duration::ZERO };
    let task = start(&home, limits).await;
    let control = connect(&home).await;
    workspace(&control, &dir).await;
    files(&control, &roots[0], "").await;
    assert!(!files_raw(&control, &roots[0], "").await.unwrap().indexing);
    tokio::time::sleep(Duration::from_millis(1000)).await;
    assert!(files_raw(&control, &roots[0], "").await.unwrap().indexing, "dropped after idling");
    stop(control, task).await;
    let _ = std::fs::remove_dir_all(&dir);
}
