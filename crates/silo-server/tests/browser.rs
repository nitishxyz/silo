//! Browser blocks: server-held state, commands, owner reports, persistence
//! and live upgrade.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use silo_client::{Control, Events};
use silo_proto::{
    control::{BrowserCommand, Created, Event, Method, OwnMode, RpcError, SpawnSpec},
    model::{Axis, BlockInfo, BlockKind, BrowserState, HistoryEntry, TermSize},
    paths,
};
use tokio::task::JoinHandle;

const TIMEOUT: Duration = Duration::from_secs(15);

fn temp_home(name: &str) -> PathBuf {
    let dir = PathBuf::from(format!("/tmp/silo-browser-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn config(home: &Path) -> silo_server::Config {
    let interval = Duration::from_millis(200);
    silo_server::Config {
        home: home.to_owned(),
        idle_exit: None,
        checkpoint_interval: interval,
        relay: silo_server::RelayConfig::Disabled,
        mdns: false,
        ..Default::default()
    }
}

async fn start(home: &Path) -> JoinHandle<anyhow::Result<()>> {
    let task = tokio::spawn(silo_server::run(config(home)));
    for _ in 0..300 {
        if silo_client::server_running(home).await {
            return task;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("server did not start");
}

async fn connect(home: &Path) -> (Control, Events) {
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    loop {
        if let Ok(pair) = Control::connect(&paths::socket(home), "browser-test").await {
            return pair;
        }
        assert!(tokio::time::Instant::now() < deadline, "no server");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn stop(control: Control, task: JoinHandle<anyhow::Result<()>>) {
    control.call_value(Method::ServerShutdown {}).await.unwrap();
    drop(control);
    tokio::time::timeout(TIMEOUT, task).await.unwrap().unwrap().unwrap();
}

/// A session whose terminal is split with a browser at `url`.
async fn browser(control: &Control, url: &str) -> Created {
    let spawn = SpawnSpec { command: vec!["/bin/sh".into()], cwd: Some("/tmp".into()), size: Some(TermSize::new(80, 24)) };
    let main: Created = control.call(Method::SessionCreate { name: None, spawn, kind: BlockKind::Terminal, url: None }).await.unwrap();
    let split = Method::BlockSplit {
        block_id: main.block_id,
        axis: Axis::Horizontal,
        spawn: SpawnSpec::default(),
        kind: BlockKind::Browser,
        url: Some(url.into()),
    };
    control.call(split).await.unwrap()
}

async fn info(control: &Control, created: &Created) -> BlockInfo {
    let view: silo_proto::model::SessionView = control.call(Method::SessionView { session_id: created.session_id.clone() }).await.unwrap();
    view.blocks.into_iter().find(|b| b.id == created.block_id).expect("browser block")
}

async fn state(control: &Control, created: &Created) -> BrowserState {
    info(control, created).await.browser.expect("browser state")
}

fn code(error: &anyhow::Error) -> &str {
    &error.downcast_ref::<RpcError>().unwrap_or_else(|| panic!("not an rpc error: {error:#}")).code
}

fn report(created: &Created, urls: &[&str], index: usize) -> Method {
    let history = urls.iter().map(|u| HistoryEntry { url: (*u).into(), title: format!("t-{u}") }).collect();
    let url = urls.get(index).copied().unwrap_or_default();
    Method::BrowserReport {
        block_id: created.block_id.clone(),
        url: url.into(),
        title: format!("t-{url}"),
        favicon: None,
        loading: false,
        history,
        index,
    }
}

/// The next browser command event.
async fn next_command(events: &mut Events) -> (String, BrowserCommand) {
    loop {
        let (_, event) = tokio::time::timeout(TIMEOUT, events.recv()).await.unwrap().expect("events");
        if let Event::BrowserCommand { block_id, command } = event {
            return (block_id, command);
        }
    }
}

#[tokio::test]
async fn split_creates_browser_and_commands_edit_history() {
    let home = temp_home("commands");
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let (viewer, mut events) = connect(&home).await;
    viewer.subscribe().await.unwrap();
    let created = browser(&control, "example.com").await;

    let block = info(&control, &created).await;
    assert_eq!((block.kind, block.size, block.pid, block.owner), (BlockKind::Browser, TermSize::default(), None, None));
    let s = block.browser.unwrap();
    assert_eq!((s.url.as_str(), s.history.len(), s.index, s.profile.as_str()), ("https://example.com", 1, 0, "default"));

    let id = created.block_id.clone();
    control.call_value(Method::BrowserNavigate { block_id: id.clone(), url: "localhost:3000/a".into() }).await.unwrap();
    assert_eq!(next_command(&mut events).await, (id.clone(), BrowserCommand::Navigate { url: "http://localhost:3000/a".into() }));
    let s = state(&control, &created).await;
    assert_eq!((s.url.as_str(), s.history.len(), s.index), ("http://localhost:3000/a", 2, 1));

    control.call_value(Method::BrowserBack { block_id: id.clone() }).await.unwrap();
    assert_eq!(next_command(&mut events).await, (id.clone(), BrowserCommand::Back));
    assert_eq!(state(&control, &created).await.url, "https://example.com");
    let error = control.call_value(Method::BrowserBack { block_id: id.clone() }).await.unwrap_err();
    assert_eq!(code(&error), "bad_request");
    for url in ["file:///etc/passwd", "chrome://settings", "devtools://devtools", "javascript:alert(1)"] {
        let error = control.call_value(Method::BrowserNavigate { block_id: id.clone(), url: url.into() }).await.unwrap_err();
        assert_eq!(code(&error), "bad_request", "{url}");
    }
    let split = Method::BlockSplit {
        block_id: id.clone(),
        axis: Axis::Vertical,
        spawn: SpawnSpec::default(),
        kind: BlockKind::Browser,
        url: Some("file:///".into()),
    };
    assert_eq!(code(&control.call_value(split).await.unwrap_err()), "bad_request");
    assert_eq!(state(&control, &created).await.url, "https://example.com", "rejected urls change nothing");
    control.call_value(Method::BrowserForward { block_id: id.clone() }).await.unwrap();
    assert_eq!(next_command(&mut events).await.1, BrowserCommand::Forward);
    assert_eq!(state(&control, &created).await.index, 1);

    control.call_value(Method::BrowserReload { block_id: id.clone(), ignore_cache: true }).await.unwrap();
    assert_eq!(next_command(&mut events).await.1, BrowserCommand::Reload { ignore_cache: true });
    control.call_value(Method::BrowserZoom { block_id: id.clone(), zoom: 1.5 }).await.unwrap();
    assert_eq!(next_command(&mut events).await.1, BrowserCommand::Zoom { zoom: 1.5 });
    assert!((state(&control, &created).await.zoom - 1.5).abs() < f32::EPSILON);

    // A new browser window.
    let window = Method::WindowCreate { session_id: created.session_id.clone(), spawn: SpawnSpec::default(), kind: BlockKind::Browser, url: None };
    let window: Created = control.call(window).await.unwrap();
    assert_eq!(state(&control, &window).await.url, "about:blank");
    let session = Method::SessionCreate { name: None, spawn: SpawnSpec::default(), kind: BlockKind::Browser, url: Some("10.0.0.2:8080".into()) };
    let session: Created = control.call(session).await.unwrap();
    assert_eq!(state(&control, &session).await.url, "http://10.0.0.2:8080");

    // Terminal-only methods and data streams are refused.
    let error = control.call_value(Method::DataConnect { block_id: id.clone() }).await.unwrap_err();
    assert_eq!(code(&error), "bad_request");
    let error = control.call_value(Method::TerminalSend { block_id: id.clone(), text: "x".into() }).await.unwrap_err();
    assert_eq!(code(&error), "bad_request");
    let main = info(&control, &created).await.session_id;
    let error = control.call_value(Method::BrowserBack { block_id: main }).await.unwrap_err();
    assert_eq!(code(&error), "not_found");

    drop(viewer);
    stop(control, task).await;
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test]
async fn report_is_owner_only() {
    let home = temp_home("report");
    let task = start(&home).await;
    let (a, _) = connect(&home).await;
    let (b, _) = connect(&home).await;
    let created = browser(&a, "example.com").await;
    let ids = vec![created.block_id.clone()];

    let error = a.call_value(report(&created, &["https://x.test"], 0)).await.unwrap_err();
    assert!(error.downcast_ref::<silo_client::Unauthorized>().is_some(), "unowned: {error:#}");
    a.call_value(Method::OwnBlock { block_ids: ids.clone(), mode: OwnMode::Take }).await.unwrap();
    assert_eq!(info(&a, &created).await.owner, Some(a.client_id()));
    let error = b.call_value(report(&created, &["https://x.test"], 0)).await.unwrap_err();
    assert!(error.downcast_ref::<silo_client::Unauthorized>().is_some(), "non-owner: {error:#}");

    a.call_value(report(&created, &["https://x.test", "https://y.test"], 1)).await.unwrap();
    let block = info(&b, &created).await;
    let s = block.browser.unwrap();
    assert_eq!((block.title.as_str(), s.url.as_str(), s.index, s.history.len()), ("t-https://y.test", "https://y.test", 1, 2));
    let error = a.call_value(report(&created, &["https://x.test"], 1)).await.unwrap_err();
    assert_eq!(code(&error), "bad_request");
    let long = format!("https://x.test/{}", "a".repeat(8 * 1024));
    let long_title = Method::BrowserReport {
        block_id: created.block_id.clone(),
        url: "https://x.test".into(),
        title: "t".repeat(8 * 1024 + 1),
        favicon: None,
        loading: false,
        history: vec![HistoryEntry { url: "https://x.test".into(), title: String::new() }],
        index: 0,
    };
    for method in [report(&created, &[&long], 0), report(&created, &["https://x.test", &long], 0), long_title] {
        assert_eq!(code(&a.call_value(method).await.unwrap_err()), "bad_request");
    }
    assert_eq!(state(&b, &created).await.url, "https://y.test", "rejected reports change nothing");
    // Long histories keep the 50 entries nearest the current one.
    let urls: Vec<String> = (0..60).map(|i| format!("https://p{i}.test")).collect();
    let urls: Vec<&str> = urls.iter().map(String::as_str).collect();
    for (index, first, kept) in [(59, "https://p10.test", 49), (5, "https://p0.test", 5), (30, "https://p0.test", 30)] {
        a.call_value(report(&created, &urls, index)).await.unwrap();
        let s = state(&b, &created).await;
        assert_eq!((s.history.len(), s.history[0].url.as_str(), s.index), (50, first, kept));
        assert_eq!(s.history[s.index].url, urls[index]);
    }

    b.call_value(Method::OwnBlock { block_ids: ids.clone(), mode: OwnMode::Request }).await.unwrap();
    assert_eq!(info(&b, &created).await.owner, Some(a.client_id()), "request leaves an owner in place");
    drop(a);
    let deadline = tokio::time::Instant::now() + TIMEOUT;
    while info(&b, &created).await.owner.is_some() {
        assert!(tokio::time::Instant::now() < deadline, "owner kept after disconnect");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    stop(b, task).await;
    let _ = std::fs::remove_dir_all(&home);
}

/// Owned, navigated state for the persistence tests.
async fn prepare(control: &Control) -> (Created, BrowserState) {
    let created = browser(control, "example.com").await;
    control.call_value(Method::BrowserNavigate { block_id: created.block_id.clone(), url: "example.org".into() }).await.unwrap();
    control.call_value(Method::BrowserZoom { block_id: created.block_id.clone(), zoom: -1.0 }).await.unwrap();
    control.call_value(Method::OwnBlock { block_ids: vec![created.block_id.clone()], mode: OwnMode::Take }).await.unwrap();
    // A link followed in the page: only the owner's report knows it.
    let urls = ["https://example.com/", "https://example.org/", "https://example.org/linked"];
    control.call_value(report(&created, &urls, 2)).await.unwrap();
    let state = state(control, &created).await;
    assert_eq!(state.url, "https://example.org/linked");
    (created, state)
}

#[tokio::test]
async fn state_survives_restart() {
    let home = temp_home("persist");
    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let (created, before) = prepare(&control).await;
    stop(control, task).await;

    let task = start(&home).await;
    let (control, _) = connect(&home).await;
    let block = info(&control, &created).await;
    assert_eq!((block.kind, block.owner), (BlockKind::Browser, None));
    assert_eq!(block.browser.unwrap(), before);
    stop(control, task).await;
    let _ = std::fs::remove_dir_all(&home);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn state_survives_takeover() {
    let home = temp_home("takeover");
    let old = start(&home).await;
    let (control, _) = connect(&home).await;
    let (created, before) = prepare(&control).await;

    let new = tokio::spawn(silo_server::takeover(config(&home), false));
    tokio::time::timeout(TIMEOUT, old).await.unwrap().unwrap().expect("old server commits and exits");
    drop(control);
    let (control, _) = connect(&home).await;
    let block = info(&control, &created).await;
    assert_eq!((block.kind, block.owner), (BlockKind::Browser, None));
    assert_eq!(block.browser.unwrap(), before);
    control.call_value(Method::BrowserBack { block_id: created.block_id.clone() }).await.unwrap();
    assert_eq!(state(&control, &created).await.url, "https://example.org/");
    stop(control, new).await;
    let _ = std::fs::remove_dir_all(&home);
}
