use std::{
    net::Ipv4Addr,
    time::{Duration, Instant},
};

use silo_latency::{Link, Proxy, ProxyConfig, Stats};
use tokio::net::UdpSocket;

/// Raw UDP echo through the proxy: never under `rtt`, and over a 0 ms
/// proxy measured in the same run the median adds `rtt` within `SLACK`.
/// The baseline takes out what the host's loopback and scheduling cost; the
/// slack covers the proxy's timer wakeups, which a loaded shared runner
/// delays by a few ms past `deliver`'s spin window, but not a wrong delay.
#[tokio::test(flavor = "multi_thread")]
async fn proxy_adds_configured_rtt() {
    const SLACK: f64 = 5.0;
    let echo = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let upstream = echo.local_addr().unwrap();
    tokio::spawn(async move {
        let mut buf = [0u8; 2048];
        while let Ok((n, from)) = echo.recv_from(&mut buf).await {
            let _ = echo.send_to(&buf[..n], from).await;
        }
    });
    let mut measured = Vec::new();
    for rtt in [0u64, 40] {
        let rtt = Duration::from_millis(rtt);
        let proxy = Proxy::spawn((Ipv4Addr::LOCALHOST, 0).into(), upstream, ProxyConfig::rtt(rtt)).await.unwrap();
        let client = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        client.connect(proxy.addr()).await.unwrap();
        let mut samples = Vec::new();
        for i in 0u32..20 {
            let sent = Instant::now();
            client.send(&i.to_be_bytes()).await.unwrap();
            let mut buf = [0u8; 4];
            let n = tokio::time::timeout(Duration::from_secs(2), client.recv(&mut buf)).await.unwrap().unwrap();
            assert_eq!(&buf[..n], &i.to_be_bytes(), "order preserved");
            samples.push(sent.elapsed());
        }
        let min = samples.iter().min().unwrap().as_secs_f64() * 1e3;
        measured.push((rtt.as_secs_f64() * 1e3, min, Stats::from_samples(&samples)));
    }
    let base = measured[0].2;
    for (want, min, stats) in &measured {
        assert!(min >= want, "rtt {want} ms: a round trip of {min:.2} ms: {stats:?}");
        assert!(stats.p50 - base.p50 < want + SLACK, "rtt {want} ms over the 0 ms baseline {base:?}: {stats:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn proxy_loss_drops_everything() {
    let sink = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let config = ProxyConfig { loss: 1.0, ..Default::default() };
    let proxy = Proxy::spawn((Ipv4Addr::LOCALHOST, 0).into(), sink.local_addr().unwrap(), config).await.unwrap();
    let client = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    client.send_to(b"x", proxy.addr()).await.unwrap();
    let mut buf = [0u8; 4];
    assert!(tokio::time::timeout(Duration::from_millis(100), sink.recv(&mut buf)).await.is_err());
}

/// Full path: in-process server, client paired over a 50 ms proxy. Enough
/// keys that the two warm-up keys (no RTT sample yet, then the tentative
/// epoch) and a few scheduling hiccups stay out of p95. The echo p95 target
/// is judged on what silo adds ([`silo_latency::Report::silo_misses`]): a
/// shared runner late on the proxy's timers is not a silo regression.
#[tokio::test(flavor = "multi_thread")]
async fn bench_echo_at_50ms() {
    const KEYS: usize = 40;
    let report = silo_latency::bench(&[50], KEYS).await.unwrap();
    let table = report.markdown();
    let row = report.rows.iter().find(|r| r.rtt_ms == Some(50) && r.program == "cat").unwrap();
    assert_eq!(row.echo.n, KEYS + 6, "letters plus the backspace burst");
    assert!(row.echo.p50 >= 50.0 && row.echo.p50 < 50.0 + 15.0, "{table}");
    let pred = row.pred.expect("predicted at 50 ms");
    assert!(pred.p50 < 5.0 && row.predicted >= KEYS, "{table}");
    let link = row.stages.iter().filter(|(name, _)| name == "server_input" || name == "client_received").count();
    assert_eq!(link, 2, "stage stamps recorded: {table}");
    let unix = &report.rows[0];
    assert!(unix.echo.p50 < 15.0 && unix.pred.is_none(), "{table}");
    assert!(report.passwords.iter().all(|p| p.shown == 0 && !p.enabled), "{table}");
    let misses = report.silo_misses();
    assert!(misses.is_empty(), "{misses:#?}\n{table}");
}

/// Typing ahead of the echo with every painted state checked (docs/PHASE5.md
/// §3.1): `cargo test -p silo-latency -- --ignored --nocapture type_ahead_probe`.
/// `PROBE_RTT` and `PROBE_JITTER` shape the link, `PROBE_GAP`..`PROBE_GAP_MAX`
/// the time between keys (ms); `PROBE_SHELL` is `/bin/bash`, `/bin/zsh` or
/// `zsh-hl` (a `ZDOTDIR=/tmp/zdot` sourcing zsh-syntax-highlighting and
/// zsh-autosuggestions). `PROBE_TICKET` and `PROBE_HOME` pair with a real
/// host instead and type into bash there.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "a diagnostic"]
async fn type_ahead_probe() {
    let env = |name: &str, default: u64| std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default);
    let (rtt, gap, gap_max, jitter) = (env("PROBE_RTT", 150), env("PROBE_GAP", 60), env("PROBE_GAP_MAX", 60), env("PROBE_JITTER", 0));
    let shell = std::env::var("PROBE_SHELL").unwrap_or_else(|_| "/bin/bash".into());
    let args: Vec<&str> = match shell.as_str() {
        "/bin/zsh" => vec!["/bin/zsh", "-f"],
        // ZDOTDIR with zsh-syntax-highlighting and zsh-autosuggestions sourced.
        "zsh-hl" => vec!["/usr/bin/env", "ZDOTDIR=/tmp/zdot", "HOME=/tmp/zdot", "/bin/zsh", "-i"],
        s => vec![s, "--norc", "--noprofile"],
    };
    let link = ProxyConfig { jitter: Duration::from_millis(jitter), ..ProxyConfig::rtt(Duration::from_millis(rtt)) };
    let gaps = (Duration::from_millis(gap), Duration::from_millis(gap_max));
    let text = b"echo hello world, this is typing ahead";
    let typed = match std::env::var("PROBE_TICKET") {
        Ok(ticket) => {
            let home = std::path::PathBuf::from(std::env::var("PROBE_HOME").expect("PROBE_HOME for the probe identity"));
            let args: Vec<&str> = vec!["/bin/bash", "-i"];
            silo_latency::bench::type_ahead_remote(&home, &ticket, &args, text, gaps).await.unwrap()
        }
        Err(_) => silo_latency::bench::type_ahead(link, &args, text, gaps).await.unwrap(),
    };
    for line in &typed.log {
        println!("{line}");
    }
    println!("states {} regressions {} exact {}", typed.states, typed.regressions.len(), typed.exact);
    for line in &typed.regressions {
        println!("REGRESSION {line}");
    }
}

/// The full report with per-stage times and the load rows:
/// `cargo test -p silo-latency --release -- --ignored --nocapture report`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "a benchmark, not a check"]
async fn report() {
    let report = silo_latency::bench_with(&[Link::rtt(0), Link::rtt(50)], 100, true).await.unwrap();
    let markdown = report.markdown();
    std::fs::create_dir_all("../../target/latency").unwrap();
    std::fs::write("../../target/latency/report.md", &markdown).unwrap();
    println!("{markdown}");
}

/// Fast links (a LAN): `cargo test -p silo-latency --release -- --ignored --nocapture lan`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "a benchmark, not a check"]
async fn lan() {
    let links =
        [Link::rtt(2), Link { seed_ms: Some(120), ..Link::rtt(2) }, Link { mb_per_s: 25, ..Link::rtt(2) }, Link { jitter_ms: 8, ..Link::rtt(2) }];
    let report = silo_latency::bench_with(&links, 100, true).await.unwrap();
    println!("{}", report.table());
}
