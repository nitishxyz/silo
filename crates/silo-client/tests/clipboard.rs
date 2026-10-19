//! Clipboard bridge and uploads against an in-process server.

use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use silo_client::{Control, DataStream, Upload, UploadSource, upload};
use silo_proto::{
    BUILD, Frame,
    control::{Capture, Created, DataTicket, Method, Pasted, SpawnSpec},
    data::{ClientData, Clip, ClipKind, ClipboardLocation, Reject, ServerData},
    kind,
    model::BlockKind,
    paths, read_frame,
    upload::{MAX_UPLOAD, UploadHello},
    write_frame,
};
use tokio::io::AsyncWriteExt;

const TIMEOUT: Duration = Duration::from_secs(10);

fn temp_home(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/silo-test-clip-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

async fn start_server(home: &Path) -> Control {
    let config =
        silo_server::Config { home: home.to_owned(), idle_exit: None, relay: silo_server::RelayConfig::Disabled, mdns: false, ..Default::default() };
    tokio::spawn(async move {
        silo_server::run(config).await.unwrap();
    });
    for _ in 0..500 {
        if silo_client::server_running(home).await {
            return Control::connect(&paths::socket(home), "test").await.unwrap().0;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("server did not start");
}

async fn session(control: &Control, script: &str) -> String {
    let spawn = SpawnSpec { command: vec!["/bin/sh".into(), "-c".into(), script.into()], ..SpawnSpec::default() };
    let created: Created = control.call(Method::SessionCreate { name: None, spawn, kind: BlockKind::Terminal, url: None }).await.unwrap();
    created.block_id
}

async fn next_clip(stream: &mut DataStream) -> ServerData {
    tokio::time::timeout(TIMEOUT, async {
        loop {
            let data = stream.rx.recv().await.expect("stream closed");
            if matches!(data, ServerData::ClipboardWrite { .. } | ServerData::ClipboardRead { .. }) {
                return data;
            }
        }
    })
    .await
    .expect("no clipboard frame")
}

#[tokio::test]
async fn osc52_reaches_attached_clients_and_shells_know_their_block() {
    let home = temp_home("osc52");
    let control = start_server(&home).await;
    let block = session(&control, r#"printf 'block=%s\n' "$SILO_BLOCK"; read _; printf '\033]52;c;aGVsbG8=\007'; sleep 30"#).await;
    let mut stream = DataStream::open(&control, &block, None).await.unwrap();
    stream.send(&ClientData::Input(b"\r".to_vec()));
    assert_eq!(next_clip(&mut stream).await, ServerData::ClipboardWrite { location: ClipboardLocation::Standard, text: "hello".into() });
    let capture: Capture = control.call(Method::TerminalCapture { block_id: block.clone(), history: false }).await.unwrap();
    assert!(capture.text.contains(&format!("block={block}")), "{}", capture.text);
    let shim = std::fs::read_to_string(paths::shims(&home).join("pbpaste")).unwrap();
    assert!(shim.contains("clipboard shim pbpaste"), "{shim}");
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn clipboard_bridge_round_trips_and_times_out() {
    let home = temp_home("bridge");
    let control = start_server(&home).await;
    let block = session(&control, "sleep 60").await;
    let read = |block: &str, kind| Method::ClipboardRead { block_id: block.to_owned(), kind };

    // Nobody attached: no content, without waiting.
    let started = Instant::now();
    let pasted: Pasted = control.call(read(&block, ClipKind::Text)).await.unwrap();
    assert_eq!(pasted.content, None);
    assert!(started.elapsed() < Duration::from_secs(2));
    let write = Method::ClipboardWrite { block_id: block.clone(), location: ClipboardLocation::Primary, text: "x".into() };
    assert!(control.call_value(write.clone()).await.is_err(), "no client to write to");

    // A client answering with content while another stays silent.
    let mut answering = DataStream::open(&control, &block, None).await.unwrap();
    let silent = DataStream::open(&control, &block, None).await.unwrap();
    let image = Clip::Image { mime: "image/png".into(), data: vec![0x89, b'P', b'N', b'G', 0, 1, 2] };
    let reply = image.clone();
    let caller = control.clone();
    let method = read(&block, ClipKind::Image);
    let pending = tokio::spawn(async move { caller.call::<Pasted>(method).await.unwrap() });
    let ServerData::ClipboardRead { request, kind: ClipKind::Image } = next_clip(&mut answering).await else { panic!("expected a read") };
    answering.send(&ClientData::ClipboardContent { request, content: Some(reply) });
    assert_eq!(pending.await.unwrap().content, Some(image));

    control.call_value(write).await.unwrap();
    assert_eq!(next_clip(&mut answering).await, ServerData::ClipboardWrite { location: ClipboardLocation::Standard, text: "x".into() });

    // Attached clients that never answer: nothing after the timeout.
    drop(answering);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let started = Instant::now();
    let pasted: Pasted = control.call(Method::ClipboardRead { block_id: block.clone(), kind: ClipKind::Text }).await.unwrap();
    assert_eq!(pasted.content, None);
    assert!(started.elapsed() >= Duration::from_millis(2900), "{:?}", started.elapsed());
    drop(silent);
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn clipboard_write_floods_coalesce_and_oversized_answers_are_dropped() {
    let home = temp_home("flood");
    let control = start_server(&home).await;
    let script = r"read _; for t in YQ== Yg== Yw== ZA== ZQ==; do printf '\033]52;p;%s\007' $t; sleep 0.02; done; sleep 30";
    let block = session(&control, script).await;
    let mut stream = DataStream::open(&control, &block, None).await.unwrap();
    stream.send(&ClientData::Input(b"\r".to_vec()));
    let write = |text: &str| ServerData::ClipboardWrite { location: ClipboardLocation::Standard, text: text.into() };
    assert_eq!(next_clip(&mut stream).await, write("a"));
    let mut last = (Instant::now(), write("a"));
    let mut sent = 1;
    while let Ok(next) = tokio::time::timeout(Duration::from_millis(700), next_clip(&mut stream)).await {
        assert!(last.0.elapsed() >= Duration::from_millis(200), "{:?}", last.0.elapsed());
        last = (Instant::now(), next);
        sent += 1;
    }
    assert_eq!(last.1, write("e"), "the last write wins");
    assert!(sent < 5, "writes coalesce: {sent} sent");

    let caller = control.clone();
    let method = Method::ClipboardRead { block_id: block.clone(), kind: ClipKind::Text };
    let pending = tokio::spawn(async move { caller.call::<Pasted>(method).await.unwrap() });
    let ServerData::ClipboardRead { request, .. } = next_clip(&mut stream).await else { panic!("expected a read") };
    let huge = Clip::Text("x".repeat(silo_proto::data::MAX_CLIP_BYTES + 1));
    stream.send(&ClientData::ClipboardContent { request, content: Some(huge) });
    let started = Instant::now();
    assert_eq!(pending.await.unwrap().content, None);
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn clipboard_reads_prefer_content_over_empty_answers() {
    let home = temp_home("prefer");
    let control = start_server(&home).await;
    let block = session(&control, "sleep 60").await;
    let mut empty = DataStream::open(&control, &block, None).await.unwrap();
    let mut full = DataStream::open(&control, &block, None).await.unwrap();
    let read = |control: &Control| {
        let (control, block) = (control.clone(), block.clone());
        tokio::spawn(async move { control.call::<Pasted>(Method::ClipboardRead { block_id: block, kind: ClipKind::Text }).await.unwrap() })
    };
    let request = async |stream: &mut DataStream| match next_clip(stream).await {
        ServerData::ClipboardRead { request, .. } => request,
        other => panic!("expected a read, got {other:?}"),
    };

    // An empty answer first, content later: the content wins.
    let pending = read(&control);
    let (a, b) = (request(&mut empty).await, request(&mut full).await);
    empty.send(&ClientData::ClipboardContent { request: a, content: None });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!pending.is_finished(), "an empty answer must not win while another client owes one");
    full.send(&ClientData::ClipboardContent { request: b, content: Some(Clip::Text("yes".into())) });
    assert_eq!(pending.await.unwrap().content, Some(Clip::Text("yes".into())));

    // Every client answers empty: `None` right away, not after the timeout.
    let started = Instant::now();
    let pending = read(&control);
    let (a, b) = (request(&mut empty).await, request(&mut full).await);
    empty.send(&ClientData::ClipboardContent { request: a, content: None });
    full.send(&ClientData::ClipboardContent { request: b, content: None });
    assert_eq!(pending.await.unwrap().content, None);
    assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());

    // A client that leaves instead of answering counts as empty.
    let started = Instant::now();
    let pending = read(&control);
    let a = request(&mut empty).await;
    request(&mut full).await;
    empty.send(&ClientData::ClipboardContent { request: a, content: None });
    drop(full);
    assert_eq!(pending.await.unwrap().content, None);
    assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
    drop(empty);
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn uploads_files_directories_and_bytes() {
    let home = temp_home("upload");
    let control = start_server(&home).await;
    let block = session(&control, "sleep 60").await;
    let src = home.join("src");
    std::fs::create_dir_all(src.join("tree/sub")).unwrap();
    let big: Vec<u8> = (0..600_000u32).map(|i| i as u8).collect();
    std::fs::write(src.join("big.bin"), &big).unwrap();
    std::fs::write(src.join("tree/a.txt"), b"a").unwrap();
    std::fs::write(src.join("tree/sub/b.txt"), b"bb").unwrap();
    std::fs::write(src.join("tree/empty"), b"").unwrap();

    let files = vec![
        Upload { name: "big.bin".into(), source: UploadSource::Path(src.join("big.bin")) },
        Upload { name: "tree".into(), source: UploadSource::Path(src.join("tree")) },
        Upload { name: "../evil\nname".into(), source: UploadSource::Bytes(b"clip".to_vec()) },
        Upload { name: "big.bin".into(), source: UploadSource::Bytes(Vec::new()) },
    ];
    let mut seen = Vec::new();
    let paths = upload(&control, &block, files, |sent, total| seen.push((sent, total))).await.unwrap();
    let total = big.len() as u64 + 1 + 2 + 4;
    assert_eq!(seen.first(), Some(&(0, total)));
    assert_eq!(seen.last(), Some(&(total, total)));
    assert!(seen.windows(2).all(|w| w[0].0 <= w[1].0));

    assert_eq!(paths.len(), 4);
    let paths: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
    let drop_dir = paths[0].parent().unwrap().to_owned();
    assert!(paths.iter().all(|p| p.is_absolute() && p.parent() == Some(drop_dir.as_path())));
    assert_eq!(drop_dir.parent().unwrap(), paths::drops(&home).canonicalize().unwrap());
    let names: Vec<_> = paths.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
    assert_eq!(names, ["big.bin", "tree", ".._evil_name", "big.bin (2)"]);
    assert_eq!(std::fs::read(&paths[0]).unwrap(), big);
    assert_eq!(std::fs::read(paths[1].join("a.txt")).unwrap(), b"a");
    assert_eq!(std::fs::read(paths[1].join("sub/b.txt")).unwrap(), b"bb");
    assert_eq!(std::fs::read(paths[1].join("empty")).unwrap(), b"");
    assert_eq!(std::fs::read(&paths[2]).unwrap(), b"clip");
    assert_eq!(std::fs::read(&paths[3]).unwrap(), b"");
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn uploads_over_the_limit_are_refused() {
    let home = temp_home("limit");
    let control = start_server(&home).await;
    let block = session(&control, "sleep 60").await;
    std::fs::create_dir_all(&home).unwrap();
    let sparse = home.join("huge");
    std::fs::File::create(&sparse).unwrap().set_len(MAX_UPLOAD + 1).unwrap();
    let files = vec![Upload { name: "huge".into(), source: UploadSource::Path(sparse) }];
    let error = upload(&control, &block, files, |_, _| {}).await.unwrap_err();
    assert_eq!(format!("{error:#}"), "Too large to upload (2 GB, max 2 GB)");

    // The server enforces it too.
    let ticket: DataTicket = control.call(Method::DataConnect { block_id: block.clone() }).await.unwrap();
    let mut stream = control.conn().open_stream().await.unwrap();
    let hello = UploadHello { build: BUILD.to_owned(), ticket: ticket.ticket, total: MAX_UPLOAD + 1 };
    write_frame(&mut stream, &Frame::json(kind::UPLOAD_HELLO, &hello).unwrap()).await.unwrap();
    stream.flush().await.unwrap();
    let reply = tokio::time::timeout(TIMEOUT, read_frame(&mut stream)).await.unwrap().unwrap().unwrap();
    assert_eq!(reply.kind, kind::REJECT);
    let reject = reply.parse::<Reject>().unwrap();
    assert_eq!((reject.code.as_str(), reject.reason.as_str()), ("too_large", "Too large to upload (2 GB, max 2 GB)"));
    let drops = std::fs::read_dir(paths::drops(&home)).map_or(0, Iterator::count);
    assert_eq!(drops, 0, "a refused upload leaves nothing behind");
    let _ = std::fs::remove_dir_all(&home);
}
