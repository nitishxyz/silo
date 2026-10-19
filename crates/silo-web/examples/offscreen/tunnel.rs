//! Tunnel proxy check, run as the very first browser (like a remote pane on app launch). Every
//! test proxy demands Basic proxy auth (407).
//! - Like silo-app's open: a profile created for proxy B and immediately `set_tunnel(A)` (before
//!   its context initialized) loads `http://10.77.0.1/` from A; B sees nothing.
//! - A runtime `set_tunnel(B)` loads a new private host from B with B's credentials.
//! - Back on A, `http://localhost:<dead port>/` and its websocket (as Vite HMR uses) go through A.
//! - A 401 from that origin is never answered.
//! - Wrong credentials fail after a bounded number of retries.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, ensure};
use silo_web::{BrowserOptions, Runtime, Tunnel, ViewSize};

use crate::{pump_for, server, wait_for};

const ORANGE: [u8; 4] = [0, 128, 255, 255];
const PURPLE: [u8; 4] = [255, 0, 128, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const SIZE: (u32, u32) = (100, 100);

fn tunnel(port: u16, user: &str, pass: &str) -> Tunnel {
    Tunnel { port, user: user.into(), pass: pass.into() }
}

pub fn run(runtime: &Runtime) -> Result<()> {
    let a = server::spawn(server::page("255,128,0"), Some("user-a:pass-a"))?;
    let b = server::spawn(server::page("128,0,255"), Some("user-b:pass-b"))?;
    let c = server::spawn(server::page("0,0,0"), Some("user-c:pass-c"))?;
    let profile = runtime.profile("example-server/default", Some(tunnel(b.port, "user-b", "pass-b")))?;
    profile.set_tunnel(Some(tunnel(a.port, "user-a", "pass-a")))?;
    let options = BrowserOptions {
        url: "http://10.77.0.1/",
        profile: &profile,
        size: ViewSize { width: 100, height: 100, scale: 1.0 },
        notify: Arc::new(|| {}),
    };
    let browser = runtime.create_browser(options)?;
    let result = (|| {
        wait_for(runtime, &browser, ORANGE, SIZE, "set_tunnel before context init, first browser (A)")?;
        ensure!(b.requests.lock().unwrap().is_empty(), "stale tunnel B was used");
        profile.set_tunnel(Some(tunnel(b.port, "user-b", "pass-b")))?;
        browser.navigate("http://10.77.0.2/");
        wait_for(runtime, &browser, PURPLE, SIZE, "authenticated proxy switched at runtime (B)")?;
        profile.set_tunnel(Some(tunnel(a.port, "user-a", "pass-a")))?;
        // Nothing listens on `dead` locally: page and websocket can only come through the proxy.
        let dead = std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
        browser.navigate(&format!("http://localhost:{dead}/"));
        wait_for(runtime, &browser, GREEN, SIZE, "localhost page + websocket via tunnel (A)")?;
        browser.navigate(&format!("http://localhost:{dead}/private"));
        pump_for(runtime, Duration::from_secs(2));
        let seen = a.requests.lock().unwrap().clone();
        println!("offscreen: proxy A saw {seen:?}");
        let host = format!("localhost:{dead}");
        let authed = |prefix: &str| seen.iter().any(|l| l.starts_with(prefix) && l.contains(&host));
        ensure!(authed("[proxy-auth] GET http://"), "localhost GET not proxy-authenticated");
        ensure!(authed("[proxy-auth] CONNECT "), "websocket CONNECT not proxy-authenticated");
        ensure!(seen.iter().any(|l| l.contains("/private ")), "origin 401 page not requested");
        ensure!(!seen.iter().any(|l| l.starts_with("[auth]")), "answered a non-proxy challenge");
        println!("offscreen: origin 401 challenge left unanswered");

        profile.set_tunnel(Some(tunnel(c.port, "user-c", "wrong")))?;
        browser.navigate("http://10.77.0.3/");
        pump_for(runtime, Duration::from_secs(3));
        let seen = c.requests.lock().unwrap().clone();
        let bad = seen.iter().filter(|l| l.starts_with("[bad-proxy-auth]")).count();
        println!("offscreen: wrong credentials: proxy C saw {} requests, {bad} with bad auth", seen.len());
        ensure!(!seen.iter().any(|l| l.starts_with("[proxy-auth]")), "C accepted wrong credentials");
        // silo-web answers at most 4 proxy challenges per second: over 3s that's at most 16
        // (unbounded, Chromium retries ~20 times per second).
        ensure!((1..=16).contains(&bad), "wrong credentials retried {bad} times");
        anyhow::Ok(())
    })();
    browser.close();
    result
}
