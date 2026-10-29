//! A page hidden from its creation (a tab opened behind) holds back its
//! media until shown: an autoplaying `<video>` asks for nothing while
//! hidden and loads once the page is shown. Hidden pages don't paint, so the
//! test server's request log tells.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail, ensure};
use silo_web::{BrowserOptions, Runtime, ViewSize};

use crate::{pump_for, pump_once, server};

const PAGE: &str = "<html><body style='margin:0;background:rgb(64,64,64)'>\
    <video src='/clip.wav' autoplay muted></video></body></html>";

fn media_asked(requests: &std::sync::Mutex<Vec<String>>) -> bool {
    requests.lock().unwrap().iter().any(|r| r.contains("clip.wav"))
}

pub fn run(runtime: &Runtime) -> Result<()> {
    let origin = server::spawn(PAGE.to_owned(), None)?;
    let profile = runtime.profile("default", None)?;
    let size = ViewSize { width: 400, height: 300, scale: 1.0 };
    let url = format!("http://127.0.0.1:{}/", origin.port);
    let browser = runtime.create_browser(BrowserOptions { url: &url, profile: &profile, size, offscreen: true, notify: Arc::new(|| {}) })?;
    // At once, before CEF has made the browser: as the app does for a tab opened behind.
    browser.set_hidden(true);
    let result = (|| {
        let started = Instant::now();
        while !origin.requests.lock().unwrap().iter().any(|r| r.starts_with("GET / ")) {
            pump_once(runtime);
            if started.elapsed() > Duration::from_secs(20) {
                bail!("the page was never requested");
            }
        }
        pump_for(runtime, Duration::from_secs(3));
        ensure!(!media_asked(&origin.requests), "hidden: the video loaded anyway: {:?}", origin.requests.lock().unwrap());
        println!("offscreen: hidden: the page loaded, its video waited 3 s");
        browser.set_hidden(false);
        let shown = Instant::now();
        while !media_asked(&origin.requests) {
            pump_once(runtime);
            if shown.elapsed() > Duration::from_secs(20) {
                bail!("shown: the video never loaded: {:?}", origin.requests.lock().unwrap());
            }
        }
        println!("offscreen: hidden: shown, the video loaded after {:?}", shown.elapsed());
        Ok(())
    })();
    browser.close();
    pump_for(runtime, Duration::from_millis(500));
    result
}
