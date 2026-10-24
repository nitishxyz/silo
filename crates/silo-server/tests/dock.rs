//! `block.move` with `dock` on tiled blocks: any side of another block of
//! the window (feature `dock_sides`), and its refusals.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use silo_client::Control;
use silo_proto::{
    control::{Created, Dock, Method, RpcError, SpawnSpec},
    model::{Axis, BlockKind, Layout, SessionView, TermSize},
    paths,
};

const TIMEOUT: Duration = Duration::from_secs(15);

fn home(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/silo-dock-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

async fn start(home: &Path) -> (Control, tokio::task::JoinHandle<anyhow::Result<()>>) {
    let config =
        silo_server::Config { home: home.to_owned(), idle_exit: None, relay: silo_server::RelayConfig::Disabled, mdns: false, ..Default::default() };
    let task = tokio::spawn(silo_server::run(config));
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        if let Ok((control, _)) = Control::connect(&paths::socket(home), "dock-test").await {
            return (control, task);
        }
        assert!(tokio::time::Instant::now() < deadline, "no server");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn code(error: &anyhow::Error) -> String {
    error.downcast_ref::<RpcError>().map_or_else(|| format!("{error:#}"), |e| e.code.clone())
}

fn shell() -> SpawnSpec {
    SpawnSpec { command: vec!["/bin/sh".into()], cwd: Some("/tmp".into()), size: Some(TermSize::new(80, 24)) }
}

async fn view(control: &Control, session: &str) -> SessionView {
    control.call(Method::SessionView { session_id: session.into() }).await.unwrap()
}

fn dock(block: &str, target: &str, axis: Axis, before: bool) -> Method {
    Method::BlockMove { block_id: block.into(), rect: None, dock: Some(Dock { target: target.into(), axis, before }) }
}

#[tokio::test(flavor = "multi_thread")]
async fn tiled_blocks_dock_on_any_side_of_another() {
    let home = home("sides");
    let (control, task) = start(&home).await;
    assert!(control.peer_supports(silo_proto::compat::feature::DOCK_SIDES));
    let a: Created = control.call(Method::SessionCreate { name: None, spawn: shell(), kind: BlockKind::Terminal, url: None }).await.unwrap();
    let split =
        |block: &str, axis| Method::BlockSplit { block_id: block.into(), axis, spawn: shell(), kind: BlockKind::Terminal, url: None, agent: None };
    let b: Created = control.call(split(&a.block_id, Axis::Horizontal)).await.unwrap();
    let c: Created = control.call(split(&b.block_id, Axis::Horizontal)).await.unwrap();
    let session = a.session_id.clone();
    let (a, b, c) = (a.block_id, b.block_id, c.block_id);
    let layout = async |control: &Control| view(control, &session).await.session.windows[0].clone();

    // a | b | c: drag c onto the bottom of a, giving (a / c) | b.
    control.call_value(dock(&c, &a, Axis::Vertical, false)).await.unwrap();
    let w = layout(&control).await;
    let Layout::Split { axis: Axis::Horizontal, first, second, .. } = &w.layout else { panic!("{:?}", w.layout) };
    assert!(matches!(&**first, Layout::Split { axis: Axis::Vertical, .. }), "{first:?}");
    assert_eq!((first.blocks(), second.blocks()), (vec![a.clone(), c.clone()], vec![b.clone()]));
    assert_eq!(w.focused, c, "the moved block is focused");

    // The left of b, then above a.
    control.call_value(dock(&c, &b, Axis::Horizontal, true)).await.unwrap();
    assert_eq!(layout(&control).await.layout.blocks(), [a.clone(), c.clone(), b.clone()]);
    control.call_value(Method::BlockZoom { block_id: b.clone(), zoomed: true }).await.unwrap();
    control.call_value(dock(&b, &a, Axis::Vertical, true)).await.unwrap();
    let w = layout(&control).await;
    assert_eq!((w.layout.blocks(), w.zoomed, w.focused.as_str()), (vec![b.clone(), a.clone(), c.clone()], false, b.as_str()));

    // Itself, a block of another window and unknown blocks are refused.
    let other: Created =
        control.call(Method::WindowCreate { session_id: session.clone(), spawn: shell(), kind: BlockKind::Terminal, url: None }).await.unwrap();
    for (block, target) in [(&a, &a), (&a, &other.block_id), (&other.block_id, &a)] {
        let err = control.call_value(dock(block, target, Axis::Vertical, true)).await.unwrap_err();
        assert_eq!(code(&err), "bad_request", "{block} onto {target}");
    }
    assert_eq!(code(&control.call_value(dock("nope", &a, Axis::Vertical, true)).await.unwrap_err()), "not_found");
    assert_eq!(layout(&control).await.layout.blocks(), [b.clone(), a.clone(), c.clone()]);

    // The old shape (no `before`) docks right or below.
    let old = serde_json::json!({"block_id": a, "dock": {"target": c, "axis": "horizontal"}});
    control.call_value(serde_json::from_value::<Method>(serde_json::json!({"method": "block.move", "params": old})).unwrap()).await.unwrap();
    assert_eq!(layout(&control).await.layout.blocks(), [b, c, a]);

    control.call_value(Method::ServerShutdown { force: true }).await.unwrap();
    drop(control);
    tokio::time::timeout(TIMEOUT, task).await.unwrap().unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&home);
}
