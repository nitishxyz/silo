//! Files blocks: creation, reads and writes under the root, path safety,
//! view state that survives a restart and an upgrade, and folder watches.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use silo_client::{Control, Events};
use silo_proto::{
    control::{
        ContentKind, Created, Event, FileBase, FileFound, FileImage, FileKind, FileList, FileRead, FileWritten, FilesCreateParams, FilesFindParams,
        FilesListParams, FilesReadImageParams, FilesReadParams, FilesRootParams, FilesSetParams, FilesWriteParams, Method, RpcError, SpawnSpec,
    },
    model::{Axis, BlockInfo, BlockKind, FileHighlight, FilesState, TermSize},
    paths,
};
use tokio::task::JoinHandle;

const TIMEOUT: Duration = Duration::from_secs(15);

/// A fresh home for the server and a root holding the test tree.
fn temp(name: &str) -> (PathBuf, PathBuf) {
    let dir = PathBuf::from(format!("/tmp/silo-files-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let root = dir.join("root");
    let write = |rel: &str, bytes: &[u8]| {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    };
    write("a.txt", b"hello\n");
    write("src/main.rs", b"fn main() {}\n");
    write("src/model.rs", b"pub struct M;\n");
    write("bin.dat", b"\x00\x01\x02binary");
    write("img.png", b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR");
    write("e.txt", "h\u{e9}llo".as_bytes());
    write(".gitignore", b"ignored/\n*.log\n");
    write("ignored/x.rs", b"");
    write("debug.log", b"");
    write("target/t.rs", b"");
    write("node_modules/n/main.js", b"");
    write(".git/config", b"");
    write(".github/ci.yml", b"");
    write("../outside/secret.txt", b"secret");
    std::os::unix::fs::symlink(dir.join("outside"), root.join("escape")).unwrap();
    std::os::unix::fs::symlink(dir.join("outside/secret.txt"), root.join("secret.txt")).unwrap();
    std::os::unix::fs::symlink("src/main.rs", root.join("main-link.rs")).unwrap();
    (dir.join("home"), root.canonicalize().unwrap())
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

/// Start a server; ready once a new socket file accepts (see `persist.rs`).
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
        if let Ok(pair) = Control::connect(&paths::socket(home), "files-test").await {
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

/// A session whose terminal is split with a files block at `root`.
async fn files(control: &Control, root: &Path) -> Created {
    let spawn = SpawnSpec { command: vec!["/bin/sh".into()], cwd: Some("/tmp".into()), size: Some(TermSize::new(80, 24)) };
    let main: Created = control.call(Method::SessionCreate { name: None, spawn, kind: BlockKind::Terminal, url: None }).await.unwrap();
    let split = Method::BlockSplit {
        block_id: main.block_id,
        axis: Axis::Horizontal,
        spawn: SpawnSpec { cwd: Some(root.display().to_string()), ..SpawnSpec::default() },
        kind: BlockKind::Files,
        url: None,
        agent: None,
    };
    control.call(split).await.unwrap()
}

async fn info(control: &Control, created: &Created) -> BlockInfo {
    let view: silo_proto::model::SessionView = control.call(Method::SessionView { session_id: created.session_id.clone() }).await.unwrap();
    view.blocks.into_iter().find(|b| b.id == created.block_id).expect("files block")
}

fn code(error: &anyhow::Error) -> &str {
    &error.downcast_ref::<RpcError>().unwrap_or_else(|| panic!("not an rpc error: {error:#}")).code
}

async fn read(control: &Control, block: &str, path: &str, max_bytes: Option<u64>) -> anyhow::Result<FileRead> {
    control.call(Method::FilesRead(FilesReadParams { block_id: block.into(), path: path.into(), max_bytes })).await
}

async fn list(control: &Control, block: &str, path: &str) -> anyhow::Result<FileList> {
    control.call(Method::FilesList(FilesListParams { block_id: block.into(), path: path.into() })).await
}

async fn find(control: &Control, block: &str, query: &str) -> FileFound {
    control.call(Method::FilesFind(FilesFindParams { block_id: block.into(), query: query.into(), limit: Some(1000) })).await.unwrap()
}

fn set(block: &str) -> FilesSetParams {
    FilesSetParams { block_id: block.into(), ..FilesSetParams::default() }
}

#[tokio::test]
async fn split_lists_reads_and_finds() {
    let (home, root) = temp("reads");
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let created = files(&control, &root).await;
    let id = created.block_id.as_str();

    let block = info(&control, &created).await;
    let root_s = root.display().to_string();
    assert_eq!((block.kind, block.title.as_str(), block.cwd.as_deref(), block.pid), (BlockKind::Files, "root", Some(root_s.as_str()), None));
    assert_eq!(block.files.as_deref().unwrap().clone(), FilesState::new(root_s.clone()));

    // Directories first; `.git` never listed; links say where they point.
    let top = list(&control, id, "").await.unwrap();
    let names: Vec<&str> = top.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(&names[..5], [".github", "ignored", "node_modules", "src", "target"].as_slice(), "{names:?}");
    assert!(!names.contains(&".git"), "{names:?}");
    let entry = |name: &str| top.entries.iter().find(|e| e.name == name).unwrap().clone();
    assert_eq!((entry("a.txt").kind, entry("a.txt").size), (FileKind::File, 6));
    assert_eq!((entry("main-link.rs").kind, entry("main-link.rs").target), (FileKind::Symlink, Some(FileKind::File)));
    assert_eq!((entry("escape").kind, entry("escape").target), (FileKind::Symlink, None), "a link out of the root is not followed");
    let src = list(&control, id, "src/").await.unwrap();
    assert_eq!((src.path.as_str(), src.entries.len()), ("src", 2));
    assert_eq!(code(&list(&control, id, "a.txt").await.unwrap_err()), "bad_request");
    assert_eq!(code(&list(&control, id, "nope").await.unwrap_err()), "not_found");

    // Text, binary, truncation (never inside a character), caps.
    let text = read(&control, id, "a.txt", None).await.unwrap();
    assert_eq!(
        (text.kind, text.text.as_deref(), text.mime.as_str(), text.size, text.truncated),
        (ContentKind::Text, Some("hello\n"), "text/plain", 6, false)
    );
    assert!(text.mtime > 0);
    let binary = read(&control, id, "bin.dat", None).await.unwrap();
    assert_eq!((binary.kind, binary.text, binary.size), (ContentKind::Binary, None, 9));
    let png = read(&control, id, "img.png", None).await.unwrap();
    assert_eq!((png.kind, png.mime.as_str()), (ContentKind::Binary, "image/png"));
    let cut = read(&control, id, "e.txt", Some(2)).await.unwrap();
    assert_eq!((cut.kind, cut.text.as_deref(), cut.truncated, cut.size), (ContentKind::Text, Some("h"), true, 6));
    let cut = read(&control, id, "a.txt", Some(3)).await.unwrap();
    assert_eq!((cut.text.as_deref(), cut.truncated), (Some("hel"), true));
    std::fs::write(root.join("big.txt"), "x".repeat(3 * 1024 * 1024)).unwrap();
    let big = read(&control, id, "big.txt", None).await.unwrap();
    assert_eq!((big.text.unwrap().len(), big.truncated, big.size), (2 * 1024 * 1024, true, 3 * 1024 * 1024));
    let all = read(&control, id, "big.txt", Some(u64::MAX)).await.unwrap();
    assert_eq!((all.text.unwrap().len(), all.truncated), (3 * 1024 * 1024, false));
    assert_eq!(code(&read(&control, id, "src", None).await.unwrap_err()), "bad_request");
    let linked = read(&control, id, "main-link.rs", None).await.unwrap();
    assert_eq!(linked.text.as_deref(), Some("fn main() {}\n"), "links inside the root are followed");

    let image: FileImage = control.call(Method::FilesReadImage(FilesReadImageParams { block_id: id.into(), path: "img.png".into() })).await.unwrap();
    assert_eq!((image.mime.as_str(), image.size), ("image/png", 16));
    assert_eq!(data_encoding::BASE64.decode(image.data_base64.as_bytes()).unwrap(), std::fs::read(root.join("img.png")).unwrap());
    std::fs::write(root.join("logo.svg"), "<svg/>").unwrap();
    let svg: FileImage = control.call(Method::FilesReadImage(FilesReadImageParams { block_id: id.into(), path: "logo.svg".into() })).await.unwrap();
    assert_eq!(svg.mime, "image/svg+xml");
    let error = control.call_value(Method::FilesReadImage(FilesReadImageParams { block_id: id.into(), path: "a.txt".into() })).await.unwrap_err();
    assert_eq!(code(&error), "bad_request");

    // `.gitignore`d paths and `.git`, `target`, `node_modules` are skipped.
    let everything = find(&control, id, "").await;
    assert!(!everything.truncated);
    for path in ["a.txt", "src/main.rs", ".gitignore", ".github/ci.yml"] {
        assert!(everything.paths.iter().any(|p| p == path), "{path}: {:?}", everything.paths);
    }
    for path in ["ignored/x.rs", "debug.log", "target/t.rs", "node_modules/n/main.js", ".git/config", "secret.txt"] {
        assert!(!everything.paths.iter().any(|p| p == path), "{path}: {:?}", everything.paths);
    }
    let main = find(&control, id, "main").await;
    assert_eq!(main.paths.first().map(String::as_str), Some("src/main.rs"), "{:?}", main.paths);
    assert!(find(&control, id, "zzz").await.paths.is_empty());

    // Terminal methods are refused; other blocks have no files.
    assert_eq!(code(&control.call_value(Method::DataConnect { block_id: id.into() }).await.unwrap_err()), "bad_request");
    let view: silo_proto::model::SessionView = control.call(Method::SessionView { session_id: created.session_id.clone() }).await.unwrap();
    let terminal = view.blocks.iter().find(|b| b.kind == BlockKind::Terminal).unwrap().id.clone();
    assert_eq!(code(&list(&control, &terminal, "").await.unwrap_err()), "bad_request");

    // A new window opens in the focused block's folder (the files block's
    // root); a new session in the home directory.
    let window = Method::WindowCreate { session_id: created.session_id.clone(), spawn: SpawnSpec::default(), kind: BlockKind::Files, url: None };
    let window: Created = control.call(window).await.unwrap();
    assert_eq!(info(&control, &window).await.files.as_deref().unwrap().clone().root, root_s);
    let session = Method::SessionCreate { name: None, spawn: SpawnSpec::default(), kind: BlockKind::Files, url: None };
    let session: Created = control.call(session).await.unwrap();
    let home_dir = std::path::PathBuf::from(std::env::var("HOME").unwrap()).canonicalize().unwrap();
    assert_eq!(info(&control, &session).await.files.as_deref().unwrap().clone().root, home_dir.display().to_string());
    let missing = Method::SessionCreate {
        name: None,
        spawn: SpawnSpec { cwd: Some(root.join("nope").display().to_string()), ..SpawnSpec::default() },
        kind: BlockKind::Files,
        url: None,
    };
    assert_eq!(code(&control.call_value(missing).await.unwrap_err()), "not_found");
    stop(control, task).await;
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}

#[tokio::test]
async fn escapes_are_refused() {
    let (home, root) = temp("escape");
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let id = files(&control, &root).await.block_id;

    for path in ["../outside/secret.txt", "src/../../outside/secret.txt", "secret.txt", "escape/secret.txt"] {
        assert_eq!(code(&read(&control, &id, path, None).await.unwrap_err()), RpcError::FORBIDDEN, "{path}");
    }
    for path in ["..", "escape"] {
        assert_eq!(code(&list(&control, &id, path).await.unwrap_err()), RpcError::FORBIDDEN, "{path}");
    }
    let image = Method::FilesReadImage(FilesReadImageParams { block_id: id.clone(), path: "escape/secret.txt".into() });
    assert_eq!(code(&control.call_value(image).await.unwrap_err()), RpcError::FORBIDDEN);
    for path in ["/etc/passwd", "~/x"] {
        assert_eq!(code(&read(&control, &id, path, None).await.unwrap_err()), "bad_request", "{path}");
    }
    // `files.set` stores no path that leaves the root.
    let bad = FilesSetParams { open: Some(vec!["../outside/secret.txt".into()]), ..set(&id) };
    assert_eq!(code(&control.call_value(Method::FilesSet(bad)).await.unwrap_err()), RpcError::FORBIDDEN);
    let bad = FilesSetParams { expanded: Some(vec!["/etc".into()]), ..set(&id) };
    assert_eq!(code(&control.call_value(Method::FilesSet(bad)).await.unwrap_err()), "bad_request");

    stop(control, task).await;
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}

/// Whether a `files.changed` of `block` listing `path` arrives within `wait`.
async fn changed(events: &mut Events, block: &str, path: &str, wait: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + wait;
    while let Ok(event) = tokio::time::timeout_at(deadline, events.recv()).await {
        if let (_, Event::FilesChanged { block_id, paths, .. }) = event.expect("events")
            && block_id == block
            && paths.iter().any(|p| p == path)
        {
            return true;
        }
    }
    false
}

/// Write `rel` under `dir` until a `files.changed` of `block` lists `seen`
/// (watches start in the background).
async fn write_seen(events: &mut Events, block: &str, dir: &Path, rel: &str, seen: &str) {
    let started = tokio::time::Instant::now();
    // FSEvents streams can take seconds to start on a loaded machine.
    let deadline = started + 3 * TIMEOUT;
    for n in 0.. {
        std::fs::write(dir.join(rel), n.to_string()).unwrap();
        if changed(events, block, seen, Duration::from_millis(500)).await {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "no files.changed for {seen}");
    }
}

/// No `files.changed` of `block` within `wait`.
async fn quiet(events: &mut Events, block: &str, wait: Duration) {
    let deadline = tokio::time::Instant::now() + wait;
    while let Ok(event) = tokio::time::timeout_at(deadline, events.recv()).await {
        let (_, event) = event.expect("events");
        assert!(!matches!(&event, Event::FilesChanged { block_id, .. } if block_id == block), "{event:?}");
    }
}

#[tokio::test]
async fn changes_in_shown_folders_are_reported_until_close() {
    let (home, root) = temp("watch");
    let task = start(&home).await;
    let (control, mut events) = connect(&home).await;
    control.subscribe().await.unwrap();
    let id = files(&control, &root).await.block_id;

    // The root is watched from the start; `src` once expanded.
    write_seen(&mut events, &id, &root, "new.txt", "new.txt").await;
    std::fs::write(root.join("src/unseen.rs"), "").unwrap();
    assert!(!changed(&mut events, &id, "src/unseen.rs", Duration::from_secs(1)).await, "src is not expanded");
    let expanded = FilesSetParams { expanded: Some(vec!["src".into()]), ..set(&id) };
    let state: FilesState = control.call(Method::FilesSet(expanded)).await.unwrap();
    assert_eq!(state.expanded, ["src"]);
    write_seen(&mut events, &id, &root, "src/new.rs", "src/new.rs").await;
    std::fs::write(root.join("src/main.rs"), "fn main() { x() }").unwrap();
    assert!(changed(&mut events, &id, "src/main.rs", TIMEOUT).await);

    // Re-rooting watches the new root only.
    let state: FilesState = control.call(Method::FilesRoot(FilesRootParams { block_id: id.clone(), path: "src".into() })).await.unwrap();
    assert_eq!(state, FilesState::new(root.join("src").display().to_string()));
    write_seen(&mut events, &id, &root, "src/later.rs", "later.rs").await;
    let up = Method::FilesRoot(FilesRootParams { block_id: id.clone(), path: root.display().to_string() });
    control.call_value(up).await.unwrap();

    control.call_value(Method::BlockClose { block_id: id.clone(), force: false }).await.unwrap();
    std::fs::write(root.join("after.txt"), "1").unwrap();
    std::fs::write(root.join("src/after.rs"), "1").unwrap();
    quiet(&mut events, &id, Duration::from_secs(1)).await;
    assert_eq!(code(&list(&control, &id, "").await.unwrap_err()), "not_found");

    stop(control, task).await;
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}

/// A files block with tabs, scroll and expanded folders set.
async fn prepare(control: &Control, root: &Path) -> (Created, FilesState) {
    let created = files(control, root).await;
    let id = created.block_id.clone();
    let params = FilesSetParams {
        open: Some(vec!["a.txt".into(), "./src/main.rs".into(), "a.txt".into()]),
        active: Some("src/model.rs".into()),
        tree: Some(false),
        expanded: Some(vec!["src/".into(), "".into()]),
        scroll: Some([("src/main.rs".to_owned(), 40), ("gone.rs".to_owned(), 3)].into()),
        ..set(&id)
    };
    let state: FilesState = control.call(Method::FilesSet(params)).await.unwrap();
    assert_eq!(state.open, ["a.txt", "src/main.rs", "src/model.rs"], "deduplicated; the active tab opened");
    assert_eq!((state.active.as_deref(), state.tree, state.expanded.as_slice()), (Some("src/model.rs"), false, ["src".to_owned()].as_slice()));
    assert_eq!(state.scroll, [("src/main.rs".to_owned(), 40)].into(), "only open tabs keep a scroll");
    // Closing the active tab shows the last one; omitted fields stay.
    let marks = [("src/main.rs".to_owned(), vec![mark(1, 1, Some("entry"))])].into();
    let state: FilesState =
        control.call(Method::FilesSet(FilesSetParams { open: Some(vec!["src/main.rs".into()]), highlights: Some(marks), ..set(&id) })).await.unwrap();
    assert_eq!((state.active.as_deref(), state.tree, state.scroll.len()), (Some("src/main.rs"), false, 1));
    assert_eq!(state.highlights["src/main.rs"], [mark(1, 1, Some("entry"))], "highlights are saved with the block");
    assert_eq!(info(control, &created).await.files.as_deref().unwrap().clone(), state);
    (created, state)
}

fn mark(start: u64, end: u64, note: Option<&str>) -> FileHighlight {
    FileHighlight { start, end, note: note.map(str::to_owned) }
}

#[tokio::test]
async fn highlights_belong_to_their_tab() {
    let (home, root) = temp("highlights");
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let created = files(&control, &root).await;
    let id = created.block_id.clone();
    let call = |params: FilesSetParams| control.call::<FilesState>(Method::FilesSet(params));

    // Opened and marked in one call; reversed ranges turn round, notes are trimmed.
    let marks = [("src/main.rs".to_owned(), vec![mark(9, 3, Some("  here ")), mark(12, 12, Some(" "))])].into();
    let state = call(FilesSetParams { active: Some("src/main.rs".into()), highlights: Some(marks), ..set(&id) }).await.unwrap();
    assert_eq!(state.highlights["src/main.rs"], [mark(3, 9, Some("here")), mark(12, 12, None)]);
    // A path not open keeps none; line 0 and too many are refused.
    let marks = [("a.txt".to_owned(), vec![mark(1, 2, None)])].into();
    let state = call(FilesSetParams { highlights: Some(marks), ..set(&id) }).await.unwrap();
    assert!(!state.highlights.contains_key("a.txt"));
    let zero = [("src/main.rs".to_owned(), vec![mark(0, 2, None)])].into();
    assert_eq!(code(&call(FilesSetParams { highlights: Some(zero), ..set(&id) }).await.unwrap_err()), "bad_request");
    let many = [("src/main.rs".to_owned(), vec![mark(1, 1, None); FileHighlight::PER_FILE + 1])].into();
    assert_eq!(code(&call(FilesSetParams { highlights: Some(many), ..set(&id) }).await.unwrap_err()), "bad_request");
    // Other changes leave them; an empty list clears them.
    let state = call(FilesSetParams { tree: Some(false), ..set(&id) }).await.unwrap();
    assert_eq!(state.highlights["src/main.rs"].len(), 2);
    let state = call(FilesSetParams { highlights: Some([("src/main.rs".to_owned(), vec![])].into()), ..set(&id) }).await.unwrap();
    assert!(state.highlights.is_empty());
    // Closing the tab drops its highlights.
    let marks = [("src/main.rs".to_owned(), vec![mark(1, 1, None)])].into();
    call(FilesSetParams { highlights: Some(marks), ..set(&id) }).await.unwrap();
    let state = call(FilesSetParams { open: Some(vec![]), ..set(&id) }).await.unwrap();
    assert!(state.highlights.is_empty() && state.active.is_none());
    // Older peers: a state without the field reads as none, and none is not written.
    let old: FilesState = serde_json::from_value(serde_json::json!({"root": "/x"})).unwrap();
    assert!(old.highlights.is_empty());
    assert!(serde_json::to_value(&old).unwrap().get("highlights").is_none());
    stop(control, task).await;
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}

async fn write(control: &Control, block: &str, path: &str, text: &str, base: Option<FileBase>, force: bool) -> anyhow::Result<FileWritten> {
    control.call(Method::FilesWrite(FilesWriteParams { block_id: block.into(), path: path.into(), text: text.into(), base, force })).await
}

async fn create(control: &Control, block: &str, path: &str, text: &str) -> anyhow::Result<FileWritten> {
    control.call(Method::FilesCreate(FilesCreateParams { block_id: block.into(), path: path.into(), text: text.into() })).await
}

fn base_of(read: &FileRead) -> FileBase {
    FileBase { mtime: read.mtime, size: read.size, sha256: read.sha256.clone() }
}

fn written_base(w: &FileWritten) -> FileBase {
    FileBase { mtime: w.mtime, size: w.size, sha256: Some(w.sha256.clone()) }
}

fn mode(path: &Path) -> u32 {
    std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(path).unwrap().permissions()) & 0o7777
}

/// Names in `dir` that look like a write's temporary file.
fn temps(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir).unwrap().filter_map(|e| e.ok()?.file_name().into_string().ok()).filter(|n| n.contains(".silo-")).collect()
}

#[tokio::test]
async fn writes_are_atomic_and_refuse_conflicts() {
    let (home, root) = temp("write");
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let id = files(&control, &root).await.block_id;

    // A whole text read carries its hash; a cut one doesn't.
    let first = read(&control, &id, "a.txt", None).await.unwrap();
    assert_eq!(first.sha256.as_deref(), Some("5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03"), "sha256 of hello\\n");
    assert_eq!(read(&control, &id, "a.txt", Some(2)).await.unwrap().sha256, None);

    // Saved from the version read: the result is the next base.
    let saved = write(&control, &id, "a.txt", "hello, world\n", Some(base_of(&first)), false).await.unwrap();
    assert_eq!((saved.path.as_str(), saved.size), ("a.txt", 13));
    assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "hello, world\n");
    let again = read(&control, &id, "a.txt", None).await.unwrap();
    assert_eq!((again.mtime, again.size, again.sha256.as_deref()), (saved.mtime, saved.size, Some(saved.sha256.as_str())));
    let saved = write(&control, &id, "a.txt", "third\n", Some(written_base(&saved)), false).await.unwrap();

    // A stale base is a conflict, with the file as it is now; nothing is written.
    let error = write(&control, &id, "a.txt", "stale\n", Some(base_of(&first)), false).await.unwrap_err();
    let rpc = error.downcast_ref::<RpcError>().unwrap();
    assert_eq!(rpc.code, RpcError::CONFLICT, "{rpc:?}");
    assert_eq!(rpc.data.as_ref().unwrap()["size"], 6);
    assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "third\n");
    // Changed by someone else: refused, then forced.
    std::fs::write(root.join("a.txt"), "theirs\n").unwrap();
    let error = write(&control, &id, "a.txt", "mine\n", Some(written_base(&saved)), false).await.unwrap_err();
    assert_eq!(code(&error), RpcError::CONFLICT);
    let forced = write(&control, &id, "a.txt", "mine\n", Some(written_base(&saved)), true).await.unwrap();
    assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "mine\n");
    // Without a hash, the time and size decide; with one, the contents do
    // (a touched file is no conflict).
    let timed = FileBase { sha256: None, ..written_base(&forced) };
    let file = std::fs::File::options().write(true).open(root.join("a.txt")).unwrap();
    file.set_modified(std::time::SystemTime::now() + Duration::from_secs(5)).unwrap();
    drop(file);
    assert_eq!(code(&write(&control, &id, "a.txt", "x\n", Some(timed), false).await.unwrap_err()), RpcError::CONFLICT);
    write(&control, &id, "a.txt", "touched\n", Some(written_base(&forced)), false).await.unwrap();
    // No base: written whatever is there.
    write(&control, &id, "a.txt", "", None, false).await.unwrap();
    assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "");

    // Permissions are kept; the file is replaced (a new inode), not rewritten.
    let main = root.join("src/main.rs");
    std::fs::set_permissions(&main, std::os::unix::fs::PermissionsExt::from_mode(0o751)).unwrap();
    let inode = |p: &Path| std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(p).unwrap());
    let before = inode(&main);
    write(&control, &id, "src/main.rs", "fn main() { saved() }\n", None, false).await.unwrap();
    assert_eq!(mode(&main), 0o751);
    assert_ne!(inode(&main), before, "renamed over");
    // A link inside the root writes its target and stays a link.
    write(&control, &id, "main-link.rs", "fn main() { linked() }\n", None, false).await.unwrap();
    assert!(std::fs::symlink_metadata(root.join("main-link.rs")).unwrap().file_type().is_symlink());
    assert_eq!(std::fs::read_to_string(&main).unwrap(), "fn main() { linked() }\n");
    assert_eq!(mode(&main), 0o751);

    // A read-only file is refused, and says so.
    std::fs::set_permissions(root.join("e.txt"), std::os::unix::fs::PermissionsExt::from_mode(0o444)).unwrap();
    let error = write(&control, &id, "e.txt", "no", None, true).await.unwrap_err();
    let rpc = error.downcast_ref::<RpcError>().unwrap();
    assert_eq!((rpc.code.as_str(), rpc.message.as_str()), (RpcError::READ_ONLY, "e.txt is read-only"));
    assert_eq!(std::fs::read_to_string(root.join("e.txt")).unwrap(), "h\u{e9}llo");
    // So is a file in a folder that can't be written (the rename needs it).
    std::fs::create_dir(root.join("locked")).unwrap();
    std::fs::write(root.join("locked/f.txt"), "f").unwrap();
    std::fs::set_permissions(root.join("locked"), std::os::unix::fs::PermissionsExt::from_mode(0o555)).unwrap();
    let error = write(&control, &id, "locked/f.txt", "g", None, false).await.unwrap_err();
    assert_eq!(code(&error), RpcError::READ_ONLY, "{error:#}");
    assert_eq!(std::fs::read_to_string(root.join("locked/f.txt")).unwrap(), "f");
    std::fs::set_permissions(root.join("locked"), std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();

    // Escapes, folders, missing files and oversized texts are refused.
    for path in ["../outside/secret.txt", "src/../../outside/secret.txt", "secret.txt", "escape/secret.txt"] {
        assert_eq!(code(&write(&control, &id, path, "pwned", None, true).await.unwrap_err()), RpcError::FORBIDDEN, "{path}");
    }
    assert_eq!(std::fs::read_to_string(home.parent().unwrap().join("outside/secret.txt")).unwrap(), "secret");
    for path in ["/etc/passwd", "~/x", "src"] {
        assert_eq!(code(&write(&control, &id, path, "x", None, true).await.unwrap_err()), "bad_request", "{path}");
    }
    assert_eq!(code(&write(&control, &id, "nope.txt", "x", None, true).await.unwrap_err()), "not_found");
    let big = "x".repeat(usize::try_from(FileWritten::MAX_BYTES).unwrap() + 1);
    assert_eq!(code(&write(&control, &id, "a.txt", &big, None, true).await.unwrap_err()), "too_large");
    assert!(temps(&root).is_empty() && temps(&root.join("src")).is_empty(), "no temporary files left");

    // New files: made once, under the root only.
    let made = create(&control, &id, "src/new.rs", "// new\n").await.unwrap();
    assert_eq!((made.path.as_str(), made.size), ("src/new.rs", 7));
    assert_eq!(std::fs::read_to_string(root.join("src/new.rs")).unwrap(), "// new\n");
    assert_eq!(code(&create(&control, &id, "src/new.rs", "").await.unwrap_err()), RpcError::EXISTS);
    assert_eq!(code(&create(&control, &id, "main-link.rs", "").await.unwrap_err()), RpcError::EXISTS);
    assert_eq!(code(&create(&control, &id, "../outside/new.txt", "").await.unwrap_err()), RpcError::FORBIDDEN);
    assert_eq!(code(&create(&control, &id, "escape/new.txt", "").await.unwrap_err()), RpcError::FORBIDDEN);
    assert_eq!(code(&create(&control, &id, "missing/new.txt", "").await.unwrap_err()), "not_found");
    assert_eq!(code(&create(&control, &id, "", "").await.unwrap_err()), "bad_request");
    assert!(!home.parent().unwrap().join("outside/new.txt").exists());

    stop(control, task).await;
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}

