//! Parked blocks: `block.spawn`, `block.park`, `block.reveal`, command
//! blocks closing on exit with their final capture, owners closing them.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use silo_client::Control;
use silo_proto::{
    control::{BlockView, Capture, Created, Method, Reveal, RpcError, SpawnSpec},
    model::{Axis, BlockKind, Lifecycle, SessionView, TermSize},
    paths,
};

const TIMEOUT: Duration = Duration::from_secs(15);

fn home(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/silo-parked-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

async fn start(home: &Path) -> (Control, tokio::task::JoinHandle<anyhow::Result<()>>) {
    let config =
        silo_server::Config { home: home.to_owned(), idle_exit: None, relay: silo_server::RelayConfig::Disabled, mdns: false, ..Default::default() };
    let task = tokio::spawn(silo_server::run(config));
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        if let Ok((control, _)) = Control::connect(&paths::socket(home), "parked-test").await {
            return (control, task);
        }
        assert!(tokio::time::Instant::now() < deadline, "no server");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn code(error: &anyhow::Error) -> String {
    error.downcast_ref::<RpcError>().map_or_else(|| format!("{error:#}"), |e| e.code.clone())
}

async fn view(control: &Control, session: &str) -> SessionView {
    control.call(Method::SessionView { session_id: session.into() }).await.unwrap()
}

async fn capture(control: &Control, block: &str) -> Capture {
    control.call(Method::TerminalCapture { block_id: block.into(), history: true }).await.unwrap()
}

/// Poll `block`'s capture until `done` holds.
async fn until(control: &Control, block: &str, done: impl Fn(&Capture) -> bool) -> Capture {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        let capture = capture(control, block).await;
        if done(&capture) {
            return capture;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out: {capture:?}");
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

fn spawn(owner: &str, command: &[&str]) -> Method {
    Method::BlockSpawn {
        block_id: owner.into(),
        command: command.iter().map(|&c| c.to_owned()).collect(),
        cwd: Some("/tmp".into()),
        env: [("SILO_PARKED_TEST".to_owned(), "env-ok".to_owned())].into(),
        size: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn parked_blocks_spawn_reveal_park_and_close() {
    let home = home("all");
    let (control, task) = start(&home).await;
    let shell = SpawnSpec { command: vec!["/bin/sh".into()], cwd: Some("/tmp".into()), size: Some(TermSize::new(80, 24)) };
    let main: Created = control.call(Method::SessionCreate { name: None, spawn: shell, kind: BlockKind::Terminal, url: None }).await.unwrap();
    let split = Method::BlockSplit {
        block_id: main.block_id.clone(),
        axis: Axis::Horizontal,
        spawn: SpawnSpec::default(),
        kind: BlockKind::Agent,
        url: None,
        agent: None,
    };
    let agent: Created = control.call(split).await.unwrap();

    // A command block: parked, owned, wide, with its env; it reads a line.
    let script = "echo \"$SILO_PARKED_TEST\"; printf 'name? '; read x; echo \"got $x\"; exit 4";
    let run: Created = control.call(spawn(&agent.block_id, &["/bin/sh", "-c", script])).await.unwrap();
    assert_eq!((run.session_id.as_str(), run.window_id.as_str()), (main.session_id.as_str(), ""));
    let v = view(&control, &main.session_id).await;
    assert_eq!(v.session.parked.len(), 1);
    assert_eq!(v.session.parked[0].owner.as_deref(), Some(agent.block_id.as_str()));
    assert!(v.session.windows.iter().all(|w| !w.layout.contains(&run.block_id)));
    let info = v.blocks.iter().find(|b| b.id == run.block_id).expect("parked blocks are in the view");
    assert_eq!(info.size.columns, 200);
    let prompt = until(&control, &run.block_id, |c| c.line_prompt).await;
    assert!(prompt.text.contains("env-ok") && prompt.text.ends_with("name?"), "{prompt:?}");
    assert_eq!(prompt.lifecycle, Lifecycle::Running);

    // It exits: closed at once, its final capture stays readable.
    control.call_value(Method::TerminalSend { block_id: run.block_id.clone(), text: "bob\r".into() }).await.unwrap();
    let ended = until(&control, &run.block_id, |c| c.lifecycle == Lifecycle::Exited).await;
    assert_eq!(ended.exited, Some(4));
    assert!(ended.text.contains("got bob"), "{ended:?}");
    let gone = control.call::<BlockView>(Method::BlockView { block_id: run.block_id.clone() }).await.unwrap_err();
    assert_eq!(code(&gone), "not_found");
    assert!(view(&control, &main.session_id).await.session.parked.is_empty());

    // Revealed, a command block stays as an exited block.
    let kept: Created = control.call(spawn(&agent.block_id, &["/bin/sh", "-c", "read x; exit 2"])).await.unwrap();
    control.call_value(Method::BlockReveal { block_id: kept.block_id.clone(), reveal: Reveal::Float }).await.unwrap();
    let v = view(&control, &main.session_id).await;
    assert!(v.session.parked.is_empty());
    assert_eq!(v.session.windows[0].floats.last().map(|f| f.id.as_str()), Some(kept.block_id.as_str()));
    assert_eq!(v.session.windows[0].focused, kept.block_id);
    control.call_value(Method::TerminalSend { block_id: kept.block_id.clone(), text: "\r".into() }).await.unwrap();
    let ended = until(&control, &kept.block_id, |c| c.lifecycle == Lifecycle::Exited).await;
    assert_eq!(ended.exited, Some(2));
    let view_of: BlockView = control.call(Method::BlockView { block_id: kept.block_id.clone() }).await.unwrap();
    assert_eq!(view_of.block.lifecycle, Lifecycle::Exited);

    // Park and reveal as a split; the only block of a window stays.
    control.call_value(Method::BlockPark { block_id: kept.block_id.clone() }).await.unwrap();
    let v = view(&control, &main.session_id).await;
    assert_eq!(v.session.parked.iter().map(|p| (p.id.as_str(), p.owner.clone())).collect::<Vec<_>>(), [(kept.block_id.as_str(), None)]);
    control.call_value(Method::BlockReveal { block_id: kept.block_id.clone(), reveal: Reveal::Split }).await.unwrap();
    assert!(view(&control, &main.session_id).await.session.windows[0].layout.contains(&kept.block_id));
    let revealed = control.call_value(Method::BlockReveal { block_id: kept.block_id.clone(), reveal: Reveal::Float }).await.unwrap_err();
    assert_eq!(code(&revealed), "bad_request");
    let lone: Created = control
        .call(Method::WindowCreate { session_id: main.session_id.clone(), spawn: SpawnSpec::default(), kind: BlockKind::Terminal, url: None })
        .await
        .unwrap();
    assert_eq!(code(&control.call_value(Method::BlockPark { block_id: lone.block_id }).await.unwrap_err()), "conflict");

    // Closing the owner closes its parked blocks, not revealed ones.
    let hidden: Created = control.call(spawn(&agent.block_id, &[])).await.unwrap();
    control.call_value(Method::BlockClose { block_id: agent.block_id.clone(), force: true }).await.unwrap();
    let v = view(&control, &main.session_id).await;
    assert!(v.session.parked.is_empty() && v.blocks.iter().all(|b| b.id != hidden.block_id));
    assert!(v.blocks.iter().any(|b| b.id == kept.block_id));

    control.call_value(Method::ServerShutdown { force: true }).await.unwrap();
    drop(control);
    tokio::time::timeout(TIMEOUT, task).await.unwrap().unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&home);
}
