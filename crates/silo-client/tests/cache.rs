//! Encrypted per-server client cache: tree, block checkpoints, offline
//! restore and resume against an in-process server.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use silo_client::{
    Control, DataStream, Mirror,
    cache::{Cache, Cached, MirrorCheckpoint, server_dir},
};
use silo_proto::{
    control::{Created, Method, SpawnSpec},
    data::{ClientData, Resume, ServerData, StartMode},
    model::TermSize,
    paths,
};

const TIMEOUT: Duration = Duration::from_secs(20);

fn temp_dir(kind: &str, name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/silo-test-cache-{kind}-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Tree {
    sessions: Vec<String>,
    focused: Option<u32>,
}

fn tree() -> Tree {
    Tree { sessions: vec!["main".into(), "logs".into()], focused: Some(1) }
}

async fn start_server(home: &Path) {
    let config = silo_server::Config { home: home.to_owned(), relay: silo_server::RelayConfig::Disabled, mdns: false, ..Default::default() };
    tokio::spawn(async move {
        silo_server::run(config).await.unwrap();
    });
    for _ in 0..200 {
        if silo_client::server_running(home).await {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("server did not start");
}

async fn setup(name: &str) -> (Control, String) {
    let home = temp_dir("home", name);
    start_server(&home).await;
    let (control, _events) = Control::connect(&paths::socket(&home), name).await.unwrap();
    let spawn = SpawnSpec { command: vec!["/bin/sh".into()], cwd: Some("/tmp".into()), size: Some(TermSize::new(80, 24)) };
    let created: Created = control
        .call(Method::SessionCreate { name: Some("main".into()), spawn, kind: silo_proto::model::BlockKind::Terminal, url: None })
        .await
        .unwrap();
    (control, created.block_id)
}

async fn pump_until(stream: &mut DataStream, mirror: &mut Mirror, mut predicate: impl FnMut(&mut Mirror) -> bool) {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            if predicate(mirror) {
                return;
            }
            let data = stream.rx.recv().await.expect("stream closed");
            mirror.apply(data).unwrap();
            stream.ack(mirror, stream.rx.is_empty());
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out; screen:\n{}", mirror.screen_text()));
}

/// A live, coherent mirror with some history and `marker` on screen.
async fn live(control: &Control, block: &str, marker: &str) -> (Mirror, DataStream) {
    let mut mirror = Mirror::new();
    let mut stream = DataStream::open(control, block, None).await.unwrap();
    mirror.begin(&stream.welcome).unwrap();
    let cmd = format!("PS1='$ '; i=0; while [ $i -lt 60 ]; do i=$((i+1)); echo history-$i; done; echo {marker}\n");
    stream.send(&ClientData::Input(cmd.into_bytes()));
    let line = format!("\n{marker}\n$");
    pump_until(&mut stream, &mut mirror, |m| m.coherent() && m.screen_text().contains(&line)).await;
    (mirror, stream)
}

/// Scrollback size and the top of history, leaving the view following output.
fn history_text(mirror: &mut Mirror) -> (u64, String) {
    let total = mirror.terminal().unwrap().scroll_state().unwrap().total;
    mirror.terminal().unwrap().scroll_to_row(0);
    let top = mirror.screen_text();
    mirror.terminal().unwrap().scroll_to_row(u64::MAX);
    (total, top)
}

#[test]
fn tree_round_trip() {
    let root = temp_dir("root", "tree");
    let mut cache = Cache::open(&root, "server-a").unwrap();
    assert!(cache.load::<Tree>().unwrap().tree.is_none());
    cache.save_tree(&tree()).unwrap();
    let reopened = Cache::open(&root, "server-a").unwrap();
    let cached: Cached<Tree> = reopened.load().unwrap();
    assert_eq!(cached.tree, Some(tree()));
    assert!(cached.blocks.is_empty() && cached.errors.is_empty());
}

#[test]
fn identities_are_isolated() {
    let root = temp_dir("root", "isolated");
    let mut a = Cache::open(&root, "/tmp/a.sock").unwrap();
    let b = Cache::open(&root, "/tmp/b.sock").unwrap();
    assert_ne!(a.dir(), b.dir());
    assert_eq!(a.dir(), server_dir(&root, "/tmp/a.sock"));
    a.save_tree(&tree()).unwrap();
    assert!(b.load::<Tree>().unwrap().tree.is_none());
    assert_eq!(Cache::open(&root, "/tmp/a.sock").unwrap().load::<Tree>().unwrap().tree, Some(tree()));
}

#[test]
fn non_coherent_mirror_is_not_saved() {
    let root = temp_dir("root", "incoherent");
    let mut cache = Cache::open(&root, "server").unwrap();
    assert!(!cache.save_block("b1", &mut Mirror::new()).unwrap());
    let cached = cache.load::<Tree>().unwrap();
    assert!(cached.blocks.is_empty() && cached.errors.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn checkpoint_restores_offline_and_resumes() {
    let (control, block) = setup("resume").await;
    let (mut a, mut stream_a) = live(&control, &block, "cached").await;
    let root = temp_dir("root", "resume");
    let mut cache = Cache::open(&root, "server").unwrap();
    cache.save_tree(&tree()).unwrap();
    assert!(cache.save_block(&block, &mut a).unwrap());

    // Offline: identical screen and history.
    let cached: Cached<Tree> = Cache::open(&root, "server").unwrap().load().unwrap();
    assert_eq!(cached.tree, Some(tree()));
    assert!(cached.errors.is_empty());
    let [entry] = cached.blocks.as_slice() else { panic!("one block expected") };
    assert_eq!(entry.id, block);
    assert_eq!(Some(entry.checkpoint.resume), a.resume());
    let mut b = entry.checkpoint.restore().unwrap();
    assert!(b.coherent());
    assert_eq!(b.screen_text(), a.screen_text());
    assert_eq!(b.size(), a.size());
    assert_eq!(b.config(), a.config());
    let history = history_text(&mut a);
    assert!(history.0 > 24 && history.1.contains("history-1\n"), "{history:?}");
    assert_eq!(history_text(&mut b), history);

    // More output while "offline", then a resume with only the missing bytes.
    stream_a.send(&ClientData::Input(b"echo while-away\n".to_vec()));
    pump_until(&mut stream_a, &mut a, |m| m.screen_text().contains("\nwhile-away\n$")).await;
    let resume = b.resume().unwrap();
    let mut stream_b = DataStream::open(&control, &block, Some(resume)).await.unwrap();
    assert_eq!(stream_b.welcome.mode, StartMode::Resume);
    b.begin(&stream_b.welcome).unwrap();
    tokio::time::timeout(TIMEOUT, async {
        while !b.screen_text().contains("\nwhile-away\n$") {
            let data = stream_b.rx.recv().await.expect("stream closed");
            assert!(!matches!(data, ServerData::SnapshotHead(_) | ServerData::SnapshotHistory(_) | ServerData::SnapshotDone), "snapshot on resume");
            b.apply(data).unwrap();
        }
    })
    .await
    .unwrap();
    assert_eq!(a.screen_text(), b.screen_text());

    // A checkpoint whose epoch no longer matches gets a snapshot.
    let mut stale = entry.checkpoint.clone();
    stale.resume = Resume { epoch: stale.resume.epoch.wrapping_add(1), offset: stale.resume.offset };
    let mut c = stale.restore().unwrap();
    let mut stream_c = DataStream::open(&control, &block, c.resume()).await.unwrap();
    assert_eq!(stream_c.welcome.mode, StartMode::Snapshot);
    c.begin(&stream_c.welcome).unwrap();
    pump_until(&mut stream_c, &mut c, |m| m.coherent() && m.screen_text().contains("\nwhile-away\n$")).await;
    assert_eq!(c.screen_text(), a.screen_text());
}

#[tokio::test(flavor = "multi_thread")]
async fn captured_checkpoint_saves_on_a_worker() {
    let (control, block) = setup("worker").await;
    let (mut a, _stream) = live(&control, &block, "captured").await;
    let checkpoint = MirrorCheckpoint::capture(&mut a).unwrap().expect("coherent");
    assert!(MirrorCheckpoint::capture(&mut Mirror::new()).unwrap().is_none());

    let root = temp_dir("root", "worker");
    let worker_root = root.clone();
    let (id, saved) = (block.clone(), checkpoint.clone());
    std::thread::spawn(move || {
        let mut cache = Cache::open(&worker_root, "server").unwrap();
        cache.save_checkpoint(&id, &saved).unwrap();
        // A second save replaces the first generation.
        cache.save_checkpoint(&id, &saved).unwrap();
    })
    .join()
    .unwrap();

    let cached = Cache::open(&root, "server").unwrap().load::<Tree>().unwrap();
    assert!(cached.errors.is_empty());
    let [entry] = cached.blocks.as_slice() else { panic!("one block expected") };
    assert_eq!(entry.id, block);
    assert_eq!(entry.checkpoint, checkpoint);
    assert_eq!(entry.checkpoint.restore().unwrap().screen_text(), a.screen_text());
}

fn snap_files(dir: &Path) -> usize {
    std::fs::read_dir(dir).map_or(0, |entries| entries.filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().ends_with(".snap")).count())
}

#[tokio::test(flavor = "multi_thread")]
async fn repeated_saves_keep_disk_bounded() {
    let (control, block) = setup("bounded").await;
    let (mut a, _stream) = live(&control, &block, "bounded").await;
    let root = temp_dir("root", "bounded");
    let mut cache = Cache::open(&root, "server").unwrap();
    let blocks = cache.dir().join("snapshots").join(&block);
    for _ in 0..10 {
        assert!(cache.save_block(&block, &mut a).unwrap());
        assert!(snap_files(&blocks) <= 2, "{} snapshot files", snap_files(&blocks));
    }
    // A commit that does not rewrite the block drops the previous generation.
    cache.save_tree(&tree()).unwrap();
    assert_eq!(snap_files(&blocks), 1);
    cache.forget_block(&block).unwrap();
    cache.save_tree(&tree()).unwrap();
    assert_eq!(snap_files(&blocks), 0);
    let cached = cache.load::<Tree>().unwrap();
    assert!(cached.blocks.is_empty() && cached.errors.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn corrupt_block_is_reported_per_block() {
    let (control, block) = setup("corrupt").await;
    let (mut a, _stream) = live(&control, &block, "one").await;
    let root = temp_dir("root", "corrupt");
    let mut cache = Cache::open(&root, "server").unwrap();
    assert!(cache.save_block("good", &mut a).unwrap());
    assert!(cache.save_block("bad", &mut a).unwrap());
    assert!(cache.save_block("gone", &mut a).unwrap());
    cache.forget_block("gone").unwrap();

    let bad = cache.dir().join("snapshots/bad/1.snap");
    let mut bytes = std::fs::read(&bad).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    std::fs::write(&bad, bytes).unwrap();

    let cached = cache.load::<Tree>().unwrap();
    let ids: Vec<&str> = cached.blocks.iter().map(|b| b.id.as_str()).collect();
    assert_eq!(ids, ["good"]);
    let errors: Vec<&str> = cached.errors.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(errors, ["bad"]);
    let mut restored = cached.blocks[0].checkpoint.restore().unwrap();
    assert_eq!(restored.screen_text(), a.screen_text());

    // gc drops checkpoints of blocks that no longer exist.
    cache.gc(["good"]).unwrap();
    let cached = cache.load::<Tree>().unwrap();
    assert_eq!(cached.blocks.len(), 1);
    assert!(cached.errors.is_empty());
}
