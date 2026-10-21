//! Mirror resume validation, gaps and ACKs against an in-process server.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use silo_client::{Applied, Control, DataStream, Events, Mirror};
use silo_proto::{
    control::{Created, Method, SpawnSpec},
    data::{ClientData, Resume, ServerData, StartMode, Welcome},
    model::TermSize,
    paths,
};

const TIMEOUT: Duration = Duration::from_secs(20);

fn temp_home(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/silo-test-resume-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

async fn start_server(home: &Path) {
    let config =
        silo_server::Config { home: home.to_owned(), idle_exit: None, relay: silo_server::RelayConfig::Disabled, mdns: false, ..Default::default() };
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

/// A server with one shell block and a quiet prompt.
async fn setup(name: &str) -> (Control, Events, String) {
    let home = temp_home(name);
    start_server(&home).await;
    let (control, events) = Control::connect(&paths::socket(&home), name).await.unwrap();
    let spawn = SpawnSpec { command: vec!["/bin/sh".into()], cwd: Some("/tmp".into()), size: Some(TermSize::new(80, 24)) };
    let created: Created = control
        .call(Method::SessionCreate { name: Some("main".into()), spawn, kind: silo_proto::model::BlockKind::Terminal, url: None })
        .await
        .unwrap();
    (control, events, created.block_id)
}

/// Apply (and ack) frames until `predicate` holds.
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

async fn attach(control: &Control, block: &str, mirror: &mut Mirror) -> DataStream {
    let stream = DataStream::open(control, block, mirror.resume()).await.unwrap();
    mirror.begin(&stream.welcome).unwrap();
    stream
}

/// A live, coherent mirror showing `marker`.
async fn live(control: &Control, block: &str, marker: &str) -> (Mirror, DataStream) {
    let mut mirror = Mirror::new();
    let mut stream = attach(control, block, &mut mirror).await;
    assert_eq!(stream.welcome.mode, StartMode::Snapshot);
    // Type only at a prompt: input sent earlier is echoed by the tty before
    // the shell prints its first prompt, which then precedes the output.
    pump_until(&mut stream, &mut mirror, |m| m.coherent() && !m.screen_text().is_empty()).await;
    stream.send(&ClientData::Input(format!("PS1='$ '; echo {marker}\n").into_bytes()));
    let line = format!("\n{marker}\n$");
    pump_until(&mut stream, &mut mirror, |m| m.coherent() && m.screen_text().contains(&line)).await;
    (mirror, stream)
}

#[tokio::test(flavor = "multi_thread")]
async fn matching_resume_replays_only_missing_output() {
    let (control, _events, block) = setup("match").await;
    let (mut a, mut stream_a) = live(&control, &block, "first").await;
    let (mut b, stream_b) = live(&control, &block, "second").await;
    drop(stream_b);
    let resume = b.resume().unwrap();
    assert_eq!(b.checkpointable(), Some(resume));

    stream_a.send(&ClientData::Input(b"echo while-away\n".to_vec()));
    pump_until(&mut stream_a, &mut a, |m| m.screen_text().contains("\nwhile-away\n$")).await;

    let mut stream_b = DataStream::open(&control, &block, Some(resume)).await.unwrap();
    assert_eq!(stream_b.welcome.mode, StartMode::Resume);
    assert_eq!((stream_b.welcome.epoch, stream_b.welcome.offset), (resume.epoch, resume.offset));
    b.begin(&stream_b.welcome).unwrap();
    let mut expected = resume.offset;
    tokio::time::timeout(TIMEOUT, async {
        while !b.screen_text().contains("\nwhile-away\n$") {
            let data = stream_b.rx.recv().await.expect("stream closed");
            match &data {
                ServerData::SnapshotHead(_) | ServerData::SnapshotHistory(_) | ServerData::SnapshotDone => panic!("snapshot on resume"),
                ServerData::Output { offset, bytes } => {
                    assert_eq!(*offset, expected, "only the missing output follows");
                    expected += bytes.len() as u64;
                }
                _ => {}
            }
            b.apply(data).unwrap();
        }
    })
    .await
    .unwrap();
    assert!(b.coherent());
    assert_eq!(a.screen_text(), b.screen_text());
}

#[tokio::test(flavor = "multi_thread")]
async fn mismatched_resume_takes_snapshot() {
    let (control, _events, block) = setup("mismatch").await;
    let (mut a, stream_a) = live(&control, &block, "hello").await;
    let resume = a.resume().unwrap();

    // The server does not resume a foreign epoch.
    let wrong = Resume { epoch: resume.epoch.wrapping_add(1), offset: resume.offset };
    let stream = DataStream::open(&control, &block, Some(wrong)).await.unwrap();
    assert_ne!(stream.welcome.mode, StartMode::Resume);
    drop(stream);

    // The mirror refuses a resume that does not match it exactly.
    let size = a.size();
    let welcome = |epoch, offset| Welcome { epoch, mode: StartMode::Resume, offset, size };
    let mut b = Mirror::new();
    assert!(b.begin(&welcome(resume.epoch, resume.offset)).is_err(), "no local state");
    assert!(a.begin(&welcome(resume.epoch, resume.offset + 1)).is_err(), "offset mismatch");
    assert_eq!(a.resume(), None);
    let (mut a, _stream) = live(&control, &block, "again").await;
    assert!(a.begin(&welcome(resume.epoch + 1, a.resume().unwrap().offset)).is_err(), "epoch mismatch");
    assert!(!a.coherent());

    // Reconnecting after a failed resume goes through a snapshot.
    let mut stream = attach(&control, &block, &mut a).await;
    assert_eq!(stream.welcome.mode, StartMode::Snapshot);
    pump_until(&mut stream, &mut a, |m| m.coherent() && m.screen_text().contains("\nagain\n$")).await;
    drop(stream_a);
}

#[tokio::test(flavor = "multi_thread")]
async fn gap_forces_snapshot() {
    let (control, _events, block) = setup("gap").await;
    let (mut a, stream_a) = live(&control, &block, "before-gap").await;
    let next = a.resume().unwrap().offset;
    let text = a.screen_text();

    assert!(a.apply(ServerData::Output { offset: next + 5, bytes: b"lost".to_vec() }).is_err());
    assert!(!a.coherent());
    assert_eq!(a.resume(), None, "no resume after a gap");
    assert_eq!(a.take_ack(true), None, "no ack after a gap");
    assert!(a.apply(ServerData::Output { offset: next, bytes: b"x".to_vec() }).is_err(), "stays invalid");
    assert!(a.apply(ServerData::Output { offset: u64::MAX, bytes: b"xy".to_vec() }).is_err());
    drop(stream_a);

    let mut stream = attach(&control, &block, &mut a).await;
    assert_eq!(stream.welcome.mode, StartMode::Snapshot);
    pump_until(&mut stream, &mut a, |m| m.coherent()).await;
    assert_eq!(a.screen_text(), text);
    stream.send(&ClientData::Input(b"echo after-gap\n".to_vec()));
    pump_until(&mut stream, &mut a, |m| m.screen_text().contains("\nafter-gap\n$")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn acking_client_keeps_up_with_a_flood() {
    let (control, _events, block) = setup("ack").await;
    let (mut a, mut stream) = live(&control, &block, "ready").await;
    let start = a.resume().unwrap().offset;

    // Throttling: an idle ack goes out once per applied offset. The pump may
    // have stopped before its last frame was acked (it was not idle then).
    if let Some(offset) = a.take_ack(true) {
        assert_eq!(offset, start);
        assert!(stream.send(&ClientData::Ack(offset)));
    }
    assert_eq!(a.take_ack(true), None, "same offset acked twice");
    stream.send(&ClientData::Input(b"i=0; while [ $i -lt 40000 ]; do i=$((i+1)); echo flood-line-$i; done; echo flood-done\n".to_vec()));
    let mut acks = 0u32;
    tokio::time::timeout(Duration::from_secs(60), async {
        while !a.screen_text().contains("\nflood-done\n$") {
            let data = stream.rx.recv().await.expect("acking client was cut off");
            a.apply(data).unwrap();
            if let Some(offset) = a.take_ack(stream.rx.is_empty()) {
                assert!(stream.send(&ClientData::Ack(offset)));
                acks += 1;
            }
        }
    })
    .await
    .unwrap();
    let end = a.resume().unwrap().offset;
    assert!(end - start > 500_000, "flood produced {} bytes", end - start);
    assert!(acks > 0);
    assert_eq!(a.take_ack(false), None);

    // Still attached and live.
    stream.send(&ClientData::Input(b"echo still-here\n".to_vec()));
    pump_until(&mut stream, &mut a, |m| m.screen_text().contains("\nstill-here\n$")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn exit_reports_code() {
    let (control, _events, block) = setup("exit").await;
    let (mut a, mut stream) = live(&control, &block, "bye").await;
    stream.send(&ClientData::Input(b"exit 7\n".to_vec()));
    let code = tokio::time::timeout(TIMEOUT, async {
        loop {
            let data = stream.rx.recv().await.expect("stream closed before exit");
            if let Applied::Exited(code) = a.apply(data).unwrap() {
                return code;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(code, Some(7));
    assert_eq!(a.exited(), Some(Some(7)));
}