#[tokio::test]
async fn state_survives_restart_and_watches_resume() {
    let (home, root) = temp("persist");
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let (created, before) = prepare(&control, &root).await;
    stop(control, task).await;

    let task = start(&home).await;
    let (control, mut events) = connect(&home).await;
    control.subscribe().await.unwrap();
    let block = info(&control, &created).await;
    assert_eq!((block.kind, block.files.as_deref()), (BlockKind::Files, Some(&before)));
    assert_eq!(read(&control, &created.block_id, "a.txt", None).await.unwrap().text.as_deref(), Some("hello\n"));
    write_seen(&mut events, &created.block_id, &root, "src/restarted.rs", "src/restarted.rs").await;
    stop(control, task).await;
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn state_survives_takeover() {
    let (home, root) = temp("takeover");
    let old = start(&home).await;
    let (control, _) = connect(&home).await;
    let (created, before) = prepare(&control, &root).await;

    let new = tokio::spawn(silo_server::takeover(config(&home), false));
    tokio::time::timeout(TIMEOUT, old).await.unwrap().unwrap().expect("old server commits and exits");
    drop(control);
    let (control, _) = connect(&home).await;
    assert_eq!(info(&control, &created).await.files.as_deref().unwrap().clone(), before);
    assert_eq!(list(&control, &created.block_id, "src").await.unwrap().entries.len(), 2);
    stop(control, new).await;
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}
