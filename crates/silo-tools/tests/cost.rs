//! The O5 cost gate (docs/OTTO.md §6.2): a parked command block against a
//! pipe for the same command. Ignored; run it on purpose:
//! `cargo nextest run -E 'package(silo-tools) & test(/cost_gate/)' --run-ignored only --no-capture`.

use std::time::{Duration, Instant};

use silo_client::{Control, transport::RelayConfig};
use silo_proto::{
    control::{Capture, Created, Event, Method, SpawnSpec},
    model::{BlockKind, Lifecycle},
    paths,
};

const RUNS: usize = 15;

fn shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
}

async fn pipe(cwd: &str, command: &str) -> (Duration, Duration) {
    use tokio::io::AsyncReadExt;
    let start = Instant::now();
    let mut child = tokio::process::Command::new(shell())
        .args(["-lc", command])
        .current_dir(cwd)
        .env("PAGER", "cat")
        .env("GIT_PAGER", "cat")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut buf = vec![0; 64 * 1024];
    let mut first = None;
    let mut out = Vec::new();
    loop {
        let n = stdout.read(&mut buf).await.unwrap();
        if n == 0 {
            break;
        }
        first.get_or_insert_with(|| start.elapsed());
        out.extend_from_slice(&buf[..n]);
    }
    child.wait().await.unwrap();
    let total = start.elapsed();
    (first.unwrap_or(total), total)
}

async fn block(control: &Control, events: &mut silo_client::Events, owner: &str, cwd: &str, command: &str) -> (Duration, Duration) {
    let start = Instant::now();
    let created: Created = control
        .call(Method::BlockSpawn {
            block_id: owner.into(),
            command: vec![shell(), "-lc".into(), command.into()],
            cwd: Some(cwd.into()),
            env: [("PAGER".to_owned(), "cat".to_owned()), ("GIT_PAGER".to_owned(), "cat".to_owned())].into(),
            size: None,
            kind: BlockKind::Terminal,
            url: None,
        })
        .await
        .unwrap();
    let spawned = start.elapsed();
    loop {
        let (_, event) = tokio::time::timeout(Duration::from_secs(120), events.recv()).await.unwrap().unwrap();
        if let Event::BlockChanged { block } = event
            && block.id == created.block_id
            && block.lifecycle == Lifecycle::Exited
        {
            break;
        }
    }
    let capture: Capture = control.call(Method::TerminalCapture { block_id: created.block_id, history: true }).await.unwrap();
    assert_eq!(capture.lifecycle, Lifecycle::Exited);
    (spawned, start.elapsed())
}

fn pct(mut v: Vec<Duration>, p: usize) -> f64 {
    v.sort();
    v[(v.len() - 1) * p / 100].as_secs_f64() * 1000.0
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "measurement, not a test"]
async fn cost_gate() {
    let home = tempfile::Builder::new().prefix("silo-cost-").tempdir_in("/tmp").unwrap();
    let config = silo_server::Config { home: home.path().into(), idle_exit: None, relay: RelayConfig::Disabled, mdns: false, ..Default::default() };
    let server = tokio::spawn(async move { silo_server::run(config).await.unwrap() });
    while !silo_client::server_running(home.path()).await {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let (control, mut events) = Control::connect(&paths::socket(home.path()), "cost-gate").await.unwrap();
    control.subscribe().await.unwrap();
    let repo = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let main: Created =
        control.call(Method::SessionCreate { name: None, spawn: SpawnSpec::default(), kind: BlockKind::Terminal, url: None }).await.unwrap();
    let cases = [
        ("true", "true", RUNS),
        ("echo", "echo hi", RUNS),
        ("git status", "git status", RUNS),
        ("git grep (rg stand-in)", "git grep -n 'fn spawn_parked' -- crates", RUNS),
        ("cargo check (no-op)", "cargo check -q --offline -p silo-proto", 6),
    ];
    // Warm caches (and build the no-op check once).
    for (_, command, _) in &cases {
        pipe(repo, command).await;
    }
    println!("| command | pipe p50 | block p50 | overhead p50 | pipe p90 | block p90 | spawn call p50 |");
    for (name, command, runs) in cases {
        let (mut pipes, mut blocks, mut spawns) = (Vec::new(), Vec::new(), Vec::new());
        for _ in 0..runs {
            pipes.push(pipe(repo, command).await.1);
            let (spawned, total) = block(&control, &mut events, &main.block_id, repo, command).await;
            blocks.push(total);
            spawns.push(spawned);
        }
        let (p, b) = (pct(pipes.clone(), 50), pct(blocks.clone(), 50));
        println!("| {name} | {p:.1} | {b:.1} | {:.1} | {:.1} | {:.1} | {:.1} |", b - p, pct(pipes, 90), pct(blocks, 90), pct(spawns, 50));
    }
    let mut first = Vec::new();
    for _ in 0..RUNS {
        first.push(pipe(repo, "echo hi").await.0);
    }
    println!("pipe spawn-to-first-byte p50 {:.1} ms", pct(first, 50));
    control.call_value(Method::ServerShutdown { force: true }).await.unwrap();
    let _ = tokio::time::timeout(Duration::from_secs(10), server).await;
}
