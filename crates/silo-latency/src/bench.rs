//! Headless keystroke bench: an in-process server, a client paired over the
//! delay proxy, a real `Predictor`, and per-key `key→pred` / `key→echo`.

use std::{
    fmt::Write as _,
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, bail};
use serde::Serialize;
use silo_client::{
    Control, Predictor,
    proto::{
        control::{Method, PairInvite},
        paths,
    },
    transport::{EndpointAddr, Net, NetOptions, PairTicket, RelayConfig},
};
use tokio::{task::JoinHandle, time::timeout};

use crate::keys::{Key, Session};
use crate::proxy::{Proxy, ProxyConfig};

pub(crate) const T: Duration = Duration::from_secs(10);
/// Pause between keystrokes so each key is measured on a quiet link.
const KEY_GAP: Duration = Duration::from_millis(20);
/// Keys per line: Enter before the cursor gets near the margin.
const LINE: usize = 60;
/// The backspace burst typed after the letters.
const BURST: &[u8] = b"xyz\x7f\x7f\x7f";

/// Percentiles in milliseconds.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Stats {
    pub n: usize,
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
}

impl Stats {
    pub fn from_samples(samples: &[Duration]) -> Stats {
        let mut ms: Vec<f64> = samples.iter().map(|d| d.as_secs_f64() * 1e3).collect();
        ms.sort_by(f64::total_cmp);
        let at = |q: f64| ms.get(((ms.len() as f64 * q).ceil() as usize).saturating_sub(1)).copied().unwrap_or(0.0);
        Stats { n: ms.len(), p50: at(0.50), p95: at(0.95), p99: at(0.99) }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Row {
    /// `"unix"` for the local baseline, else `"<rtt>ms"`.
    pub link: String,
    /// Injected RTT; `None` for the unix baseline.
    pub rtt_ms: Option<u64>,
    /// `"cat"` or `"zsh"`.
    pub program: String,
    pub echo: Stats,
    /// `key→pred`: the key's cell first visible (overlay, else echo);
    /// `None` when nothing was predicted.
    pub pred: Option<Stats>,
    /// Keys the overlay showed before their echo.
    pub predicted: usize,
    /// Shown predictions the mirror disagreed with.
    pub mispredicted: usize,
    /// Echo p50 came in under the injected RTT: iroh found a path around the proxy.
    pub bypassed: bool,
}

impl Row {
    #[must_use]
    pub fn misprediction_rate(&self) -> f64 {
        self.mispredicted as f64 / self.echo.n.max(1) as f64
    }
}

/// `read -s` typed over one link: no cell may be shown.
#[derive(Clone, Debug, Serialize)]
pub struct Password {
    pub link: String,
    pub shown: usize,
    pub enabled: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub keys: usize,
    pub rows: Vec<Row>,
    pub passwords: Vec<Password>,
}

impl Report {
    /// Rows whose measurement did not go through the proxy.
    pub fn bypassed(&self) -> Vec<&str> {
        self.rows.iter().filter(|r| r.bypassed).map(|r| r.link.as_str()).collect()
    }

    /// Every missed §6 target (headless): pred p95 ≤ 20 ms at ≥ 50 ms RTT,
    /// echo p95 ≤ RTT + 20 ms, < 1 % mispredictions on zsh, nothing shown
    /// on a password prompt.
    #[must_use]
    pub fn misses(&self) -> Vec<String> {
        let mut out = Vec::new();
        for r in &self.rows {
            let (name, rtt) = (format!("{} {}", r.link, r.program), r.rtt_ms.unwrap_or(0) as f64);
            match r.pred {
                _ if rtt < 50.0 => {}
                Some(p) if p.p95 <= 20.0 => {}
                p => out.push(format!("{name}: pred p95 {:?} > 20 ms", p.map(|p| p.p95))),
            }
            if r.echo.p95 > rtt + 20.0 {
                out.push(format!("{name}: echo p95 {:.1} > {:.0} ms", r.echo.p95, rtt + 20.0));
            }
            if r.program == "zsh" && r.misprediction_rate() >= 0.01 {
                out.push(format!("{name}: {} mispredictions", r.mispredicted));
            }
        }
        let shown = self.passwords.iter().filter(|p| p.shown > 0 || p.enabled);
        out.extend(shown.map(|p| format!("{} password prompt: {} cells shown, enabled {}", p.link, p.shown, p.enabled)));
        out
    }

    /// `target/latency/report.md`: the table, password checks and targets.
    #[must_use]
    pub fn markdown(&self) -> String {
        let mut out = format!("# Keystroke latency (headless)\n\n{} keys per program plus a backspace burst; times in ms.\n\n", self.keys);
        out.push_str(&self.table());
        out.push_str("\n## Password prompt (`read -s`)\n\n| link | cells shown | prediction enabled |\n|---|---:|---|\n");
        for p in &self.passwords {
            let _ = writeln!(out, "| {} | {} | {} |", p.link, p.shown, p.enabled);
        }
        let misses = self.misses();
        out.push_str("\n## §6 targets\n\n");
        out.push_str(if misses.is_empty() { "All met.\n" } else { "Missed:\n" });
        for miss in misses {
            let _ = writeln!(out, "- {miss}");
        }
        out
    }

    pub fn table(&self) -> String {
        let mut out = String::from(
            "| link | program | echo p50 | echo p95 | echo p99 | pred p50 | pred p95 | pred p99 | predicted | mispredicted |\n\
             |---|---|---:|---:|---:|---:|---:|---:|---:|---:|\n",
        );
        for row in &self.rows {
            let pred = |f: fn(&Stats) -> f64| row.pred.as_ref().map_or("-".to_owned(), |s| format!("{:.1}", f(s)));
            let _ = writeln!(
                out,
                "| {} | {} | {:.1} | {:.1} | {:.1} | {} | {} | {} | {}/{} | {} ({:.1} %) |{}",
                row.link,
                row.program,
                row.echo.p50,
                row.echo.p95,
                row.echo.p99,
                pred(|s| s.p50),
                pred(|s| s.p95),
                pred(|s| s.p99),
                row.predicted,
                row.echo.n,
                row.mispredicted,
                row.misprediction_rate() * 100.0,
                if row.bypassed { " BYPASSED PROXY" } else { "" }
            );
        }
        out
    }
}

/// Run the bench: a unix baseline row, then one row per RTT (in ms).
///
/// # Errors
/// Server start, pairing, attach or a keystroke that never echoes.
pub async fn bench(rtts: &[u64], keys: usize) -> anyhow::Result<Report> {
    let home = temp_home();
    let task = start(&home).await?;
    let result = run(&home, rtts, keys).await;
    let stopped = stop(&home, task).await;
    let _ = std::fs::remove_dir_all(&home);
    let report = result?;
    stopped?;
    Ok(report)
}

async fn run(home: &Path, rtts: &[u64], keys: usize) -> anyhow::Result<Report> {
    let owner = Control::connect(&paths::socket(home), "bench").await?.0;
    let (mut rows, password) = measure(&owner, "unix", None, keys).await?;
    let mut passwords = vec![password];
    for (i, &rtt) in rtts.iter().enumerate() {
        let upstream = server_addr(&owner).await?;
        let proxy = Proxy::spawn((Ipv4Addr::LOCALHOST, 0).into(), upstream.1, ProxyConfig::rtt(Duration::from_millis(rtt))).await?;
        let ticket = PairTicket { addr: EndpointAddr::new(upstream.0.addr.id).with_ip_addr(proxy.addr()), secret: upstream.0.secret };
        let identity = silo_client::client_identity(&home.join(format!("client-{i}")))?;
        let options = NetOptions { relay: RelayConfig::Disabled, mdns: false, port: None, pin_initial_path: true };
        let net = Net::bind_with(&identity, options).await?;
        let (remote, _events) = timeout(T, silo_client::pair(&net, &ticket, "bench")).await.context("pair timed out")??;
        let measured = measure(&remote, &format!("{rtt}ms"), Some(rtt), keys).await;
        drop(remote);
        net.close().await?;
        let (measured, password) = measured?;
        rows.extend(measured);
        passwords.push(password);
    }
    Ok(Report { keys, rows, passwords })
}

/// `(name, command)` of the bench programs; zsh only where installed.
fn programs() -> Vec<(&'static str, &'static [&'static str])> {
    let mut out: Vec<(&'static str, &'static [&'static str])> = vec![("cat", &["/bin/cat"])];
    if Path::new("/bin/zsh").exists() {
        out.push(("zsh", &["/bin/zsh", "-f"]));
    }
    out
}

/// A row from measured keys; `rtt` is `None` for the unix baseline.
fn row(link: &str, rtt: Option<u64>, program: &str, keys: &[Key]) -> Row {
    let echo = Stats::from_samples(&keys.iter().map(|k| k.echo).collect::<Vec<_>>());
    let predicted = keys.iter().filter(|k| k.predicted).count();
    let pred = (predicted > 0).then(|| Stats::from_samples(&keys.iter().map(|k| k.visible).collect::<Vec<_>>()));
    let mispredicted = keys.iter().filter(|k| k.wrong).count();
    let bypassed = rtt.is_some_and(|rtt| echo.p50 < rtt as f64);
    Row { link: link.into(), rtt_ms: rtt, program: program.into(), echo, pred, predicted, mispredicted, bypassed }
}

/// Every program over `control` (`keys` letters, Enter every `LINE` keys,
/// then a backspace burst), and the `read -s` check.
async fn measure(control: &Control, link: &str, rtt: Option<u64>, keys: usize) -> anyhow::Result<(Vec<Row>, Password)> {
    let predictor = || if rtt.is_some() { Predictor::remote(None) } else { Predictor::local() };
    let mut rows = Vec::new();
    for (program, command) in programs() {
        let mut session = Session::open(control, command, predictor()).await?;
        let mut typed = Vec::with_capacity(keys + BURST.len());
        for i in 0..keys {
            if i > 0 && i % LINE == 0 {
                session.enter().await?;
            }
            typed.push(session.key(b'a' + (i % 26) as u8).await?);
            tokio::time::sleep(KEY_GAP).await;
        }
        for &byte in BURST {
            typed.push(session.key(byte).await?);
        }
        session.close();
        rows.push(row(link, rtt, program, &typed));
    }
    let mut session = Session::open(control, &["/bin/sh", "-c", "read -s x"], predictor()).await?;
    let wait = Duration::from_millis(rtt.unwrap_or(0) + 50);
    let enabled = session.blind(b"secret", wait).await?;
    session.close();
    Ok((rows, Password { link: link.into(), shown: session.max_shown, enabled }))
}

/// A fresh invite and the server's IPv4 loopback endpoint address.
async fn server_addr(owner: &Control) -> anyhow::Result<(PairTicket, SocketAddr)> {
    for _ in 0..200 {
        let invite: PairInvite = owner.call(Method::PairCreate { ttl_secs: 60 }).await?;
        let ticket = PairTicket::decode(&invite.ticket)?;
        let port = ticket.addr.ip_addrs().find(|a| a.is_ipv4()).map(SocketAddr::port);
        if let Some(port) = port {
            return Ok((ticket, (Ipv4Addr::LOCALHOST, port).into()));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    bail!("server endpoint has no direct IPv4 address")
}

fn temp_home() -> PathBuf {
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let dir = PathBuf::from(format!("/tmp/silo-latency-{}-{nanos}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

async fn start(home: &Path) -> anyhow::Result<JoinHandle<anyhow::Result<()>>> {
    let config = silo_server::Config {
        home: home.to_owned(),
        idle_exit: None,
        relay: RelayConfig::Disabled,
        mdns: false,
        pin_initial_path: true,
        ..Default::default()
    };
    let task = tokio::spawn(async move { silo_server::run(config).await });
    for _ in 0..500 {
        if silo_client::server_running(home).await {
            return Ok(task);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    bail!("server did not start")
}

async fn stop(home: &Path, task: JoinHandle<anyhow::Result<()>>) -> anyhow::Result<()> {
    let owner = Control::connect(&paths::socket(home), "bench").await?.0;
    let _ = owner.call_value(Method::ServerShutdown {}).await;
    drop(owner);
    timeout(T, task).await.context("server shutdown timed out")??
}
