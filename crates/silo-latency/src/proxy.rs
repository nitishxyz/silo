//! UDP delay proxy: forwards datagrams between one client and an upstream
//! socket (the server's iroh endpoint), delaying each direction.

use std::{
    io,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};

use rand::Rng;
use tokio::{
    net::UdpSocket,
    sync::mpsc,
    task::JoinSet,
    time::{Instant, sleep_until},
};

/// Per-direction impairment.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProxyConfig {
    /// One-way delay applied to every datagram in each direction.
    pub delay: Duration,
    /// Extra uniform delay in `[0, jitter)`; non-zero allows reordering.
    pub jitter: Duration,
    /// Drop probability in `[0, 1]`.
    pub loss: f64,
}

impl ProxyConfig {
    /// `rtt / 2` each way, no jitter, no loss.
    pub fn rtt(rtt: Duration) -> Self {
        ProxyConfig { delay: rtt / 2, ..Default::default() }
    }

    fn deadline(&self) -> Option<Instant> {
        let mut rng = rand::rng();
        if self.loss > 0.0 && rng.random_bool(self.loss.min(1.0)) {
            return None;
        }
        let jitter = if self.jitter.is_zero() { Duration::ZERO } else { self.jitter.mul_f64(rng.random::<f64>()) };
        Some(Instant::now() + self.delay + jitter)
    }
}

/// A running proxy; dropping it stops forwarding.
pub struct Proxy {
    addr: SocketAddr,
    _tasks: JoinSet<()>,
}

#[derive(Clone)]
enum Target {
    Upstream(Arc<UdpSocket>),
    Client(Arc<UdpSocket>, Arc<Mutex<Option<SocketAddr>>>),
}

impl Target {
    async fn send(&self, packet: &[u8]) {
        let _ = match self {
            Target::Upstream(socket) => socket.send(packet).await,
            Target::Client(socket, peer) => {
                let Some(peer) = *peer.lock().unwrap() else { return };
                socket.send_to(packet, peer).await
            }
        };
    }
}

impl Proxy {
    /// Listen on `listen` for the client and forward to `upstream`.
    ///
    /// # Errors
    /// Socket bind/connect failure.
    pub async fn spawn(listen: SocketAddr, upstream: SocketAddr, config: ProxyConfig) -> io::Result<Proxy> {
        let front = Arc::new(UdpSocket::bind(listen).await?);
        let back_bind: SocketAddr =
            if upstream.is_ipv4() { (Ipv4Addr::UNSPECIFIED, 0).into() } else { (Ipv6Addr::UNSPECIFIED, 0).into() };
        let back = Arc::new(UdpSocket::bind(back_bind).await?);
        back.connect(upstream).await?;
        let addr = front.local_addr()?;
        let client = Arc::new(Mutex::new(None));

        let mut tasks = JoinSet::new();
        let up = line(&mut tasks, config, Target::Upstream(back.clone()));
        let down = line(&mut tasks, config, Target::Client(front.clone(), client.clone()));
        tasks.spawn(async move {
            let mut buf = vec![0u8; 65536];
            while let Ok((n, from)) = front.recv_from(&mut buf).await {
                *client.lock().unwrap() = Some(from);
                if up.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });
        tasks.spawn(async move {
            let mut buf = vec![0u8; 65536];
            loop {
                // ICMP port-unreachable surfaces as a recv error; keep going.
                if let Ok(n) = back.recv(&mut buf).await
                    && down.send(buf[..n].to_vec()).is_err()
                {
                    break;
                }
            }
        });
        Ok(Proxy { addr, _tasks: tasks })
    }

    /// Client-facing address.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
}

/// One direction: stamps a deadline on arrival and sends at that time.
/// Without jitter a single FIFO task keeps packet order.
fn line(tasks: &mut JoinSet<()>, config: ProxyConfig, target: Target) -> mpsc::UnboundedSender<Vec<u8>> {
    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (fifo_tx, mut fifo_rx) = mpsc::unbounded_channel::<(Instant, Vec<u8>)>();
    let fifo_target = target.clone();
    tasks.spawn(async move {
        while let Some((at, packet)) = fifo_rx.recv().await {
            deliver(at, &fifo_target, &packet).await;
        }
    });
    tasks.spawn(async move {
        while let Some(packet) = rx.recv().await {
            let Some(at) = config.deadline() else { continue };
            if config.jitter.is_zero() {
                let _ = fifo_tx.send((at, packet));
            } else {
                let target = target.clone();
                tokio::spawn(async move { deliver(at, &target, &packet).await });
            }
        }
    });
    tx
}

/// Timer wakeups overshoot by a few ms (tick rounding plus macOS timer
/// coalescing), so sleep to just before the deadline and yield-spin the rest.
async fn deliver(at: Instant, target: &Target, packet: &[u8]) {
    if let Some(early) = at.checked_sub(SPIN) {
        sleep_until(early).await;
    }
    while Instant::now() < at {
        tokio::task::yield_now().await;
    }
    target.send(packet).await;
}

const SPIN: Duration = Duration::from_millis(5);
