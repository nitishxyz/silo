//! Remote agents end to end: B is an in-process silo server whose `otto`
//! service starts `silo otto server --daemon` (this build) on temporary
//! homes, like `silo server` does; A is a device paired over loopback (no
//! relays, no mDNS) reaching it through `silo/tunnel/1`. The model is a fake
//! Chat Completions server here, signed in as the `custom` provider.

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use otto_client::{Connection, Dial, FollowerState, SessionCreateOptions, SessionFollower, Stream};
use otto_proto::{AuthMode, Delivery, InputPart, LoginResult, Method, ServerInfo, SessionStatus, TurnStatus};
use silo_client::{
    Control,
    callback::CallbackForward,
    transport::{EndpointAddr, Net, PairTicket, RelayConfig, tunnel::TunnelErrCode},
    tunnel::{TunnelDialer, refusal},
};
use silo_proto::{
    compat::feature,
    control::{InvitePurpose, Method as SiloMethod, PairInvite},
    paths,
};
use silo_server::ServiceFn;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::watch,
    task::{AbortHandle, JoinHandle},
    time::timeout,
};

const T: Duration = Duration::from_secs(20);
const KEY: &str = "sk-remote-agent-test-4b1f";

/// One test's temporary tree: `a/` is the device, `b/silo`, `b/otto`,
/// `b/config`, `b/user` (the daemon's `HOME`) and `b/work` are the host.
/// Dropped (also on panic): stops B's otto daemon, then removes the tree.
struct Tree(PathBuf);

