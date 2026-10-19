//! Tunnel PAC check: a profile proxied to test server A loads `http://10.77.0.1/` from A; after a
//! runtime `set_tunnel_port(B)` a new private host loads from B. Then probes `app.localhost`.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use silo_web::{BrowserOptions, Runtime, ViewSize};

use crate::{pump_for, server, wait_for};

const ORANGE: [u8; 4] = [0, 128, 255, 255];
const PURPLE: [u8; 4] = [255, 0, 128, 255];

pub fn run(runtime: &Runtime) -> Result<()> {
    let a = server::spawn(server::page("255,128,0"))?;
    let b = server::spawn(server::page("128,0,255"))?;
    let profile = runtime.profile("example-server/default", Some(a.port))?;
    let size = ViewSize {
        width: 100,
        height: 100,
        scale: 1.0,
    };
    let options = BrowserOptions {
        url: "http://10.77.0.1/",
        profile: &profile,
        size,
        notify: Arc::new(|| {}),
    };
    let browser = runtime.create_browser(options)?;
    let result = (|| {
        wait_for(
            runtime,
            &browser,
            ORANGE,
            (100, 100),
            "tunnel PAC at context init (server A)",
        )?;
        println!("offscreen: server A saw {:?}", a.requests.lock().unwrap());
        profile.set_tunnel_port(Some(b.port))?;
        browser.navigate("http://10.77.0.2/");
        wait_for(
            runtime,
            &browser,
            PURPLE,
            (100, 100),
            "tunnel PAC set at runtime (server B)",
        )?;
        browser.navigate("http://app.localhost/");
        pump_for(runtime, Duration::from_secs(3));
        let requests = b.requests.lock().unwrap().clone();
        let proxied = requests.iter().any(|line| line.contains("app.localhost"));
        println!("offscreen: server B saw {requests:?}; app.localhost proxied: {proxied}");
        anyhow::Ok(())
    })();
    browser.close();
    result
}
