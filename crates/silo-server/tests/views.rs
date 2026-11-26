//! Each app's own view of a session (docs/PLAN.md §3.8): its shown tab,
//! focused block and zoom are its own; the tree is shared.
//!
//! One test: the view id a hello carries is process-wide in `silo_client`.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use silo_client::{Control, Events};
use silo_proto::{
    control::{ClientRole, Created, Event, Method},
    model::{Axis, SessionView},
    paths,
};
use silo_transport::RelayConfig;
use tokio::{task::JoinHandle, time::timeout};

const T: Duration = Duration::from_secs(10);

async fn start(home: &Path) -> JoinHandle<()> {
    let config = silo_server::Config { home: home.to_owned(), idle_exit: None, relay: RelayConfig::Disabled, mdns: false, ..Default::default() };
    let task = tokio::spawn(async move { silo_server::run(config).await.unwrap() });
    for _ in 0..500 {
        if silo_client::server_running(home).await {
            return task;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("server did not start");
}

/// An app with its own view `view`, or (`None`) a CLI without one.
async fn connect(home: &Path, view: Option<&str>) -> (Control, Events) {
    silo_client::set_role(if view.is_some() { ClientRole::App } else { ClientRole::Cli });
    silo_client::set_view(view.map(str::to_owned));
    let (control, events) = Control::connect(&paths::socket(home), "views-test").await.unwrap();
    silo_client::set_role(ClientRole::Cli);
    silo_client::set_view(None);
    control.subscribe().await.unwrap();
    (control, events)
}

/// What `control` is shown of `session`: its tab, that tab's focused block
/// and whether it is zoomed.
async fn shown(control: &Control, session: &str) -> (String, String, bool) {
    let view: SessionView = control.call(Method::SessionView { session_id: session.to_owned() }).await.unwrap();
    looks(&view)
}

fn looks(view: &SessionView) -> (String, String, bool) {
    let window = view.session.windows.iter().find(|w| w.id == view.session.active_window).unwrap();
    (window.id.clone(), window.focused.clone(), window.zoomed)
}

/// The `session.changed` carrying `revision`, as this client was sent it.
async fn changed(events: &mut Events, revision: u64) -> SessionView {
    timeout(T, async {
        loop {
            if let Some((_, Event::SessionChanged { view })) = events.recv().await
                && view.session.revision >= revision
            {
                return view;
            }
        }
    })
    .await
    .expect("session.changed")
}

fn spawn() -> silo_proto::control::SpawnSpec {
    silo_proto::control::SpawnSpec { command: vec!["/bin/cat".into()], cwd: Some("/tmp".into()), size: None }
}

fn split(block: &str) -> Method {
    Method::BlockSplit {
        block_id: block.to_owned(),
        axis: Axis::Horizontal,
        spawn: spawn(),
        kind: Default::default(),
        url: None,
        agent: None,
        ids: None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn each_app_has_its_own_tab_focus_and_zoom() {
    let home = PathBuf::from(format!("/tmp/silo-views-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let task = start(&home).await;
    let at = |window: &str, block: &str, zoomed: bool| (window.to_owned(), block.to_owned(), zoomed);

    let (a, mut a_events) = connect(&home, Some("mac-a")).await;
    let first: Created = a.call(Method::SessionCreate { name: None, spawn: spawn(), kind: Default::default(), url: None }).await.unwrap();
    let (s, w1, b1) = (first.session_id.as_str(), first.window_id.as_str(), first.block_id.as_str());
    let second: Created = a.call(split(b1)).await.unwrap();
    let b2 = second.block_id.as_str();
    assert_eq!(shown(&a, s).await, at(w1, b2, false), "a split focuses the new block for whoever split");

    // A second app starts where the last person to act is.
    let (b, mut b_events) = connect(&home, Some("mac-b")).await;
    assert_eq!(shown(&b, s).await, at(w1, b2, false));

    // Its click is its own: the first app's focus stays, and the event it
    // is sent says so (an app follows the focus it is told).
    b.call_value(Method::BlockFocus { block_id: b1.to_owned() }).await.unwrap();
    assert_eq!(shown(&b, s).await, at(w1, b1, false));
    assert_eq!(shown(&a, s).await, at(w1, b2, false));
    let view: SessionView = b.call(Method::SessionView { session_id: s.to_owned() }).await.unwrap();
    let revision = view.session.revision;
    assert_eq!(looks(&changed(&mut b_events, revision).await), at(w1, b1, false));
    assert_eq!(looks(&changed(&mut a_events, revision).await), at(w1, b2, false));

    // So is its zoom, and the first app's new tab.
    b.call_value(Method::BlockZoom { block_id: b1.to_owned(), zoomed: true }).await.unwrap();
    let tab: Created = a
        .call(Method::WindowCreate { session_id: s.to_owned(), spawn: spawn(), kind: Default::default(), url: None, background: false, ids: None })
        .await
        .unwrap();
    let (w2, b3) = (tab.window_id.as_str(), tab.block_id.as_str());
    assert_eq!(shown(&a, s).await, at(w2, b3, false));
    assert_eq!(shown(&b, s).await, at(w1, b1, true));
    // The tree is shared: both have the new tab.
    let view: SessionView = b.call(Method::SessionView { session_id: s.to_owned() }).await.unwrap();
    assert_eq!(view.session.windows.len(), 2);

    // A client without a view (the CLI, a script) sees and moves the view
    // of the app used last: the second, which focused a block.
    let (cli, _) = connect(&home, None).await;
    assert_eq!(shown(&cli, s).await, at(w1, b1, true));
    cli.call_value(Method::BlockFocus { block_id: b2.to_owned() }).await.unwrap();
    assert_eq!(shown(&b, s).await, at(w1, b2, true));
    assert_eq!(shown(&a, s).await, at(w2, b3, false));

    // A block closed by one app: the other, focused there, goes back to
    // the block it focused before, not to where the closer is.
    a.call_value(Method::BlockClose { block_id: b2.to_owned(), force: true }).await.unwrap();
    assert_eq!(shown(&b, s).await, at(w1, b1, false), "one block left: not zoomed");
    assert_eq!(shown(&a, s).await, at(w2, b3, false));

    // The same app back on a new connection is where it was; a new one
    // starts where the last person to act is.
    drop((b, b_events));
    let (b, _) = connect(&home, Some("mac-b")).await;
    assert_eq!(shown(&b, s).await, at(w1, b1, false));
    let (c, _) = connect(&home, Some("mac-c")).await;
    assert_eq!(shown(&c, s).await, at(w2, b3, false));

    // Views are saved with the tree: after a restart each app is where it was.
    a.call_value(Method::ServerShutdown { force: true }).await.unwrap();
    let _ = timeout(T, task).await;
    drop((a, a_events, b, c, cli));
    let task = start(&home).await;
    let (a, _a_events) = connect(&home, Some("mac-a")).await;
    let (b, _) = connect(&home, Some("mac-b")).await;
    assert_eq!(shown(&a, s).await, at(w2, b3, false));
    assert_eq!(shown(&b, s).await, at(w1, b1, false));

    // The tab an app shows closes under it: it moves to where the closer is.
    a.call_value(Method::WindowSelect { session_id: s.to_owned(), window_id: w1.to_owned() }).await.unwrap();
    a.call_value(Method::WindowClose { window_id: w1.to_owned(), force: true }).await.unwrap();
    assert_eq!(shown(&b, s).await, at(w2, b3, false));

    a.call_value(Method::ServerShutdown { force: true }).await.unwrap();
    let _ = timeout(T, task).await;
    let _ = std::fs::remove_dir_all(&home);
}