impl Tree {
    fn new(name: &str) -> Tree {
        let root = PathBuf::from(format!("/tmp/silo-ra-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for dir in ["a", "b/silo", "b/otto", "b/config", "b/user", "b/work"] {
            std::fs::create_dir_all(root.join(dir)).unwrap();
        }
        Tree(root)
    }
    fn a(&self) -> PathBuf {
        self.0.join("a")
    }
    fn b(&self, dir: &str) -> PathBuf {
        self.0.join("b").join(dir)
    }
}

impl Drop for Tree {
    fn drop(&mut self) {
        let home = self.b("otto");
        if let Ok(runtime) = std::fs::read(otto_proto::paths::runtime(&home))
            && let Ok(runtime) = serde_json::from_slice::<otto_proto::paths::Runtime>(&runtime)
        {
            unsafe { libc::kill(runtime.pid as i32, libc::SIGTERM) };
            for _ in 0..100 {
                if unsafe { libc::kill(runtime.pid as i32, 0) } != 0 {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// B's `otto` service, as silo-cli's `otto_service` builds it but with
/// every home in `tree` (the daemon gets `OTTO_SILO_HOME` from `ensure_started`).
fn otto_service(tree: &Tree) -> ServiceFn {
    let (home, silo, config, user) = (tree.b("otto"), tree.b("silo"), tree.b("config"), tree.b("user"));
    Arc::new(move || -> std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<PathBuf>> + Send>> {
        let (home, silo, config, user) = (home.clone(), silo.clone(), config.clone(), user.clone());
        Box::pin(async move {
            let envs = [("SILO_HOME", silo.as_os_str()), ("OTTO_SILO_CONFIG_DIR", config.as_os_str()), ("HOME", user.as_os_str())];
            otto_client::ensure_started(&home, Path::new(env!("CARGO_BIN_EXE_silo")), &["otto", "server", "--daemon"], &envs)
                .await
                .map_err(|e| std::io::Error::other(format!("otto: {e}")))?;
            Ok(otto_proto::paths::socket(&home))
        })
    })
}

/// B running (with the otto service when `otto`), A paired with it.
struct Remote {
    task: JoinHandle<()>,
    control: Control,
    dialer: Arc<TunnelDialer>,
}

impl Remote {
    async fn start(tree: &Tree, otto: bool) -> Remote {
        let home = tree.b("silo");
        let mut config = silo_server::Config { home: home.clone(), idle_exit: None, relay: RelayConfig::Disabled, mdns: false, ..Default::default() };
        if otto {
            config.services.insert(feature::OTTO.into(), otto_service(tree));
        }
        let task = tokio::spawn(async move { silo_server::run(config).await.unwrap() });
        let mut control = None;
        for _ in 0..500 {
            if silo_client::server_running(&home).await {
                control = Some(Control::connect(&paths::socket(&home), "owner").await.unwrap().0);
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let control = control.expect("server did not start");
        let ticket = invite(&control).await;
        let net = Net::bind(&silo_client::client_identity(&tree.a().join("client")).unwrap(), RelayConfig::Disabled, false).await.unwrap();
        drop(timeout(T, silo_client::pair(&net, &ticket, "laptop")).await.unwrap().unwrap());
        Remote { task, control, dialer: Arc::new(TunnelDialer::new(Arc::new(net), ticket.addr)) }
    }

    async fn stop(self) {
        drop(self.dialer);
        self.control.call_value(SiloMethod::ServerShutdown { force: false }).await.unwrap();
        drop(self.control);
        timeout(T, self.task).await.unwrap().unwrap();
    }
}

/// A fresh device invite, rewritten to the server's loopback address.
async fn invite(control: &Control) -> PairTicket {
    for _ in 0..200 {
        let invite: PairInvite = control.call(SiloMethod::PairCreate { ttl_secs: 60, purpose: InvitePurpose::Device }).await.unwrap();
        let ticket = PairTicket::decode(&invite.ticket).unwrap();
        if let Some(port) = ticket.addr.ip_addrs().find(|a| a.is_ipv4()).map(SocketAddr::port) {
            let addr = EndpointAddr::new(ticket.addr.id).with_ip_addr((Ipv4Addr::LOCALHOST, port).into());
            return PairTicket { addr, secret: ticket.secret };
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("server endpoint has no direct address");
}

/// A dial through B's `otto` service. Each stream is relayed by a task whose
/// abort handle lands in `links`: aborting them drops the tunnel streams.
fn dial(dialer: Arc<TunnelDialer>, links: Arc<Mutex<Vec<AbortHandle>>>) -> Dial {
    Arc::new(move || {
        let (dialer, links) = (dialer.clone(), links.clone());
        Box::pin(async move {
            let mut tunnel = dialer.open_service("otto").await?;
            let (ours, mut theirs) = tokio::io::duplex(1 << 16);
            let relay = tokio::spawn(async move {
                let _ = tokio::io::copy_bidirectional(&mut tunnel, &mut theirs).await;
            });
            links.lock().unwrap().push(relay.abort_handle());
            Ok(Box::new(ours) as Box<dyn Stream>)
        })
    })
}

/// A Chat Completions server: `GET` lists the model `fast`; `POST` streams
/// `reply` in 6-character chunks, one every `every` (counted in `sent`).
struct Model {
    port: u16,
    sent: Arc<AtomicUsize>,
}

async fn model(reply: &'static str, every: Duration) -> Model {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let sent = Arc::new(AtomicUsize::new(0));
    let counter = sent.clone();
    tokio::spawn(async move {
        while let Ok((tcp, _)) = listener.accept().await {
            let sent = counter.clone();
            tokio::spawn(async move {
                let _ = answer(tcp, reply, every, &sent).await;
            });
        }
    });
    Model { port, sent }
}

async fn answer(mut tcp: TcpStream, reply: &str, every: Duration, sent: &AtomicUsize) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let end = loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break end + 4;
        }
        let mut chunk = [0u8; 4096];
        let n = tcp.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
    let length = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse::<usize>().ok()).unwrap_or(0);
    let mut body = buf.split_off(end);
    while body.len() < length {
        let mut chunk = [0u8; 4096];
        let n = tcp.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    if head.starts_with("get ") {
        let json = serde_json::json!({"object": "list", "data": [{"id": "fast", "object": "model"}]}).to_string();
        let out = format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{json}", json.len());
        return tcp.write_all(out.as_bytes()).await;
    }
    tcp.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n").await?;
    let event = |delta: serde_json::Value, finish: Option<&str>| {
        let chunk = serde_json::json!({"id": "x", "object": "chat.completion.chunk", "model": "fast", "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]});
        format!("data: {chunk}\n\n")
    };
    let chars: Vec<char> = reply.chars().collect();
    for piece in chars.chunks(6) {
        tcp.write_all(event(serde_json::json!({"content": piece.iter().collect::<String>()}), None).as_bytes()).await?;
        tcp.flush().await?;
        sent.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(every).await;
    }
    tcp.write_all(event(serde_json::json!({}), Some("stop")).as_bytes()).await?;
    tcp.write_all(b"data: [DONE]\n\n").await?;
    tcp.flush().await
}

/// Whether any file under `dir` contains `needle`.
fn contains(dir: &Path, needle: &str) -> bool {
    std::fs::read_dir(dir).into_iter().flatten().flatten().any(|entry| {
        let path = entry.path();
        if path.is_dir() {
            contains(&path, needle)
        } else {
            std::fs::read(&path).is_ok_and(|b| b.windows(needle.len()).any(|w| w == needle.as_bytes()))
        }
    })
}

async fn until(changes: &mut watch::Receiver<FollowerState>, what: &str, done: impl Fn(&FollowerState) -> bool) -> FollowerState {
    let wait = async {
        loop {
            let state = changes.borrow_and_update().clone();
            if done(&state) {
                return state;
            }
            changes.changed().await.unwrap();
        }
    };
    timeout(T, wait).await.unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

fn finished(state: &FollowerState) -> bool {
    state.mirror.turn.is_some_and(|(_, status)| status != TurnStatus::Running) && state.mirror.info.status != SessionStatus::Busy
}

fn replied(state: &FollowerState, reply: &str) -> bool {
    state.mirror.items.iter().any(|item| item.text() == reply)
}

/// Signs B's otto in to the fake model and opens a session there.
async fn signed_in_session(otto: &Connection, tree: &Tree, model: &Model) -> String {
    let login = otto
        .call_typed::<LoginResult>(Method::AuthLogin {
            provider: "custom".into(),
            mode: AuthMode::ApiKey,
            api_key: Some(KEY.into()),
            device: false,
            base_url: Some(format!("http://127.0.0.1:{}/v1", model.port)),
        })
        .await
        .unwrap();
    let LoginResult::SignedIn { auth } = login else { panic!("an API key signs in at once: {login:?}") };
    assert!(auth.signed_in && auth.provider == "custom", "{auth:?}");
    let models = otto.model_list(Some("custom".into())).await.unwrap();
    assert!(models.iter().any(|m| m.id == "fast"), "{models:?}");
    let options = SessionCreateOptions {
        project_root: Some(tree.b("work").to_string_lossy().into_owned()),
        provider: Some("custom".into()),
        model: Some("fast".into()),
        ..Default::default()
    };
    otto.session_create(options).await.unwrap().id
}

fn text(text: &str) -> Vec<InputPart> {
    vec![InputPart::Text { text: text.into() }]
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_agent_runs_on_the_remote() {
    const REPLY: &str = "Hello from the host: this reply was streamed by the otto daemon running next to the remote silo server.";
    let tree = Tree::new("run");
    let remote = Remote::start(&tree, true).await;
    assert!(remote.control.hello().features.iter().any(|f| f == feature::OTTO), "{:?}", remote.control.hello().features);
    let model = model(REPLY, Duration::from_millis(5)).await;
    let otto = timeout(T, Connection::connect_dial(dial(remote.dialer.clone(), Arc::default()))).await.unwrap().unwrap();

    let info: ServerInfo = otto.call_typed(Method::ServerInfo {}).await.unwrap();
    assert_eq!(info.pid, otto.hello().pid);
    assert_ne!(info.pid, std::process::id(), "otto runs as B's daemon, not in this process");
    assert_eq!(otto.hello().home, tree.b("user").to_string_lossy(), "the daemon's home is B's");

    let session = signed_in_session(&otto, &tree, &model).await;
    assert!(contains(&tree.b("otto"), KEY), "the key is stored in B's otto home");
    assert!(!contains(&tree.a(), KEY), "nothing about the sign-in is stored on A");

    let follower = SessionFollower::new(otto.clone(), session.clone()).await.unwrap();
    let mut changes = follower.changes();
    otto.prompt(&session, text("say hello"), Delivery::Queue).await.unwrap();
    let state = until(&mut changes, "the reply", |s| finished(s) && replied(s, REPLY)).await;
    assert_eq!(state.mirror.turn.map(|(_, status)| status), Some(TurnStatus::Done), "{:?}", state.mirror.last_error);
    assert!(model.sent.load(Ordering::SeqCst) > 1, "the reply was streamed");

    drop(follower);
    otto.close();
    remote.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn remote_agent_reconnects() {
    const REPLY: &str = "One. Two. Three. Four. Five. Six. Seven. Eight. Nine. Ten. Eleven. Twelve. Thirteen. Fourteen. Fifteen. \
        Sixteen. Seventeen. Eighteen. Nineteen. Twenty. Twenty-one. Twenty-two. Twenty-three. Twenty-four. The end.";
    let tree = Tree::new("reconnect");
    let remote = Remote::start(&tree, true).await;
    let model = model(REPLY, Duration::from_millis(60)).await;
    let links = Arc::new(Mutex::new(Vec::new()));
    let otto = timeout(T, Connection::connect_dial(dial(remote.dialer.clone(), links.clone()))).await.unwrap().unwrap();
    let session = signed_in_session(&otto, &tree, &model).await;

    let follower = SessionFollower::new(otto.clone(), session.clone()).await.unwrap();
    let mut changes = follower.changes();
    otto.prompt(&session, text("count slowly"), Delivery::Queue).await.unwrap();
    let wait = async {
        while model.sent.load(Ordering::SeqCst) < 8 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    timeout(T, wait).await.expect("the reply started");
    let before = changes.borrow().mirror.last_seq;

    // The tunnel streams go mid-reply; the follower redials and resumes.
    for link in links.lock().unwrap().drain(..) {
        link.abort();
    }
    until(&mut changes, "the disconnect", |s| !s.connected).await;
    assert!(model.sent.load(Ordering::SeqCst) * 6 < REPLY.len(), "the reply was still streaming when the tunnel went");
    let state = until(&mut changes, "the reconnect", |s| s.connected).await;
    assert!(state.mirror.last_seq >= before, "resumed from last_seq, not from scratch");
    assert!(!links.lock().unwrap().is_empty(), "redialed through the tunnel");

    let state = until(&mut changes, "the rest of the reply", |s| finished(s) && replied(s, REPLY)).await;
    assert_eq!(state.mirror.turn.map(|(_, status)| status), Some(TurnStatus::Done), "{:?}", state.mirror.last_error);
    let fresh = otto.reconnect().await.unwrap();
    let view = fresh.session_view(&session).await.unwrap();
    assert_eq!(state.mirror.last_seq, view.seq, "no events missing");
    let texts = |items: &[otto_proto::Item]| items.iter().map(|i| (i.id.clone(), i.text())).collect::<Vec<_>>();
    assert_eq!(texts(&state.mirror.items), texts(&view.items), "the mirror matches the host");

    drop(follower);
    fresh.close();
    otto.close();
    remote.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn callback_forward_reaches_the_remote_listener() {
    let tree = Tree::new("callback");
    let remote = Remote::start(&tree, false).await;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let remote_port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let log = seen.clone();
    let server = tokio::spawn(async move {
        while let Ok((mut tcp, _)) = listener.accept().await {
            let mut head = Vec::new();
            while !head.ends_with(b"\r\n\r\n") {
                let mut byte = [0u8; 1];
                if tcp.read(&mut byte).await.unwrap_or(0) == 0 {
                    break;
                }
                head.push(byte[0]);
            }
            let line = String::from_utf8_lossy(&head).lines().next().unwrap_or_default().to_owned();
            log.lock().unwrap().push(line);
            let _ = tcp.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\nConnection: close\r\n\r\nsigned in").await;
        }
    });
    let local = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap().local_addr().unwrap().port();
    let forward = CallbackForward::start(remote.dialer.clone(), local, remote_port, "/auth/callback".into(), Duration::from_secs(30)).unwrap();

    let request = |head: &'static str| async move {
        let mut tcp = TcpStream::connect((Ipv4Addr::LOCALHOST, local)).await.unwrap();
        tcp.write_all(head.as_bytes()).await.unwrap();
        let mut out = String::new();
        timeout(T, tcp.read_to_string(&mut out)).await.unwrap().unwrap();
        out
    };
    let out = request("GET /auth/callback?code=abc&state=xyz HTTP/1.1\r\nHost: localhost\r\n\r\n").await;
    assert!(out.starts_with("HTTP/1.1 200") && out.ends_with("signed in"), "{out}");
    let out = request("POST /auth/callback HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n").await;
    assert!(out.starts_with("HTTP/1.1 404 "), "{out}");
    assert_eq!(*seen.lock().unwrap(), ["GET /auth/callback?code=abc&state=xyz HTTP/1.1"], "only the GET reached B");

    drop(forward);
    server.abort();
    remote.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn host_without_otto_is_not_found() {
    let tree = Tree::new("none");
    let remote = Remote::start(&tree, false).await;
    assert!(!remote.control.hello().features.iter().any(|f| f == feature::OTTO), "{:?}", remote.control.hello().features);
    let err = timeout(T, remote.dialer.open_service("otto")).await.unwrap().unwrap_err();
    assert_eq!(refusal(&err), Some(TunnelErrCode::NotFound), "{err}");
    remote.stop().await;
}
