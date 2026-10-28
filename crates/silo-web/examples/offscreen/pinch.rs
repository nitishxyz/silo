//! Pinch zoom, as the app sends it (`gesture.rs`), and scrolling after it
//! (`--pinch`, also part of the full run): an 800x600@1x tall page with a
//! cross-site frame under the fingers once zoomed. Fast pinches end with
//! the fingers moving: released, Chromium flings, and a fling over that
//! frame left its touch scroll running, which swallowed every later wheel
//! scroll; the app cancels its touches instead.

use std::time::Duration;

use anyhow::{Result, ensure};
use silo_web::{Browser, BrowserOptions, Event, Modifiers, Pinch, Runtime, ViewSize};

use crate::{pump_for, wait_event};

/// Title `s<scale>:<visual viewport left>:<top>` (page px) after every zoom or scroll.
fn page(frame_port: u16) -> String {
    format!(
        "data:text/html,<html><head><title>s</title></head><body style='margin:0'>\
         <iframe src='http://127.0.0.1:{frame_port}/' style='position:fixed;left:0;top:0;width:300px;height:600px;border:0'></iframe>\
         <div style='height:5000px;background:linear-gradient(rgb(255,0,0),rgb(0,0,255))'></div><script>\
         function r(){{const v=visualViewport;document.title='s'+v.scale.toFixed(2)+':'+Math.round(scrollX+v.offsetLeft)+':'+Math.round(scrollY+v.offsetTop)}}\
         visualViewport.onresize=r;visualViewport.onscroll=r;onscroll=r;r()</script></body></html>"
    )
}

#[derive(Clone, Copy, Debug, Default)]
struct View {
    scale: f32,
    left: f32,
    top: f32,
}

/// The page's view after pumping `ms`.
fn view(runtime: &Runtime, browser: &Browser, last: &mut View, ms: u64) -> View {
    pump_for(runtime, Duration::from_millis(ms));
    for e in browser.take_events() {
        if let Event::TitleChanged(t) = e
            && let Some(rest) = t.strip_prefix('s')
            && let [s, x, y] = rest.split(':').collect::<Vec<_>>()[..]
        {
            *last = View { scale: s.parse().unwrap_or(last.scale), left: x.parse().unwrap_or(last.left), top: y.parse().unwrap_or(last.top) };
        }
    }
    *last
}

fn points((x, y): (f32, f32), r: f32) -> [(f32, f32); 2] {
    [(x - r, y), (x + r, y)]
}

/// A quick pinch to `to`× the start span (4 steps at 120 Hz), the touches
/// down 16 pt inside the 200 pt radius, then lifted with `end`.
fn pinch(runtime: &Runtime, browser: &Browser, center: (f32, f32), to: f32, end: Pinch) {
    browser.pinch(Pinch::Start, points(center, 184.0));
    browser.pinch(Pinch::Move, points(center, 200.0));
    for i in 1..=4 {
        browser.pinch(Pinch::Move, points(center, 200.0 * to.powf(i as f32 / 4.0)));
        pump_for(runtime, Duration::from_millis(8));
    }
    browser.pinch(end, points(center, 200.0 * to));
}

fn scroll(runtime: &Runtime, browser: &Browser) {
    for _ in 0..20 {
        browser.wheel(400.0, 300.0, 0.0, -10.0, Modifiers::default());
        pump_for(runtime, Duration::from_millis(8));
    }
}

/// Runs a CDP call to completion, pumping.
pub fn cdp(runtime: &Runtime, browser: &Browser, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
    let mut future = std::pin::pin!(browser.cdp(method, params));
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    let started = std::time::Instant::now();
    loop {
        if let std::task::Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
        crate::pump_once(runtime);
        ensure!(started.elapsed() < Duration::from_secs(10), "{method}: no reply");
    }
}

