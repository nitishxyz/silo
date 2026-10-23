//! Smoke test: init CEF windowless, load solid-colour pages, wait for accelerated paints and check
//! the IOSurface centre pixel after input, navigation and tunnel-PAC loads. Re-execs itself inside a dev
//! `.app` (CEF needs the bundle layout). `cargo run -p silo-web --example offscreen`

mod bundle;
mod nav;
mod server;
mod tunnel;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use objc2::{MainThreadMarker, sel};
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::{NSDate, NSObjectProtocol, NSRunLoop};
use silo_web::{Browser, BrowserOptions, Config, Event, KeyInput, Modifiers, MouseButton, Runtime, ViewSize};

/// Pink; a mouse press turns it green, a key press blue.
const PAGE: &str = "data:text/html,<html><body style='margin:0;height:100vh;background:rgb(255,0,128)' \
    onmousedown=\"document.body.style.background='rgb(0,255,0)'\" \
    onkeydown=\"document.body.style.background='rgb(0,0,255)'\"></body></html>";
const PINK: [u8; 4] = [128, 0, 255, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const BLUE: [u8; 4] = [255, 0, 0, 255];

fn main() -> Result<()> {
    if std::env::args().any(|arg| arg.starts_with("--type=")) {
        std::process::exit(silo_web::run_helper());
    }
    let exe = std::env::current_exe()?;
    if silo_web::layout::Layout::for_main(&exe).is_err() {
        let cef_dir = PathBuf::from(option_env!("CEF_PATH").context("build with CEF_PATH set")?);
        let dir = exe.parent().context("exe dir")?.join("silo-web-dev");
        let bundled = bundle::assemble(&dir, &exe, &cef_dir)?;
        println!("offscreen: running from {}", bundled.display());
        let status = std::process::Command::new(&bundled).args(std::env::args().skip(1)).status()?;
        std::process::exit(status.code().unwrap_or(1));
    }
    run()
}

fn pixels_match(pixel: [u8; 4], expected: [u8; 4]) -> bool {
    pixel.iter().zip(expected).all(|(a, b)| a.abs_diff(b) <= 2)
}

/// Earliest pump CEF asked for; the example pumps exactly like silo-app (`web.rs`): when due,
/// plus a 33 ms safety tick while browsers are live, never otherwise.
static DUE: Mutex<Option<Instant>> = Mutex::new(None);
static LAST: Mutex<Option<Instant>> = Mutex::new(None);

fn schedule(delay: Duration) {
    let at = Instant::now() + delay;
    let mut due = DUE.lock().unwrap();
    if due.is_none_or(|d| at < d) {
        *due = Some(at);
    }
}

fn pump_once(runtime: &Runtime) {
    NSRunLoop::currentRunLoop().runUntilDate(&NSDate::dateWithTimeIntervalSinceNow(0.004));
    let now = Instant::now();
    let due = {
        let mut due = DUE.lock().unwrap();
        let ready = due.is_some_and(|d| d <= now);
        if ready {
            *due = None;
        }
        ready
    };
    let mut last = LAST.lock().unwrap();
    let tick = Runtime::live_browsers() > 0 && last.is_none_or(|l| now - l >= Duration::from_millis(33));
    if due || tick {
        *last = Some(now);
        runtime.pump();
    }
}

fn pump_for(runtime: &Runtime, duration: Duration) {
    let started = Instant::now();
    while started.elapsed() < duration {
        pump_once(runtime);
    }
}

/// Pumps until `browser` emits an event matching `pred` (other events are dropped).
fn wait_event(runtime: &Runtime, browser: &Browser, what: &str, pred: impl Fn(&Event) -> bool) -> Result<()> {
    let started = Instant::now();
    loop {
        pump_once(runtime);
        if let Some(event) = browser.take_events().into_iter().find(|event| pred(event)) {
            println!("offscreen: {what}: {event:?} after {:?}", started.elapsed());
            return Ok(());
        }
        if started.elapsed() > Duration::from_secs(20) {
            bail!("{what}: no matching event in 20s");
        }
    }
}

/// Pumps until the latest accelerated paint is `px` pixels with centre pixel `expected`.
fn wait_for(runtime: &Runtime, browser: &Browser, expected: [u8; 4], px: (u32, u32), what: &str) -> Result<()> {
    let started = Instant::now();
    loop {
        pump_once(runtime);
        let last = browser.frame().map(|f| (f.width, f.height, f.pixel(f.width / 2, f.height / 2)));
        if let Some((w, h, Some(pixel))) = last
            && (w, h) == px
            && pixels_match(pixel, expected)
        {
            println!("offscreen: {what}: {w}x{h} IOSurface, centre BGRA {pixel:?} after {:?}", started.elapsed());
            return Ok(());
        }
        if started.elapsed() > Duration::from_secs(20) {
            bail!("{what}: no matching paint in 20s (last frame: {last:?})");
        }
    }
}

fn run() -> Result<()> {
    let mtm = MainThreadMarker::new().context("main thread")?;
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    app.finishLaunching();

    let root = std::env::temp_dir().join("silo-web-offscreen");
    let config = Config {
        root,
        schedule: Arc::new(schedule),
        switches: ["use-mock-keychain".to_owned()]
            .into_iter()
            .chain(std::env::var("SILO_WEB_SWITCHES").ok().into_iter().flat_map(|s| s.split_whitespace().map(str::to_owned).collect::<Vec<_>>()))
            .collect(),
    };
    let runtime = Runtime::init(config)?;
    let handling = app.respondsToSelector(sel!(isHandlingSendEvent));
    println!("offscreen: CEF initialized; NSApp answers isHandlingSendEvent: {handling}");

    // Like the app on launch: the engine idles (pumped only when CEF asks) before the first
    // page, longer than the helpers' 15 s connection timeout; then a tunnelled profile opens.
    let idle = Duration::from_secs(20);
    pump_for(&runtime, idle);
    println!("offscreen: idled {idle:?} with no browser");
    if let Err(err) = tunnel::run(&runtime) {
        runtime.shutdown(Duration::from_secs(5))?;
        return Err(err);
    }

    let profile = runtime.profile("default", None)?;
    let size = ViewSize { width: 320, height: 200, scale: 2.0 };
    let options = BrowserOptions { url: PAGE, profile: &profile, size, offscreen: false, notify: Arc::new(|| {}) };
    let browser = runtime.create_browser(options)?;

    let result = (|| {
        wait_for(&runtime, &browser, PINK, (640, 400), "first paint")?;
        let none = Modifiers::default();
        browser.mouse_move(160.0, 100.0, none, &[]);
        browser.mouse_button(160.0, 100.0, MouseButton::Left, true, 1, none);
        browser.mouse_button(160.0, 100.0, MouseButton::Left, false, 1, none);
        wait_for(&runtime, &browser, GREEN, (640, 400), "after mouse click")?;
        browser.set_focus(true);
        browser.key(&KeyInput { key: "a", key_char: Some("a"), modifiers: none, down: true, repeat: false });
        browser.key(&KeyInput { key: "a", key_char: Some("a"), modifiers: none, down: false, repeat: false });
        wait_for(&runtime, &browser, BLUE, (640, 400), "after key press")?;
        browser.resize(ViewSize { width: 200, height: 100, scale: 1.0 });
        wait_for(&runtime, &browser, BLUE, (200, 100), "after resize to 200x100@1x")?;
        nav::run(&runtime, &browser)
    })();
    println!("offscreen: events {:?}", browser.take_events());
    browser.close();
    drop(browser);
    runtime.shutdown(Duration::from_secs(5))?;
    println!("offscreen: shut down cleanly");
    result
}
