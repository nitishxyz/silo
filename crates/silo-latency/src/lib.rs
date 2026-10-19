//! Latency harness: a UDP delay proxy and the headless keystroke bench.

pub mod bench;
pub mod keys;
pub mod proxy;

pub use bench::{Password, Report, Row, Stats, bench};
pub use proxy::{Proxy, ProxyConfig};
