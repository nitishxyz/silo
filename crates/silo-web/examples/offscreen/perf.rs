//! Frame pacing, input latency and idle cost (`--perf`) on an 800x600@2x
//! page of text, pumped like the app: when CEF asks, a 33 ms tick, and a
//! frame asked for on every display refresh (`FPS`, default the display's),
//! as a window showing the page does. `DRIVE=none`: no window, only
//! `Runtime::drive_pages`. Scrolling: a wheel step every 8.3 ms (a 120 Hz
//! trackpad) for 2 s; latency: a wheel step on an idle page to its paint.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use silo_web::{Browser, BrowserOptions, Event, Modifiers, Runtime, ViewSize};

use crate::wait_event;

/// Text to scroll; records when its main thread saw wheel events and frames.
fn page() -> String {
    let text = "<p>Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor.</p>".repeat(400);
    format!(
        "data:text/html,<html><body style='margin:0 20px'>{text}<script>window.w=[];window.f=[];\
         addEventListener('wheel',()=>w.push(performance.now()),{{passive:true}});\
         (function r(t){{f.push(t);requestAnimationFrame(r)}})()</script></body></html>"
    )
}

const RAF: &str = "data:text/html,<html><body style='margin:0'><div id=d style='width:100px;height:100px;background:red'></div>\
    <script>let i=0;function f(){d.style.transform='translateX('+(i=(i+1)*(i<500))+'px)';requestAnimationFrame(f)}f()</script></body></html>";

/// CPU time (user + system) of this, the browser, process.
fn cpu() -> Duration {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    // SAFETY: getrusage fills the zeroed struct.
    let u = unsafe {
        libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr());
        usage.assume_init()
    };
    let t = |v: libc::timeval| Duration::from_secs(v.tv_sec as u64) + Duration::from_micros(v.tv_usec as u64);
    t(u.ru_utime) + t(u.ru_stime)
}

/// CPU time of this process's children (CEF's renderer, GPU and utility helpers).
fn helpers_cpu() -> Duration {
    let out = std::process::Command::new("ps").args(["-A", "-o", "ppid=,cputime="]).output().map(|o| o.stdout).unwrap_or_default();
    let me = std::process::id().to_string();
    String::from_utf8_lossy(&out)
        .lines()
        .filter_map(|l| l.trim().split_once(' ').filter(|(ppid, _)| *ppid == me).map(|(_, t)| t.trim().to_owned()))
        .map(|t| Duration::from_secs_f64(t.split(':').fold(0.0, |acc, part| acc * 60.0 + part.parse::<f64>().unwrap_or(0.0))))
        .sum()
}

/// The display's refresh, asking `browser` for a frame on each (unless `DRIVE=none`).
struct Vsync {
    period: Duration,
    next: Instant,
    drive: bool,
}

impl Vsync {
    fn step(&mut self, runtime: &Runtime, browser: &Browser) {
        if self.drive && Instant::now() >= self.next {
            self.next += self.period;
            browser.begin_frame();
        }
        crate::pump_precise(runtime);
    }

