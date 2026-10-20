//! Data-stream flow control and resume: ring-boundary resume, transition
//! ordering, slow-subscriber skip-ahead, bounded snapshot sends and ACKs.

use std::{sync::atomic::Ordering, time::Duration};

use silo_proto::{
    Frame,
    data::{Resume, ServerData, StartMode, Welcome},
    kind,
    model::{BlockInfo, BlockKind, Lifecycle, TermSize, TerminalConfig},
};
use silo_pty::SpawnConfig;
use silo_server::block::{self, Attach, BlockHandle, BlockMsg, HISTORY_CHUNK, Note, PUMP_WINDOW, RING_CAP, Sink, SubscriberStats};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

fn info(id: &str, size: TermSize) -> BlockInfo {
    BlockInfo {
        id: id.into(),
        session_id: "s-test".into(),
        kind: BlockKind::Terminal,
        title: "shell".into(),
        cwd: None,
        exited: None,
        lifecycle: Lifecycle::Running,
        size,
        owner: None,
        owner_name: None,
        pid: None,
        browser: None,
    }
}

fn spawn(id: &str, size: TermSize, script: &str) -> (BlockHandle, UnboundedReceiver<Note>) {
    let (notes, rx) = unbounded_channel();
    let dir = std::env::temp_dir();
    let config = SpawnConfig { command: vec!["/bin/sh".into(), "-c".into(), script.into()], cwd: Some(dir), env: Vec::new() };
    (block::spawn(info(id, size), &config, notes).expect("spawn"), rx)
}

/// A fake data connection: frames land in `rx`; `recv` plays the writer and
/// keeps the sink's unsent-bytes counter balanced.
struct Client {
    sink: Sink,
    rx: UnboundedReceiver<Frame>,
}

impl Client {
    fn attach(block: &BlockHandle, sub_id: u64, resume: Option<Resume>) -> Self {
        let (tx, rx) = unbounded_channel();
        let sink = Sink::new(tx);
        block.send(BlockMsg::Attach(Box::new(Attach { sub_id, client_id: 7, resume, sink: sink.clone(), remote: false })));
        Self { sink, rx }
    }

    async fn recv(&mut self, wait: Duration) -> Option<Frame> {
        let frame = tokio::time::timeout(wait, self.rx.recv()).await.ok()??;
        self.sink.queued.fetch_sub(frame.payload.len() + 5, Ordering::AcqRel);
        Some(frame)
    }

    async fn welcome(&mut self) -> Welcome {
        let frame = self.recv(Duration::from_secs(5)).await.expect("welcome frame");
        assert_eq!(frame.kind, kind::WELCOME);
        frame.parse().unwrap()
    }

    /// Everything until the stream goes quiet.
    async fn drain(&mut self, quiet: Duration) -> Vec<ServerData> {
        let mut out = Vec::new();
        while let Some(frame) = self.recv(quiet).await {
            out.push(ServerData::from_frame(frame).unwrap());
        }
        out
    }
}

async fn stats(block: &BlockHandle, sub_id: u64) -> Option<SubscriberStats> {
    let (reply, rx) = tokio::sync::oneshot::channel();
    block.send(BlockMsg::Subscriber { sub_id, reply });
    rx.await.unwrap()
}

