//! Latency harness: a UDP delay proxy and the headless keystroke bench.

pub mod bench;
pub mod proxy;

pub use bench::{Report, Row, Stats, bench};
pub use proxy::{Proxy, ProxyConfig};
