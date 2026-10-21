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
    Control, DataStream, Mirror, Predictor,
    proto::{
        control::{Created, Method, OwnMode, PairInvite, SpawnSpec},
        data::ServerData,
        model::{BlockKind, TermSize},
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
    /// `"cat"` or `"zsh"`; `"cat+load"` while another session streams.
    pub program: String,
    pub echo: Stats,
    /// `key→pred`: the key's cell first visible (overlay, else echo);
    /// `None` when nothing was predicted.
    pub pred: Option<Stats>,
    /// Keys the overlay showed before their echo.
    pub predicted: usize,
    /// How long those keys showed as (underlined) predictions.
    pub overlay: Option<Stats>,
    /// Shown predictions the mirror disagreed with.
    pub mispredicted: usize,
    /// Echo p50 came in under the injected RTT: iroh found a path around the proxy.
    pub bypassed: bool,
    /// `key→stage` per reached stage of [`crate::stages::STAGES`] (input
    /// stages only under load, where output is not attributable).
    pub stages: Vec<(String, Stats)>,
    /// `key→echo` less each key's measured link legs (`client_sent→server_input`
    /// and `server_flushed→client_received`): the latency silo adds, free of
    /// proxy and runner timer jitter. A key missing a leg counts in full.
    /// `None` for the unix baseline.
    pub added: Option<Stats>,
}