/// Wait until the block's output offset reaches `offset`; returns the epoch.
async fn wait_offset(block: &BlockHandle, offset: u64) -> u64 {
    for _ in 0..400 {
        let checkpoint = block.checkpoint().await.unwrap();
        if checkpoint.offset >= offset {
            assert_eq!(checkpoint.offset, offset);
            return checkpoint.epoch;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("output never reached {offset}");
}

/// Wait until at least `min` bytes were produced and output stopped.
async fn wait_quiet(block: &BlockHandle, min: u64) {
    let mut last = (0, 0);
    for _ in 0..400 {
        let offset = block.checkpoint().await.unwrap().offset;
        last = if offset == last.0 { (offset, last.1 + 1) } else { (offset, 0) };
        if offset >= min && last.1 >= 4 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("output never settled: {last:?}");
}

async fn wait_cancelled(sink: &Sink) {
    tokio::time::timeout(Duration::from_secs(20), sink.cancelled()).await.expect("subscriber cut off");
}

#[derive(Debug, PartialEq)]
enum Event {
    Bytes(u64, Vec<u8>),
    Resized(u64, TermSize),
    Configured(TerminalConfig),
}

/// Output merged into contiguous runs (chunking differs between live and
/// replayed streams), transitions kept in order.
fn events(data: impl IntoIterator<Item = ServerData>) -> Vec<Event> {
    let mut out: Vec<Event> = Vec::new();
    for item in data {
        match item {
            ServerData::Output { offset, bytes } => match out.last_mut() {
                Some(Event::Bytes(start, run)) if *start + run.len() as u64 == offset => run.extend(bytes),
                _ => out.push(Event::Bytes(offset, bytes)),
            },
            ServerData::Resized { offset, size } => out.push(Event::Resized(offset, size)),
            ServerData::Configured(config) => out.push(Event::Configured(config)),
            other => panic!("unexpected {other:?}"),
        }
    }
    out
}

#[tokio::test]
async fn resume_at_ring_boundary_replays_else_snapshot() {
    const TOTAL: u64 = 5_000_000;
    let script = format!("head -c {TOTAL} /dev/zero | tr '\\0' a; sleep 30");
    let (block, _notes) = spawn("b-ring", TermSize::new(40, 10), &script);
    let epoch = wait_offset(&block, TOTAL).await;
    let ring_start = TOTAL - RING_CAP as u64;

    // Exactly the first retained byte: replayed from the ring.
    let mut client = Client::attach(&block, 1, Some(Resume { epoch, offset: ring_start }));
    let welcome = client.welcome().await;
    assert_eq!((welcome.mode, welcome.offset, welcome.epoch), (StartMode::Resume, ring_start, epoch));
    let replayed = events(client.drain(Duration::from_millis(300)).await);
    assert!(matches!(replayed.first(), Some(Event::Configured(_))));
    match &replayed[1..] {
        [Event::Bytes(offset, bytes)] => {
            assert_eq!((*offset, bytes.len()), (ring_start, RING_CAP));
            assert!(bytes.iter().all(|b| *b == b'a'));
        }
        other => panic!("unexpected replay {:?}", other.len()),
    }
    block.send(BlockMsg::Detach(1));

    // The live end: nothing to replay.
    let mut client = Client::attach(&block, 2, Some(Resume { epoch, offset: TOTAL }));
    let welcome = client.welcome().await;
    assert_eq!((welcome.mode, welcome.offset), (StartMode::Resume, TOTAL));
    let rest = events(client.drain(Duration::from_millis(200)).await);
    assert_eq!(rest.len(), 1, "only the configuration follows");
    block.send(BlockMsg::Detach(2));

    // Just before the ring, a future offset, a stale epoch: snapshot.
    for (sub_id, resume) in [
        (3, Resume { epoch, offset: ring_start - 1 }),
        (4, Resume { epoch, offset: TOTAL + 1 }),
        (5, Resume { epoch: epoch ^ 1, offset: TOTAL }),
        (6, Resume { epoch, offset: u64::MAX }),
    ] {
        let mut client = Client::attach(&block, sub_id, Some(resume));
        let welcome = client.welcome().await;
        assert_eq!((welcome.mode, welcome.offset, welcome.epoch), (StartMode::Snapshot, TOTAL, epoch), "{resume:?}");
        block.send(BlockMsg::Detach(sub_id));
    }
    block.send(BlockMsg::Shutdown);
}

#[tokio::test]
async fn resume_preserves_resize_and_config_order() {
    let initial = TermSize::new(40, 10);
    let (block, _notes) = spawn("b-order", initial, "sleep 30");
    let mut live = Client::attach(&block, 1, None);
    let welcome = live.welcome().await;
    let start = welcome.offset;
    let mut prelude = live.drain(Duration::from_millis(200)).await;
    assert!(matches!(prelude.pop(), Some(ServerData::SnapshotDone)));

    let config = |background| TerminalConfig { background, ..TerminalConfig::default() };
    let inject = |text: &str| BlockMsg::Inject { bytes: text.as_bytes().to_vec(), input: Vec::new() };
    let resize = |columns| BlockMsg::Desired { client_id: 9, client_name: "t".into(), label: "t".into(), size: TermSize::new(columns, 12) };
    for msg in [
        inject("one"),
        resize(50),
        BlockMsg::Config { client_id: 9, config: config(0x0011_2233) },
        inject("two"),
        BlockMsg::Config { client_id: 9, config: config(0x0044_5566) },
        resize(60),
        inject("three"),
        resize(70),
    ] {
        block.send(msg);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let live_events = events(live.drain(Duration::from_millis(400)).await);
    assert_eq!(live_events.iter().filter(|e| matches!(e, Event::Resized(..))).count(), 3);
    assert_eq!(live_events.iter().filter(|e| matches!(e, Event::Configured(_))).count(), 2);

    let mut resumed = Client::attach(&block, 2, Some(Resume { epoch: welcome.epoch, offset: start }));
    let again = resumed.welcome().await;
    assert_eq!((again.mode, again.offset, again.size), (StartMode::Resume, start, initial));
    let mut replay = events(resumed.drain(Duration::from_millis(300)).await);
    assert_eq!(replay.remove(0), Event::Configured(TerminalConfig::default()));
    assert_eq!(replay, live_events);

    // Resuming mid-way starts from the state in effect there.
    let Some(Event::Bytes(two_at, _)) = live_events.iter().find(|e| matches!(e, Event::Bytes(_, b) if b == b"two")) else {
        panic!("no 'two' in {live_events:?}");
    };
    let mut mid = Client::attach(&block, 3, Some(Resume { epoch: welcome.epoch, offset: *two_at }));
    let at_two = mid.welcome().await;
    assert_eq!(at_two.size, TermSize::new(50, 12));
    let replay = events(mid.drain(Duration::from_millis(300)).await);
    assert_eq!(replay[0], Event::Configured(config(0x0011_2233)));
    block.send(BlockMsg::Shutdown);
}

#[tokio::test]
async fn slow_subscriber_is_cut_while_fast_one_keeps_up() {
    const TOTAL: u64 = 30_000_000;
    let script = format!("sleep 0.5; head -c {TOTAL} /dev/zero | tr '\\0' a; sleep 30");
    let (block, _notes) = spawn("b-slow", TermSize::new(40, 10), &script);
    // Never reads: its writer queue only grows.
    let slow = Client::attach(&block, 1, None);
    let mut fast = Client::attach(&block, 2, None);
    let start = fast.welcome().await.offset;
    let mut next = start;
    let mut acks = 0u32;
    while next < TOTAL {
        let Some(frame) = fast.recv(Duration::from_secs(20)).await else {
            panic!("fast subscriber starved at {next} (cut: {})", fast.sink.is_cancelled());
        };
        if let ServerData::Output { offset, bytes } = ServerData::from_frame(frame).unwrap() {
            assert_eq!(offset, next, "gap in fast subscriber output");
            next += bytes.len() as u64;
            acks += 1;
            if acks.is_multiple_of(16) {
                block.send(BlockMsg::Ack { sub_id: 2, offset: next });
            }
        }
    }
    assert_eq!(next, TOTAL);
    wait_cancelled(&slow.sink).await;
    assert!(stats(&block, 1).await.is_none(), "slow subscriber dropped");
    let fast_stats = stats(&block, 2).await.expect("fast subscriber kept");
    assert_eq!(fast_stats.sent, TOTAL);
    assert!(!fast.sink.is_cancelled());
    block.send(BlockMsg::Shutdown);
}

#[tokio::test]
async fn large_history_attach_is_streamed_within_budget() {
    let script = "od -An -tx1 -v /dev/urandom | head -c 4000000 | tr -d '\\n'; sleep 30";
    let (block, _notes) = spawn("b-history", TermSize::new(200, 50), script);
    wait_quiet(&block, 3_000_000).await;
    let bound = PUMP_WINDOW + HISTORY_CHUNK + 256 * 1024;

    let mut client = Client::attach(&block, 1, None);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let held = stats(&block, 1).await.expect("subscribed");
    assert!(held.queued <= bound, "writer queue {} over {bound}", held.queued);
    assert!(held.bulk > 0, "history larger than the window waits in the actor: {held:?}");
    assert!(held.pending < 1024, "only SNAPSHOT_DONE waits behind the history: {held:?}");

    let mut history = 0usize;
    loop {
        let queued = client.sink.backlog();
        assert!(queued <= bound, "writer queue {queued} over {bound}");
        let frame = client.recv(Duration::from_secs(10)).await.expect("snapshot stalled");
        if frame.kind == kind::WELCOME {
            continue;
        }
        match ServerData::from_frame(frame).unwrap() {
            ServerData::SnapshotHistory(bytes) => history += bytes.len(),
            ServerData::SnapshotDone => break,
            _ => {}
        }
    }
    assert!(history > PUMP_WINDOW, "history {history} should exceed the window");
    let done = stats(&block, 1).await.unwrap();
    assert_eq!((done.bulk, done.pending), (0, 0));
    block.send(BlockMsg::Shutdown);
}

#[tokio::test]
async fn acks_are_monotonic_and_bounded() {
    let (block, _notes) = spawn("b-ack", TermSize::new(40, 10), "sleep 30");
    let mut client = Client::attach(&block, 1, None);
    let start = client.welcome().await.offset;
    client.drain(Duration::from_millis(200)).await;
    block.send(BlockMsg::Inject { bytes: b"hello".to_vec(), input: Vec::new() });
    client.drain(Duration::from_millis(200)).await;
    let s = stats(&block, 1).await.unwrap();
    assert_eq!((s.sent, s.acked), (start + 5, start), "acked starts at Welcome.offset");

    let ack = |sub_id, offset| BlockMsg::Ack { sub_id, offset };
    for offset in [start, start + 3, start + 3, start + 5] {
        block.send(ack(1, offset));
        assert_eq!(stats(&block, 1).await.unwrap().acked, offset);
    }
    assert!(!client.sink.is_cancelled());

    // Backwards, not yet sent, or absurd: the stream ends.
    let end = start + 5;
    for (sub_id, before, bad) in [(2, Some(end), end - 1), (3, None, end + 1), (4, None, u64::MAX)] {
        let mut other = Client::attach(&block, sub_id, None);
        assert_eq!(other.welcome().await.offset, end);
        other.drain(Duration::from_millis(100)).await;
        if let Some(offset) = before {
            block.send(ack(sub_id, offset));
        }
        block.send(ack(sub_id, bad));
        wait_cancelled(&other.sink).await;
        assert!(stats(&block, sub_id).await.is_none(), "invalid ack {bad} accepted");
    }

    // Reading but no longer acking: once the acked offset leaves the ring
    // the stream is cut, even though the writer keeps up.
    let reader = tokio::spawn(async move {
        while client.recv(Duration::from_secs(5)).await.is_some() {}
        client
    });
    block.send(BlockMsg::Inject { bytes: vec![b'x'; RING_CAP + 1024], input: Vec::new() });
    let client = reader.await.unwrap();
    assert!(client.sink.is_cancelled());
    assert!(stats(&block, 1).await.is_none());
    block.send(BlockMsg::Shutdown);
}

#[tokio::test]
async fn signal_killed_child_reports_no_exit_code() {
    let (block, mut notes) = spawn("b-signal", TermSize::new(40, 10), "kill -9 $$");
    let wait = async {
        while let Some(note) = notes.recv().await {
            if matches!(note, Note::Exited(_)) {
                return;
            }
        }
        panic!("notes closed");
    };
    tokio::time::timeout(Duration::from_secs(10), wait).await.expect("exit note");
    {
        let info = block.info.lock().unwrap();
        assert_eq!((info.lifecycle, info.exited), (Lifecycle::Exited, None));
    }
    let mut client = Client::attach(&block, 1, None);
    client.welcome().await;
    let frames = client.drain(Duration::from_millis(300)).await;
    assert_eq!(frames.last(), Some(&ServerData::Exited { code: None }));
    assert_eq!(block.checkpoint().await.unwrap().exit_code, None);
    block.send(BlockMsg::Shutdown);
}

#[tokio::test]
async fn dirty_note_once_per_checkpoint() {
    let (block, mut notes) = spawn("b-dirty", TermSize::new(40, 10), "sleep 30");
    let dirty = |notes: &mut UnboundedReceiver<Note>| {
        let mut count = 0;
        while let Ok(note) = notes.try_recv() {
            count += usize::from(matches!(note, Note::Dirty(_)));
        }
        count
    };
    let inject = |text: &str| BlockMsg::Inject { bytes: text.as_bytes().to_vec(), input: Vec::new() };
    block.checkpoint().await.unwrap();
    dirty(&mut notes);
    block.checkpoint().await.unwrap();
    assert_eq!(dirty(&mut notes), 0, "no change, no note");

    for text in ["a", "b", "c"] {
        block.send(inject(text));
    }
    block.checkpoint().await.unwrap();
    assert_eq!(dirty(&mut notes), 1, "first change only");

    block.send(inject("d"));
    block.send(BlockMsg::Desired { client_id: 9, client_name: "t".into(), label: "t".into(), size: TermSize::new(50, 12) });
    block.checkpoint().await.unwrap();
    assert_eq!(dirty(&mut notes), 1, "re-armed by the checkpoint");

    block.send(BlockMsg::Desired { client_id: 9, client_name: "t".into(), label: "t".into(), size: TermSize::new(60, 12) });
    block.checkpoint().await.unwrap();
    assert_eq!(dirty(&mut notes), 1, "resize alone dirties");
    block.send(BlockMsg::Shutdown);
}

mod control {
    use std::{path::PathBuf, time::Duration};

    use silo_client::Control;
    use silo_proto::{
        control::{Created, Method, MethodDescription, OwnMode, SpawnSpec},
        model::{BlockInfo, BlockKind, SessionView, TermSize},
        paths,
    };

    /// A server in a temp home; the home is removed on drop.
    struct Server {
        home: PathBuf,
        task: tokio::task::JoinHandle<()>,
    }

    impl Server {
        async fn start(name: &str) -> Self {
            let home = PathBuf::from(format!("/tmp/silo-stream-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&home);
            let config = silo_server::Config {
                home: home.clone(),
                idle_exit: None,
                relay: silo_server::RelayConfig::Disabled,
                mdns: false,
                ..Default::default()
            };
            let task = tokio::spawn(async move { silo_server::run(config).await.unwrap() });
            for _ in 0..300 {
                if silo_client::server_running(&home).await {
                    return Self { home, task };
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("server did not start");
        }

        async fn connect(&self, name: &str) -> Control {
            Control::connect(&paths::socket(&self.home), name).await.unwrap().0
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.task.abort();
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }

    async fn info(control: &Control, created: &Created) -> BlockInfo {
        let view: SessionView = control.call(Method::SessionView { session_id: created.session_id.clone() }).await.unwrap();
        view.blocks.into_iter().find(|b| b.id == created.block_id).unwrap()
    }

    async fn wait_owner(control: &Control, created: &Created, owner: u64, name: &str) {
        for _ in 0..500 {
            let info = info(control, created).await;
            if info.owner == Some(owner) && info.owner_name.as_deref() == Some(name) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("owner never became {owner} ({name}): {:?}", info(control, created).await);
    }

    async fn own(control: &Control, created: &Created, columns: u16) {
        let size = TermSize::new(columns, 20);
        control.call_value(Method::TerminalResize { block_id: created.block_id.clone(), size }).await.unwrap();
        control.call_value(Method::OwnBlock { block_ids: vec![created.block_id.clone()], mode: OwnMode::Take }).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn owner_name_follows_take_and_fallback() {
        let server = Server::start("owner").await;
        let a = server.connect("Silo 0.1.0 (macOS, pid 1)").await;
        let spawn = SpawnSpec { command: vec!["/bin/sh".into()], cwd: Some("/tmp".into()), size: None };
        let created: Created = a.call(Method::SessionCreate { name: None, spawn, kind: BlockKind::Terminal, url: None }).await.unwrap();
        own(&a, &created, 70).await;
        wait_owner(&a, &created, a.client_id(), "Silo").await;

        let b = server.connect("silo cli").await;
        own(&b, &created, 90).await;
        wait_owner(&a, &created, b.client_id(), "silo cli").await;
        assert_eq!(info(&a, &created).await.size.columns, 90);

        drop(b);
        wait_owner(&a, &created, a.client_id(), "Silo").await;
        assert_eq!(info(&a, &created).await.size.columns, 70);
        a.call_value(Method::ServerShutdown {}).await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ping_and_api_describe() {
        let server = Server::start("api").await;
        let control = server.connect("silo cli").await;
        assert_eq!(control.call_value(Method::Ping {}).await.unwrap(), serde_json::json!({}));
        let d: MethodDescription = control.call(Method::ApiDescribe { method: "block.float".into() }).await.unwrap();
        assert_eq!(d.example["method"], "block.float");
        assert!(d.params_schema["properties"]["block_id"].is_object(), "{}", d.params_schema);
        let err = control.call_value(Method::ApiDescribe { method: "nope".into() }).await.unwrap_err();
        assert!(err.to_string().contains("not_found"), "{err}");
        control.call_value(Method::ServerShutdown {}).await.unwrap();
    }
}

/// The driving client's stream paces the program: while it applies
/// nothing the PTY is held about a megabyte ahead of it, and acknowledging
/// lets the whole output through without the stream being cut. Held past
/// the hold limit, the program runs on and the stalled stream is cut.
#[tokio::test]
async fn driver_paces_output_until_it_stalls() {
    const TOTAL: u64 = 20_000_000;
    let script = format!("sleep 0.3; head -c {TOTAL} /dev/zero | tr '\\0' a; sleep 30");
    let (block, _notes) = spawn("b-paced", TermSize::new(40, 10), &script);
    let mut client = Client::attach(&block, 1, None);
    block.send(BlockMsg::Own { client_id: 7, mode: silo_proto::control::OwnMode::Take });
    let start = client.welcome().await.offset;
    // Unacknowledged: held near the driver window (plus the reader's read-ahead).
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let held = block.checkpoint().await.unwrap().offset;
    assert!(held > start && held < start + 2_000_000, "held at {held}");
    let mut next = start;
    while next < TOTAL {
        let Some(frame) = client.recv(Duration::from_secs(20)).await else {
            panic!("driver starved at {next} (cut: {})", client.sink.is_cancelled());
        };
        if let ServerData::Output { offset, bytes } = ServerData::from_frame(frame).unwrap() {
            assert_eq!(offset, next, "gap in driver output");
            next += bytes.len() as u64;
            block.send(BlockMsg::Ack { sub_id: 1, offset: next });
        }
    }
    assert!(!client.sink.is_cancelled(), "a driver that keeps up is never cut");
    block.send(BlockMsg::Shutdown);

    let (block, _notes) = spawn("b-stalled", TermSize::new(40, 10), &script);
    let stalled = Client::attach(&block, 1, None);
    block.send(BlockMsg::Own { client_id: 7, mode: silo_proto::control::OwnMode::Take });
    wait_cancelled(&stalled.sink).await;
    wait_offset(&block, TOTAL).await;
    block.send(BlockMsg::Shutdown);
}

/// After the child exits the actor waits only briefly for output still in
/// the PTY, so it must stop holding the reader for a stalled driver then,
/// or that output is lost. (On macOS a child's exit waits for its output to
/// drain; on Linux it does not, so the exit is reported here as the exit
/// watcher reports it, while output is still on its way.) The flood lands
/// before the client takes the block; taking it puts the driver past its
/// window, and the reader parks after handing over `X`.
#[tokio::test]
async fn output_held_for_a_stalled_driver_survives_the_exit() {
    const FLOOD: u64 = 1_200_000;
    let script = format!("sleep 0.3; head -c {FLOOD} /dev/zero | tr '\\0' a; sleep 1; printf X; sleep 0.4; printf END-MARKER; sleep 30");
    let (block, _notes) = spawn("b-held-exit", TermSize::new(40, 10), &script);
    // Never acknowledges.
    let _driver = Client::attach(&block, 1, None);
    wait_offset(&block, FLOOD).await;
    block.send(BlockMsg::Own { client_id: 7, mode: silo_proto::control::OwnMode::Take });
    wait_offset(&block, FLOOD + 1).await;
    // The marker follows within the actor's wait after this exit.
    tokio::time::sleep(Duration::from_millis(250)).await;
    block.send(BlockMsg::ChildExited(Some(0)));
    wait_offset(&block, FLOOD + b"XEND-MARKER".len() as u64).await;
    block.send(BlockMsg::Shutdown);
}

/// A focus report (mode 1004) does not make its client the driver whose
/// configuration the server terminal answers with; typing does.
#[tokio::test]
async fn focus_reports_do_not_take_the_terminal() {
    let (block, _notes) = spawn("b-focus", TermSize::new(40, 10), "cat");
    let attach = |sub_id, client_id| {
        let (tx, rx) = unbounded_channel();
        let sink = Sink::new(tx);
        block.send(BlockMsg::Attach(Box::new(Attach { sub_id, client_id, resume: None, sink: sink.clone(), remote: false })));
        Client { sink, rx }
    };
    let mut a = attach(1, 3);
    let _b = attach(2, 4);
    a.welcome().await;
    let config = |background| TerminalConfig { background, ..TerminalConfig::default() };
    block.send(BlockMsg::Config { client_id: 3, config: config(0x0011_1111) });
    block.send(BlockMsg::Config { client_id: 4, config: config(0x0022_2222) });
    block.send(BlockMsg::StreamInput { sub_id: 2, bytes: b"x".to_vec() });
    block.send(BlockMsg::StreamInput { sub_id: 1, bytes: b"\x1b[O".to_vec() });
    let configured: Vec<u32> = a
        .drain(Duration::from_millis(600))
        .await
        .into_iter()
        .filter_map(|data| match data {
            ServerData::Configured(config) => Some(config.background),
            _ => None,
        })
        .collect();
    // The first client's before anyone typed, then client 4's once it typed;
    // client 3 losing focus afterwards does not take it back.
    assert_eq!(configured.last(), Some(&0x0022_2222), "{configured:x?}");
    block.send(BlockMsg::StreamInput { sub_id: 1, bytes: b"y".to_vec() });
    let typed: Vec<u32> = a
        .drain(Duration::from_millis(600))
        .await
        .into_iter()
        .filter_map(|data| match data {
            ServerData::Configured(config) => Some(config.background),
            _ => None,
        })
        .collect();
    assert_eq!(typed, [0x0011_1111], "typing does take it");
    block.send(BlockMsg::Shutdown);
}