    fn run_for(&mut self, runtime: &Runtime, browser: &Browser, d: Duration) {
        let start = Instant::now();
        while start.elapsed() < d {
            self.step(runtime, browser);
        }
    }
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn stats(name: &str, mut v: Vec<f64>) {
    if v.is_empty() {
        println!("offscreen: perf: {name}: none");
        return;
    }
    v.sort_by(f64::total_cmp);
    let mean = v.iter().sum::<f64>() / v.len() as f64;
    let at = |p: f64| v[((v.len() - 1) as f64 * p).round() as usize];
    println!("offscreen: perf: {name}: n={} mean={mean:.1} p50={:.1} p95={:.1} max={:.1} ms", v.len(), at(0.5), at(0.95), at(1.0));
}

fn intervals(paints: &[Instant]) -> Vec<f64> {
    paints.windows(2).map(|w| ms(w[1] - w[0])).collect()
}

/// A windowless accessory app is napped (its timers coalesced); the app is
/// in front when it scrolls a page.
fn no_nap() {
    use objc2::msg_send;
    use objc2::runtime::{AnyClass, AnyObject};
    let reason = objc2_foundation::NSString::from_str("perf");
    // NSActivityUserInitiated | NSActivityLatencyCritical; kept for the process's life.
    unsafe {
        let info: *mut AnyObject = msg_send![AnyClass::get(c"NSProcessInfo").unwrap(), processInfo];
        let activity: *mut AnyObject = msg_send![info, beginActivityWithOptions: 0x00FF_FFFFu64 | 0xFF_0000_0000u64, reason: &*reason];
        let _: *mut AnyObject = msg_send![activity, retain];
    }
}

pub fn run(runtime: &Runtime) -> Result<()> {
    no_nap();
    let profile = runtime.profile("default", None)?;
    let paints: Arc<Mutex<Vec<Instant>>> = Arc::default();
    let log = paints.clone();
    let notify = Arc::new(move || log.lock().unwrap().push(Instant::now()));
    let size = ViewSize { width: 800, height: 600, scale: 2.0 };
    let url = page();
    let browser = runtime.create_browser(BrowserOptions { url: &url, profile: &profile, size, offscreen: false, notify })?;
    let fps: u32 = std::env::var("FPS").ok().and_then(|f| f.parse().ok()).unwrap_or_else(silo_web::display_rate);
    let drive = std::env::var("DRIVE").map_or(true, |d| d != "none");
    println!(
        "offscreen: perf: {fps} fps (display {} Hz), frames {}",
        silo_web::display_rate(),
        if drive { "asked each refresh" } else { "undriven" }
    );
    let mut vsync = Vsync { period: Duration::from_secs(1) / fps, next: Instant::now(), drive };
    let result = (|| {
        wait_event(runtime, &browser, "perf page", |e| matches!(e, Event::Loading(false)))?;
        browser.set_frame_rate(fps);
        vsync.run_for(runtime, &browser, Duration::from_millis(500));
        browser.take_events();

        paints.lock().unwrap().clear();
        let (t, c, h) = (Instant::now(), cpu(), helpers_cpu());
        vsync.run_for(runtime, &browser, Duration::from_secs(5));
        let secs = t.elapsed().as_secs_f64();
        let (browser_cpu, helpers) = ((cpu() - c).as_secs_f64() / secs, (helpers_cpu() - h).as_secs_f64() / secs);
        println!(
            "offscreen: perf: idle 5 s: {} paints, CPU browser {:.1}% helpers {:.1}%",
            paints.lock().unwrap().len(),
            browser_cpu * 100.0,
            helpers * 100.0
        );

        browser.mouse_move(400.0, 300.0, Modifiers::default(), &[]);
        paints.lock().unwrap().clear();
        let (t, h) = (Instant::now(), helpers_cpu());
        let mut next = t;
        let mut surfaces: Vec<u32> = Vec::new();
        while t.elapsed() < Duration::from_secs(2) {
            if Instant::now() >= next {
                browser.wheel(400.0, 300.0, 0.0, -3.0, Modifiers::default());
                next += Duration::from_micros(8333);
            }
            vsync.step(runtime, &browser);
            if let Some(id) = browser.frame().map(|f| f.surface_id())
                && surfaces.last() != Some(&id)
            {
                surfaces.push(id);
            }
        }
        let got = paints.lock().unwrap().clone();
        let helpers = (helpers_cpu() - h).as_secs_f64() / t.elapsed().as_secs_f64();
        println!("offscreen: perf: scroll 2 s: {} paints ({:.1} fps), CPU helpers {:.1}%", got.len(), got.len() as f64 / 2.0, helpers * 100.0);
        stats("scroll paint interval", intervals(&got));
        // How soon CEF paints into a surface again (the app holds the latest
        // one until it drew it).
        let reuse = surfaces.iter().enumerate().filter_map(|(i, id)| surfaces[..i].iter().rposition(|p| p == id).map(|j| i - j)).min();
        let distinct = surfaces.iter().collect::<std::collections::HashSet<_>>().len();
        println!("offscreen: perf: surfaces: {distinct} in turn, one reused after {reuse:?} frames at the soonest");
        let seen = crate::pinch::cdp(
            runtime,
            &browser,
            "Runtime.evaluate",
            serde_json::json!({"expression": "[w.slice(-200), f.slice(-200)].map(a => a.slice(1).map((t, i) => t - a[i]))", "returnByValue": true}),
        )?;
        let gaps = |v: &serde_json::Value| v.as_array().map(|a| a.iter().filter_map(serde_json::Value::as_f64).collect()).unwrap_or_default();
        stats("page: wheel event interval", gaps(&seen["result"]["value"][0]));
        stats("page: rAF interval", gaps(&seen["result"]["value"][1]));
        vsync.run_for(runtime, &browser, Duration::from_millis(300));

        let mut latency = Vec::new();
        for i in 0..40u64 {
            vsync.run_for(runtime, &browser, Duration::from_millis(60 + (i * 37) % 90));
            paints.lock().unwrap().clear();
            let sent = Instant::now();
            browser.wheel(400.0, 300.0, 0.0, if i % 2 == 0 { -20.0 } else { 20.0 }, Modifiers::default());
            while paints.lock().unwrap().is_empty() && sent.elapsed() < Duration::from_millis(500) {
                vsync.step(runtime, &browser);
            }
            if let Some(&p) = paints.lock().unwrap().first() {
                latency.push(ms(p - sent));
            }
        }
        stats("wheel → paint", latency);

        browser.navigate(RAF);
        wait_event(runtime, &browser, "rAF page", |e| matches!(e, Event::Loading(false)))?;
        vsync.run_for(runtime, &browser, Duration::from_millis(500));
        paints.lock().unwrap().clear();
        vsync.run_for(runtime, &browser, Duration::from_secs(2));
        let got = paints.lock().unwrap().clone();
        println!("offscreen: perf: rAF 2 s: {} paints ({:.1} fps)", got.len(), got.len() as f64 / 2.0);
        stats("rAF paint interval", intervals(&got));
        Ok(())
    })();
    browser.close();
    result
}