impl Row {
    /// Measured while another session streamed: diagnostics, no targets.
    #[must_use]
    pub fn loaded(&self) -> bool {
        self.program.contains("+load")
    }

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
        self.judge(false)
    }

    /// [`Report::misses`] with the proxied echo target judged on
    /// [`Row::added`] (≤ 20 ms at p95): a slow shared runner delaying the
    /// proxy's timers can't miss it, a slower silo still does.
    #[must_use]
    pub fn silo_misses(&self) -> Vec<String> {
        self.judge(true)
    }

    fn judge(&self, added: bool) -> Vec<String> {
        let mut out = Vec::new();
        for r in self.rows.iter().filter(|r| !r.loaded()) {
            let (name, rtt) = (format!("{} {}", r.link, r.program), r.rtt_ms.unwrap_or(0) as f64);
            match r.pred {
                _ if rtt < 50.0 => {}
                Some(p) if p.p95 <= 20.0 => {}
                p => out.push(format!("{name}: pred p95 {:?} > 20 ms", p.map(|p| p.p95))),
            }
            match r.added {
                Some(a) if added && r.rtt_ms.is_some() => {
                    if a.p95 > 20.0 {
                        out.push(format!("{name}: silo-added echo p95 {:.1} > 20 ms", a.p95));
                    }
                }
                None if added && r.rtt_ms.is_some() => out.push(format!("{name}: no silo-added echo")),
                _ if r.echo.p95 > rtt + 20.0 => out.push(format!("{name}: echo p95 {:.1} > {:.0} ms", r.echo.p95, rtt + 20.0)),
                _ => {}
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
        out.push_str("\n## Stages (key→stage p50 / p95 / p99, ms)\n\n");
        out.push_str(&self.stage_table());
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

    /// One line per row: every reached stage, then the echo.
    pub fn stage_table(&self) -> String {
        let mut out = String::from("| link | program | stages |\n|---|---|---|\n");
        for row in &self.rows {
            let f = |s: &Stats| format!("{:.2}/{:.2}/{:.2}", s.p50, s.p95, s.p99);
            let stages: Vec<String> =
                row.stages.iter().map(|(name, s)| format!("{name} {}", f(s))).chain([format!("echo {}", f(&row.echo))]).collect();
            let added = row.added.map_or(String::new(), |a| format!(" (silo-added {})", f(&a)));
            let _ = writeln!(out, "| {} | {} | {}{added} |", row.link, row.program, stages.join(" → "));
        }
        out
    }

    pub fn table(&self) -> String {
        let mut out = String::from(
            "| link | program | echo p50 | echo p95 | echo p99 | pred p50 | pred p95 | pred p99 | predicted | shown p50/p95 | mispredicted |\n\
             |---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n",
        );
        for row in &self.rows {
            let pred = |f: fn(&Stats) -> f64| row.pred.as_ref().map_or("-".to_owned(), |s| format!("{:.1}", f(s)));
            let _ = writeln!(
                out,
                "| {} | {} | {:.1} | {:.1} | {:.1} | {} | {} | {} | {}/{} | {} | {} ({:.1} %) |{}",
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
                row.overlay.map_or("-".to_owned(), |s| format!("{:.1}/{:.1}", s.p50, s.p95)),
                row.mispredicted,
                row.misprediction_rate() * 100.0,
                if row.bypassed { " BYPASSED PROXY" } else { "" }
            );
        }
        out
    }
}

/// One proxied iroh link.
#[derive(Clone, Copy, Debug)]
pub struct Link {
    pub rtt_ms: u64,
    /// Extra uniform one-way delay in `[0, jitter)` per datagram.
    pub jitter_ms: u64,
    /// Link rate in MB/s each way (`0`: unlimited).
    pub mb_per_s: u64,
    /// The RTT the predictor starts from; `None` takes the connection's
    /// path RTT, as the app does. A larger one stands for a connection that
    /// came up over a relay before switching to the direct path.
    pub seed_ms: Option<u64>,
}

impl Link {
    #[must_use]
    pub fn rtt(rtt_ms: u64) -> Self {
        Self { rtt_ms, jitter_ms: 0, mb_per_s: 0, seed_ms: None }
    }

    fn name(&self) -> String {
        let mut name = format!("{}ms", self.rtt_ms);
        if self.jitter_ms > 0 {
            name += &format!(" +{}ms jitter", self.jitter_ms);
        }
        if self.mb_per_s > 0 {
            name += &format!(" {}MB/s", self.mb_per_s);
        }
        if let Some(seed) = self.seed_ms {
            name += &format!(" (seed {seed}ms)");
        }
        name
    }
}

/// Run the bench: a unix baseline row, then one row per RTT (in ms).
///
/// # Errors
/// Server start, pairing, attach or a keystroke that never echoes.
pub async fn bench(rtts: &[u64], keys: usize) -> anyhow::Result<Report> {
    bench_with(&rtts.iter().copied().map(Link::rtt).collect::<Vec<_>>(), keys, false).await
}

/// [`bench`] over `links`, with `load`: each link also types into `cat`
/// while another session on it streams output as fast as it can.
///
/// # Errors
/// As [`bench`].
pub async fn bench_with(links: &[Link], keys: usize, load: bool) -> anyhow::Result<Report> {
    crate::stages::install();
    let home = temp_home();
    let task = start(&home).await?;
    let result = run(&home, links, keys, load).await;
    let stopped = stop(&home, task).await;
    let _ = std::fs::remove_dir_all(&home);
    let report = result?;
    stopped?;
    Ok(report)
}

async fn run(home: &Path, links: &[Link], keys: usize, load: bool) -> anyhow::Result<Report> {
    let owner = Control::connect(&paths::socket(home), "bench").await?.0;
    let (mut rows, password) = measure(&owner, "unix", None, None, keys, load).await?;
    let mut passwords = vec![password];
    for (i, link) in links.iter().enumerate() {
        let upstream = server_addr(&owner).await?;
        let config = ProxyConfig {
            jitter: Duration::from_millis(link.jitter_ms),
            rate: link.mb_per_s * 1_000_000,
            ..ProxyConfig::rtt(Duration::from_millis(link.rtt_ms))
        };
        let proxy = Proxy::spawn((Ipv4Addr::LOCALHOST, 0).into(), upstream.1, config).await?;
        let ticket = PairTicket { addr: EndpointAddr::new(upstream.0.addr.id).with_ip_addr(proxy.addr()), secret: upstream.0.secret };
        let identity = silo_client::client_identity(&home.join(format!("client-{i}")))?;
        let options = NetOptions { relay: RelayConfig::Disabled, mdns: false, port: None, pin_initial_path: true };
        let net = Net::bind_with(&identity, options).await?;
        let (remote, _events) = timeout(T, silo_client::pair(&net, &ticket, "bench")).await.context("pair timed out")??;
        let seed = match link.seed_ms {
            Some(seed) => Some(Duration::from_millis(seed)),
            None => match remote.conn() {
                silo_client::transport::Conn::Iroh(conn) => conn.rtt(),
                silo_client::transport::Conn::Unix(_) => None,
            },
        };
        let measured = measure(&remote, &link.name(), Some(link.rtt_ms), Some(seed), keys, load).await;
        drop(remote);
        net.close().await?;
        let (measured, password) = measured?;
        rows.extend(measured);
        passwords.push(password);
    }
    Ok(Report { keys, rows, passwords })
}

/// Type `text` into `command` over `link`, a key every `gaps` (uniform), without
/// waiting for echoes ([`Session::type_ahead`]).
///
/// # Errors
/// As [`bench`].
pub async fn type_ahead(link: ProxyConfig, command: &[&str], text: &[u8], gaps: (Duration, Duration)) -> anyhow::Result<crate::keys::TypedAhead> {
    let home = temp_home();
    let task = start(&home).await?;
    let result = async {
        let owner = Control::connect(&paths::socket(&home), "bench").await?.0;
        let upstream = server_addr(&owner).await?;
        let proxy = Proxy::spawn((Ipv4Addr::LOCALHOST, 0).into(), upstream.1, link).await?;
        let ticket = PairTicket { addr: EndpointAddr::new(upstream.0.addr.id).with_ip_addr(proxy.addr()), secret: upstream.0.secret };
        let identity = silo_client::client_identity(&home.join("client"))?;
        let options = NetOptions { relay: RelayConfig::Disabled, mdns: false, port: None, pin_initial_path: true };
        let net = Net::bind_with(&identity, options).await?;
        let (remote, _events) = timeout(T, silo_client::pair(&net, &ticket, "bench")).await.context("pair timed out")??;
        let rtt = match remote.conn() {
            silo_client::transport::Conn::Iroh(conn) => conn.rtt(),
            silo_client::transport::Conn::Unix(_) => None,
        };
        let mut session = Session::open(&remote, command, Predictor::remote(rtt)).await?;
        // One echoed key confirms the first epoch, as at any prompt.
        session.key(b'#').await?;
        let typed = session.type_ahead(text, gaps).await;
        session.close();
        drop(remote);
        net.close().await?;
        typed
    }
    .await;
    let stopped = stop(&home, task).await;
    let _ = std::fs::remove_dir_all(&home);
    let typed = result?;
    stopped?;
    Ok(typed)
}

/// [`type_ahead`] against a real remote server, paired with `ticket` from
/// a throwaway identity under `home`.
///
/// # Errors
/// Pairing, attach or a key that never echoes.
pub async fn type_ahead_remote(
    home: &Path,
    ticket: &str,
    command: &[&str],
    text: &[u8],
    gaps: (Duration, Duration),
) -> anyhow::Result<crate::keys::TypedAhead> {
    let ticket = PairTicket::decode(ticket)?;
    let identity = silo_client::client_identity(&home.join("client"))?;
    let net = Net::bind_with(&identity, NetOptions::default()).await?;
    let (remote, _events) =
        timeout(Duration::from_secs(30), silo_client::pair(&net, &ticket, "type-ahead probe")).await.context("pair timed out")??;
    let rtt = match remote.conn() {
        silo_client::transport::Conn::Iroh(conn) => conn.rtt(),
        silo_client::transport::Conn::Unix(_) => None,
    };
    let mut session = Session::open(&remote, command, Predictor::remote(rtt)).await?;
    session.key(b'#').await?;
    let typed = session.type_ahead(text, gaps).await;
    session.close();
    tokio::time::sleep(Duration::from_millis(500)).await;
    drop(remote);
    net.close().await?;
    typed
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
    let reached = if program.ends_with("+load") { crate::stages::INPUT_STAGES } else { crate::stages::STAGES.len() };
    let stages = crate::stages::STAGES[..reached]
        .iter()
        .enumerate()
        .filter_map(|(i, (name, _))| {
            let samples: Vec<Duration> = keys.iter().filter_map(|k| k.stages.get(i).copied().flatten()).collect();
            (!samples.is_empty()).then(|| ((*name).to_owned(), Stats::from_samples(&samples)))
        })
        .collect();
    let echo = Stats::from_samples(&keys.iter().map(|k| k.echo).collect::<Vec<_>>());
    let added = rtt.is_some().then(|| Stats::from_samples(&keys.iter().map(silo_added).collect::<Vec<_>>()));
    let predicted = keys.iter().filter(|k| k.predicted).count();
    let pred = (predicted > 0).then(|| Stats::from_samples(&keys.iter().map(|k| k.visible).collect::<Vec<_>>()));
    let shown: Vec<Duration> = keys.iter().filter(|k| k.predicted).map(|k| k.echo.saturating_sub(k.visible)).collect();
    let overlay = (!shown.is_empty()).then(|| Stats::from_samples(&shown));
    let mispredicted = keys.iter().filter(|k| k.wrong).count();
    let bypassed = rtt.is_some_and(|rtt| echo.p50 < rtt as f64);
    Row { link: link.into(), rtt_ms: rtt, program: program.into(), echo, pred, predicted, overlay, mispredicted, bypassed, stages, added }
}

/// `key.echo` less its two link legs, or all of it when a leg is missing.
fn silo_added(key: &Key) -> Duration {
    let at = |name: &str| {
        let i = crate::stages::STAGES.iter().position(|(stage, _)| *stage == name)?;
        key.stages.get(i).copied().flatten()
    };
    let leg = |from: &str, to: &str| at(to)?.checked_sub(at(from)?);
    match (leg("client_sent", "server_input"), leg("server_flushed", "client_received")) {
        (Some(up), Some(down)) => key.echo.saturating_sub(up + down),
        _ => key.echo,
    }
}

/// Every program over `control` (`keys` letters, Enter every `LINE` keys,
/// then a backspace burst), and the `read -s` check.
/// `seed`: `Some` for a remote link, with the RTT its predictor starts from.
async fn measure(
    control: &Control,
    link: &str,
    rtt: Option<u64>,
    seed: Option<Option<Duration>>,
    keys: usize,
    load: bool,
) -> anyhow::Result<(Vec<Row>, Password)> {
    let predictor = || seed.map_or_else(Predictor::local, Predictor::remote);
    let mut rows = Vec::new();
    for (program, command) in programs() {
        let session = Session::open(control, command, predictor()).await?;
        rows.push(row(link, rtt, program, &type_keys(session, keys).await?));
    }
    if load {
        let flood = Flood::start(control).await?;
        let session = Session::open(control, &["/bin/cat"], predictor()).await;
        let typed = match session {
            Ok(session) => type_keys(session, keys).await,
            Err(error) => Err(error),
        };
        let flooded = flood.stop(control).await;
        rows.push(row(link, rtt, "cat+load", &typed?));
        let flooded = flooded?;
        let cut = if flooded.cut { ", stream cut as lagging" } else { "" };
        let delay = Stats::from_samples(&flooded.delays);
        rows.last_mut().expect("row").program =
            format!("cat+load ({:.0} MB/s, output delay {:.0}/{:.0}/{:.0} ms{cut})", flooded.rate / 1e6, delay.p50, delay.p95, delay.p99);
    }
    let mut session = Session::open(control, &["/bin/sh", "-c", "read -s x"], predictor()).await?;
    let wait = Duration::from_millis(rtt.unwrap_or(0) + 50);
    let enabled = session.blind(b"secret", wait).await?;
    session.close();
    Ok((rows, Password { link: link.into(), shown: session.max_shown, enabled }))
}

/// `keys` letters (Enter every `LINE` keys), then the backspace burst.
async fn type_keys(mut session: Session, keys: usize) -> anyhow::Result<Vec<Key>> {
    crate::stages::clear();
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
    Ok(typed)
}

/// A session printing timestamped lines as fast as it can, its output
/// applied to a mirror (and acknowledged) on its own thread, as a client
/// would; each line's age on arrival is its output delay.
struct Flood {
    session_id: String,
    started: std::time::Instant,
    thread: std::thread::JoinHandle<(u64, Vec<Duration>)>,
}

struct Flooded {
    /// Bytes per second received.
    rate: f64,
    /// Output delays, sampled about every millisecond.
    delays: Vec<Duration>,
    /// The server cut the stream (a subscriber that fell too far behind).
    cut: bool,
}

/// Age of the first complete timestamped line in `bytes`.
fn line_age(bytes: &[u8]) -> Option<Duration> {
    let start = bytes.iter().position(|&b| b == b'\n')? + 1;
    let line = bytes.get(start..)?;
    let end = line.iter().position(|&b| b == b' ')?;
    let stamp: f64 = std::str::from_utf8(&line[..end]).ok()?.parse().ok()?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs_f64();
    Some(Duration::from_secs_f64((now - stamp).max(0.0)))
}

impl Flood {
    async fn start(control: &Control) -> anyhow::Result<Self> {
        let script = r#"use Time::HiRes "time"; $| = 0; my $pad = "x" x 150; printf "%.6f %s\n", time, $pad while 1"#;
        let command = ["/usr/bin/perl", "-e", script].map(str::to_owned).to_vec();
        let spawn = SpawnSpec { command, cwd: Some("/tmp".into()), size: Some(TermSize::new(200, 50)) };
        let method = Method::SessionCreate { name: None, spawn, kind: BlockKind::Terminal, url: None };
        let created: Created = control.call(method).await?;
        let mut stream = DataStream::open(control, &created.block_id, None).await?;
        // Taken, as the app takes the focused pane: this client drives it.
        control.call_value(Method::OwnBlock { block_ids: vec![created.block_id.clone()], mode: OwnMode::Take }).await?;
        let thread = std::thread::spawn(move || {
            let mut mirror = Mirror::new();
            let (mut bytes, mut delays) = (0, Vec::new());
            if mirror.begin(&stream.welcome).is_err() {
                return (bytes, delays);
            }
            let mut sampled = std::time::Instant::now();
            while let Some(data) = stream.rx.blocking_recv() {
                if let ServerData::Output { bytes: out, .. } = &data {
                    bytes += out.len() as u64;
                    if sampled.elapsed() >= Duration::from_millis(1)
                        && let Some(age) = line_age(out)
                    {
                        delays.push(age);
                        sampled = std::time::Instant::now();
                    }
                }
                if mirror.apply(data).is_err() {
                    break;
                }
                stream.ack(&mut mirror, stream.rx.is_empty());
            }
            (bytes, delays)
        });
        Ok(Self { session_id: created.session_id, started: std::time::Instant::now(), thread })
    }

    /// End the session.
    async fn stop(self, control: &Control) -> anyhow::Result<Flooded> {
        let cut = self.thread.is_finished();
        control.call_value(Method::SessionClose { session_id: self.session_id, force: false }).await?;
        let secs = self.started.elapsed().as_secs_f64();
        let (bytes, delays) = tokio::task::spawn_blocking(move || self.thread.join()).await?.map_err(|_| anyhow::anyhow!("flood thread panicked"))?;
        Ok(Flooded { rate: bytes as f64 / secs, delays, cut })
    }
}

/// A fresh invite and the server's IPv4 loopback endpoint address.
async fn server_addr(owner: &Control) -> anyhow::Result<(PairTicket, SocketAddr)> {
    for _ in 0..200 {
        let invite: PairInvite = owner.call(Method::PairCreate { ttl_secs: 60, purpose: silo_client::proto::control::InvitePurpose::Device }).await?;
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
    let _ = owner.call_value(Method::ServerShutdown { force: false }).await;
    drop(owner);
    timeout(T, task).await.context("server shutdown timed out")??
}
