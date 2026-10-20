//! Latency harness: a UDP delay proxy and the headless keystroke bench.

pub mod bench;
pub mod keys;
pub mod proxy;
pub mod stages;

pub use bench::{Link, Password, Report, Row, Stats, bench, bench_with};
pub use proxy::{Proxy, ProxyConfig};