pub fn run(runtime: &Runtime) -> Result<()> {
    let frame = crate::server::spawn(crate::server::page("0,200,0"), None)?;
    let profile = runtime.profile("default", None)?;
    let size = ViewSize { width: 800, height: 600, scale: 1.0 };
    let url = page(frame.port);
    let browser =
        runtime.create_browser(BrowserOptions { url: &url, profile: &profile, size, offscreen: false, notify: std::sync::Arc::new(|| {}) })?;
    let result = (|| {
        wait_event(runtime, &browser, "pinch page", |e| matches!(e, Event::Loading(false)))?;
        let mut last = View { scale: 1.0, ..View::default() };
        let center = (400.0, 300.0);
        pinch(runtime, &browser, center, 2.0, Pinch::Cancel);
        let zoomed = view(runtime, &browser, &mut last, 500);
        println!("offscreen: pinch: zoomed {zoomed:?}");
        ensure!(zoomed.scale > 1.8, "pinch did not zoom");
        scroll(runtime, &browser);
        let scrolled = view(runtime, &browser, &mut last, 500);
        println!("offscreen: pinch: scrolled {scrolled:?}");
        ensure!(scrolled.top > zoomed.top + 50.0, "no scroll after a pinch");
        // Scrolling while pinching: both fingers move up, the page with them.
        browser.pinch(Pinch::Start, points(center, 184.0));
        browser.pinch(Pinch::Move, points(center, 200.0));
        browser.pinch(Pinch::Move, points(center, 220.0));
        pump_for(runtime, Duration::from_millis(8));
        for i in 1..=10 {
            browser.pinch(Pinch::Move, points((center.0, center.1 - 10.0 * i as f32), 220.0));
            pump_for(runtime, Duration::from_millis(8));
        }
        browser.pinch(Pinch::Cancel, points((center.0, center.1 - 100.0), 220.0));
        let panned = view(runtime, &browser, &mut last, 500);
        println!("offscreen: pinch: panned {panned:?}");
        ensure!(panned.top > scrolled.top + 20.0, "fingers moving up didn't pan the page");
        pinch(runtime, &browser, center, 0.4, Pinch::Cancel);
        let out = view(runtime, &browser, &mut last, 500);
        println!("offscreen: pinch: out {out:?}");
        ensure!(out.scale < 1.05, "pinching in did not zoom out");
        scroll(runtime, &browser);
        let again = view(runtime, &browser, &mut last, 500);
        println!("offscreen: pinch: scrolled again {again:?}");
        ensure!(again.top > out.top + 100.0, "no scroll after pinching back");
        // Scroll, pinch right away and scroll on: the first scroll's wheel
        // latch (500 ms) outlives the pinch, and swallowed the second.
        scroll(runtime, &browser);
        pinch(runtime, &browser, center, 1.5, Pinch::Cancel);
        let before = view(runtime, &browser, &mut last, 200);
        scroll(runtime, &browser);
        let after = view(runtime, &browser, &mut last, 500);
        println!("offscreen: pinch: scroll, pinch, scroll {before:?} → {after:?}");
        ensure!(after.top > before.top + 100.0, "no scroll right after a pinch");
        // Touches left down (an end that never came) swallow wheel scrolls;
        // cancelled late, scrolling works again.
        browser.pinch(Pinch::Start, points(center, 184.0));
        browser.pinch(Pinch::Move, points(center, 200.0));
        scroll(runtime, &browser);
        let stuck = view(runtime, &browser, &mut last, 300);
        browser.pinch(Pinch::Cancel, points(center, 200.0));
        pump_for(runtime, Duration::from_millis(100));
        scroll(runtime, &browser);
        let freed = view(runtime, &browser, &mut last, 500);
        println!("offscreen: pinch: touches down {stuck:?}, lifted {freed:?}");
        ensure!(freed.top > stuck.top + 100.0, "no scroll after lifting stale touches");
        Ok(())
    })();
    browser.close();
    result
}
