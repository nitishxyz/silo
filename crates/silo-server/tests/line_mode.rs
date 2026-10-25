//! Data-stream line mode (`TTY_MODE`) and input acknowledgements (`INPUT_ACK`).

use std::time::Duration;

use silo_proto::{
    Frame,
    data::ServerData,
    kind,
    model::{BlockInfo, BlockKind, Lifecycle, TermSize},
};
use silo_pty::SpawnConfig;
use silo_server::block::{self, Attach, BlockHandle, BlockMsg, Sink};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};

fn spawn(id: &str, script: &str) -> BlockHandle {
    let info = BlockInfo {
        id: id.into(),
        session_id: "s-test".into(),
        kind: BlockKind::Terminal,
        title: "shell".into(),
        cwd: None,
        exited: None,
        lifecycle: Lifecycle::Running,
        size: TermSize::new(40, 10),
        owner: None,
        owner_name: None,
        owner_block: None,
        pid: None,
        browser: None,
        agent: None,
        files: None,
        changes: None,
    };
    let config = SpawnConfig { command: vec!["/bin/sh".into(), "-c".into(), script.into()], cwd: Some(std::env::temp_dir()), env: Vec::new() };
    let (notes, _) = unbounded_channel();
    block::spawn(info, &config, notes).expect("spawn")
}

/// One attached stream: received data frames and the output bytes with
/// the offset of the first one.
struct Stream {
    rx: UnboundedReceiver<Frame>,
    data: Vec<ServerData>,
    output: Vec<u8>,
    start: Option<u64>,
}

impl Stream {
    fn attach(block: &BlockHandle, sub_id: u64, remote: bool) -> Self {
        let (tx, rx) = unbounded_channel();
        block.send(BlockMsg::Attach(Box::new(Attach { sub_id, client_id: 7, resume: None, sink: Sink::new(tx), remote })));
        Self { rx, data: Vec::new(), output: Vec::new(), start: None }
    }

    /// Receive until `done` holds; panics after 10 s.
    async fn until(&mut self, done: impl Fn(&Self) -> bool) {
        let wait = async {
            while !done(self) {
                let frame = self.rx.recv().await.expect("stream open");
                if frame.kind == kind::WELCOME {
                    continue;
                }
                let data = ServerData::from_frame(frame).unwrap();
                if let ServerData::Output { offset, bytes } = &data {
                    self.start.get_or_insert(*offset);
                    self.output.extend_from_slice(bytes);
                }
                self.data.push(data);
            }
        };
        tokio::time::timeout(Duration::from_secs(10), wait).await.expect("timed out");
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.output).into_owned()
    }

    /// Stream offset of the end of the first `needle` in the output.
    fn end_of(&self, needle: &str) -> u64 {
        let at = self.text().find(needle).expect("needle in output");
        self.start.unwrap() + (at + needle.len()) as u64
    }

    fn modes(&self) -> Vec<(u64, bool, bool)> {
        self.data
            .iter()
            .filter_map(|d| match d {
                ServerData::TtyMode { offset, echo, icanon } => Some((*offset, *echo, *icanon)),
                _ => None,
            })
            .collect()
    }

    fn last_ack(&self) -> Option<(u64, u64)> {
        self.data.iter().rev().find_map(|d| match d {
            ServerData::InputAck { count, offset } => Some((*count, *offset)),
            _ => None,
        })
    }
}

#[tokio::test]
async fn stty_noecho_is_announced_after_preceding_output() {
    // Everything after `read go` is live output: the stream attached first.
    let block = spawn("b-mode", "read go; printf before; read x; stty -echo; printf after; read y");
    let mut stream = Stream::attach(&block, 1, true);
    block.send(BlockMsg::Input(b"go\n".to_vec()));
    stream.until(|s| s.text().contains("before") && !s.modes().is_empty()).await;
    let (_, echo, icanon) = stream.modes()[0];
    assert!(echo && icanon, "cooked with echo first");
    block.send(BlockMsg::StreamInput { sub_id: 1, bytes: b"typed\n".to_vec() });
    stream.until(|s| s.modes().iter().any(|m| !m.1)).await;
    let (offset, _, icanon) = *stream.modes().iter().find(|m| !m.1).unwrap();
    assert!(icanon);
    assert!(offset >= stream.end_of("typed"), "mode at {offset} precedes the echo before it");
    block.send(BlockMsg::Shutdown);
}

#[tokio::test]
async fn input_acks_count_inputs_and_precede_their_echo() {
    let block = spawn("b-ack", "read go; printf ready; cat");
    let mut stream = Stream::attach(&block, 1, true);
    let mut local = Stream::attach(&block, 2, false);
    block.send(BlockMsg::Input(b"go\n".to_vec()));
    stream.until(|s| s.text().contains("ready")).await;
    for key in ["x", "y", "z"] {
        block.send(BlockMsg::StreamInput { sub_id: 1, bytes: key.as_bytes().to_vec() });
    }
    stream.until(|s| s.text().contains("xyz") && s.last_ack().is_some_and(|(count, _)| count == 3)).await;
    let (_, offset) = stream.last_ack().unwrap();
    assert!(offset < stream.end_of("xyz"), "ack offset {offset} is past the echo of z");
    // Another stream's input does not count for this one; local streams
    // get neither acks nor modes.
    block.send(BlockMsg::StreamInput { sub_id: 2, bytes: b"w".to_vec() });
    stream.until(|s| s.text().contains("xyzw")).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    while let Ok(frame) = stream.rx.try_recv() {
        stream.data.push(ServerData::from_frame(frame).unwrap());
    }
    assert_eq!(stream.last_ack().unwrap().0, 3);
    local.until(|s| s.text().contains("xyzw")).await;
    assert!(local.last_ack().is_none() && local.modes().is_empty(), "{:?}", local.data);
    block.send(BlockMsg::Shutdown);
}
