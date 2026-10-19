//! Headless keystroke bench: an in-process server, a client paired over the
//! delay proxy, and per-key `key→echo` timings.

use std::{
    fmt::Write as _,
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use anyhow::{Context, anyhow, bail};
use serde::Serialize;
use silo_client::{
    Control, DataStream, Mirror,
    proto::{
        control::{Created, Method, PairInvite, SpawnSpec},
        data::{ClientData, ServerData},
        model::TermSize,
        paths,
    },
    transport::{EndpointAddr, Net, PairTicket, RelayConfig},
};
use tokio::{task::JoinHandle, time::timeout};

use crate::proxy::{Proxy, ProxyConfig};

const T: Duration = Duration::from_secs(10);
/// Pause between keystrokes so each key is measured on a quiet link.
const KEY_GAP: Duration = Duration::from_millis(20);

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
    pub echo: Stats,
    /// `key→pred`; `None` until the predictor exists.
    // TODO(P5-2): fill from `Predictor::overlay()` (first frame showing the key's cell).
    pub pred: Option<Stats>,
    /// Echo p50 came in under the injected RTT: iroh found a path around the proxy.
    pub bypassed: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub keys: usize,
    pub rows: Vec<Row>,
}

impl Report {
    /// Rows whose measurement did not go through the proxy.
    pub fn bypassed(&self) -> Vec<&str> {
        self.rows.iter().filter(|r| r.bypassed).map(|r| r.link.as_str()).collect()
    }

    pub fn table(&self) -> String {
        let mut out = String::from("link    | echo p50 | echo p95 | echo p99 | pred p50 | pred p95 | pred p99\n");
        for row in &self.rows {
            let pred = |f: fn(&Stats) -> f64| row.pred.as_ref().map_or("-".to_owned(), |s| format!("{:.1}", f(s)));
            let _ = writeln!(
                out,
                "{:<7} | {:>8.1} | {:>8.1} | {:>8.1} | {:>8} | {:>8} | {:>8}{}",
                row.link,
                row.echo.p50,
                row.echo.p95,
                row.echo.p99,
                pred(|s| s.p50),
                pred(|s| s.p95),
                pred(|s| s.p99),
                if row.bypassed { "  BYPASSED PROXY" } else { "" }
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
    let unix = measure(&owner, keys).await?;
    let mut rows = vec![Row { link: "unix".into(), rtt_ms: None, echo: unix, pred: None, bypassed: false }];
    for (i, &rtt) in rtts.iter().enumerate() {
        let upstream = server_addr(&owner).await?;
        let proxy = Proxy::spawn((Ipv4Addr::LOCALHOST, 0).into(), upstream.1, ProxyConfig::rtt(Duration::from_millis(rtt))).await?;
        let ticket = PairTicket { addr: EndpointAddr::new(upstream.0.addr.id).with_ip_addr(proxy.addr()), secret: upstream.0.secret };
        let identity = silo_client::client_identity(&home.join(format!("client-{i}")))?;
        let net = Net::bind(&identity, RelayConfig::Disabled, false).await?;
        let (remote, _events) = timeout(T, silo_client::pair(&net, &ticket, "bench")).await.context("pair timed out")??;
        let echo = measure(&remote, keys).await;
        drop(remote);
        net.close().await?;
        let echo = echo?;
        let bypassed = echo.p50 < rtt as f64;
        rows.push(Row { link: format!("{rtt}ms"), rtt_ms: Some(rtt), echo, pred: None, bypassed });
    }
    Ok(Report { keys, rows })
}

/// Create a shell session on `control` and time `keys` single-byte echoes.
async fn measure(control: &Control, keys: usize) -> anyhow::Result<Stats> {
    let spec = SpawnSpec { command: vec!["/bin/sh".into()], cwd: Some("/tmp".into()), size: Some(TermSize::new(200, 24)) };
    let created: Created = control.call(Method::SessionCreate { name: None, spawn: spec }).await?;
    let mut mirror = Mirror::new();
    let mut stream = DataStream::open(control, &created.block_id, None).await?;
    mirror.begin(&stream.welcome)?;
    // Wait for the prompt, then for output to settle.
    while recv(&mut stream, &mut mirror, T).await?.is_none() {}
    while timeout(Duration::from_millis(200), recv(&mut stream, &mut mirror, T)).await.is_ok() {}

    let mut samples = Vec::with_capacity(keys);
    for i in 0..keys {
        let key = b'a' + (i % 26) as u8;
        let sent = Instant::now();
        stream.send(&ClientData::Input(vec![key]));
        // TODO(P5-2): `predictor.input(&[key], frame)`; stamp `t_pred` when the overlay first shows the cell.
        let deadline = sent + T;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if recv(&mut stream, &mut mirror, left).await?.is_some_and(|bytes| bytes.contains(&key)) {
                break;
            }
        }
        samples.push(sent.elapsed());
        stream.ack(&mut mirror, true);
        tokio::time::sleep(KEY_GAP).await;
    }
    stream.send(&ClientData::Input(b"\x15exit\n".to_vec()));
    Ok(Stats::from_samples(&samples))
}

/// Apply the next server frame; returns the bytes when it was output.
async fn recv(stream: &mut DataStream, mirror: &mut Mirror, wait: Duration) -> anyhow::Result<Option<Vec<u8>>> {
    let data = timeout(wait, stream.rx.recv()).await.context("timed out waiting for output")?.ok_or_else(|| anyhow!("data stream closed"))?;
    // TODO(P5-2): `predictor.server(&data)` and `predictor.judge(frame, mirror.next_offset())` after apply.
    let bytes = match &data {
        ServerData::Output { bytes, .. } => Some(bytes.clone()),
        _ => None,
    };
    mirror.apply(data)?;
    Ok(bytes)
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
    let config = silo_server::Config { home: home.to_owned(), idle_exit: None, relay: RelayConfig::Disabled, mdns: false, ..Default::default() };
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
