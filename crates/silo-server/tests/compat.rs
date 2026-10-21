//! Peers of other builds: an older client (no `features`), a newer one
//! (unknown methods and frame kinds) and an incompatible protocol.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::json;
use silo_client::Control;
use silo_proto::{
    Frame, PROTOCOL, VT,
    control::{Created, DataTicket, Method, RpcError, ServerHello, ServerMessage, SpawnSpec},
    data::{ClientData, DataHello, ServerData},
    kind,
    model::{BlockKind, TermSize},
    paths, read_frame, write_frame,
};
use silo_transport::RelayConfig;
use tokio::{io::AsyncWriteExt, net::UnixStream, task::JoinHandle, time::timeout};

const T: Duration = Duration::from_secs(10);

fn temp_home(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/silo-compat-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

async fn start(home: &Path) -> JoinHandle<()> {
    let config = silo_server::Config { home: home.to_owned(), relay: RelayConfig::Disabled, mdns: false, ..Default::default() };
    let task = tokio::spawn(async move { silo_server::run(config).await.unwrap() });
    for _ in 0..500 {
        if silo_client::server_running(home).await {
            return task;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("server did not start");
}

async fn send(stream: &mut UnixStream, frame: &Frame) {
    write_frame(stream, frame).await.unwrap();
    stream.flush().await.unwrap();
}

async fn recv(stream: &mut UnixStream) -> Frame {
    timeout(T, read_frame(stream)).await.unwrap().unwrap().expect("a frame")
}

/// Send a raw JSON hello; the server's reply frame.
async fn raw_hello(home: &Path, hello: serde_json::Value) -> (UnixStream, Frame) {
    let mut stream = UnixStream::connect(paths::socket(home)).await.unwrap();
    send(&mut stream, &Frame::json(kind::CONTROL_HELLO, &hello).unwrap()).await;
    let reply = recv(&mut stream).await;
    (stream, reply)
}

async fn response(stream: &mut UnixStream, id: u64) -> Result<serde_json::Value, RpcError> {
    loop {
        match recv(stream).await.parse::<ServerMessage>().unwrap() {
            ServerMessage::Response { id: got, result, error } if got == id => return error.map_or(Ok(result.unwrap_or_default()), Err),
            _ => {}
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn older_and_newer_peers_work_together() {
    let home = temp_home("peers");
    let task = start(&home).await;

    // Another build of the same protocol.
    let (mut old, reply) = raw_hello(&home, json!({"protocol": PROTOCOL, "vt": VT, "build": "older", "client_name": "old"})).await;
    assert_eq!(reply.kind, kind::CONTROL_HELLO);
    let hello: ServerHello = reply.parse().unwrap();
    assert_eq!(hello.protocol, PROTOCOL);
    assert_eq!(hello.features, silo_proto::compat::server_features());

    // A newer client: an unknown method gets a distinct error, an unknown
    // frame kind is skipped, extra params fields are ignored.
    send(&mut old, &Frame::json(kind::CONTROL, &json!({"id": 1, "method": "future.thing", "params": {}})).unwrap()).await;
    assert_eq!(response(&mut old, 1).await.unwrap_err().code, RpcError::METHOD_NOT_FOUND);
    send(&mut old, &Frame::new(0x7f, b"from the future".to_vec())).await;
    send(&mut old, &Frame::json(kind::CONTROL, &json!({"id": 2, "method": "ping", "params": {"later": true}})).unwrap()).await;
    response(&mut old, 2).await.unwrap();

    // Data streams skip unknown frame kinds too.
    let control = Control::connect(&paths::socket(&home), "owner").await.unwrap().0;
    let spawn = SpawnSpec { command: vec!["/bin/sh".into()], cwd: Some("/tmp".into()), size: Some(TermSize::new(80, 24)) };
    let created: Created = control.call(Method::SessionCreate { name: None, spawn, kind: BlockKind::Terminal, url: None }).await.unwrap();
    let ticket: DataTicket = control.call(Method::DataConnect { block_id: created.block_id }).await.unwrap();
    let mut data = UnixStream::connect(paths::socket(&home)).await.unwrap();
    send(&mut data, &Frame::json(kind::DATA_HELLO, &DataHello::ours(ticket.ticket, None)).unwrap()).await;
    assert_eq!(recv(&mut data).await.kind, kind::WELCOME);
    send(&mut data, &ClientData::Unknown(0x7e).to_frame()).await;
    send(&mut data, &ClientData::Input(b"echo compat-ok\n".to_vec()).to_frame()).await;
    let mut seen = Vec::new();
    timeout(T, async {
        loop {
            if let Ok(ServerData::Output { bytes, .. }) = ServerData::from_frame(recv(&mut data).await) {
                seen.extend(bytes);
                if String::from_utf8_lossy(&seen).matches("compat-ok").count() >= 2 {
                    return;
                }
            }
        }
    })
    .await
    .expect("input after an unknown frame still reaches the shell");

    // A different protocol is refused, saying why.
    let (_, reply) = raw_hello(&home, json!({"protocol": PROTOCOL + 1, "vt": VT, "build": "newer", "client_name": "new"})).await;
    let ServerMessage::Response { error: Some(error), .. } = reply.parse().unwrap() else { panic!("not a rejection") };
    assert_eq!(error.code, RpcError::INCOMPATIBLE);
    assert!(error.message.contains(&format!("protocol {}", PROTOCOL + 1)), "{}", error.message);
    // So is a data stream of another terminal format.
    let mut data = UnixStream::connect(paths::socket(&home)).await.unwrap();
    let hello = DataHello { vt: "ghostsnp1-other".into(), ..DataHello::ours("00".into(), None) };
    send(&mut data, &Frame::json(kind::DATA_HELLO, &hello).unwrap()).await;
    let reject = recv(&mut data).await;
    assert_eq!(reject.kind, kind::REJECT);
    assert!(String::from_utf8_lossy(&reject.payload).contains(RpcError::INCOMPATIBLE));

    control.call_value(Method::ServerShutdown { force: false }).await.unwrap();
    drop(control);
    timeout(T, task).await.unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&home);
}
