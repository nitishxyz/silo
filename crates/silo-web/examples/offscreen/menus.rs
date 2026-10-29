//! The context menu's engine features on a 400x300@1x page over http: the
//! link text the menu script reports, media actions on a video (loop, mute,
//! play, controls) seen in the page and in the next menu's flags, a Save As
//! download and a page-started one into a temp folder, the print panel, a
//! DevTools window, and a `view-source:` page.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail, ensure};
use objc2::{msg_send, runtime::AnyObject, sel};
use objc2_foundation::NSString;
use silo_web::{Browser, BrowserOptions, DownloadState, Event, MediaAction, MediaKind, Runtime, ViewSize};

use crate::popups::{menu_at, next};
use crate::{pump_once, server, wait_for};

/// Grey once the video's metadata loaded; then red for loop, green for
/// muted, blue for playing (half once it shows controls). An audio-only
/// source makes Chromium call the element audio.
const PAGE: &str = "<html><body style='margin:0;height:100vh;background:rgb(0,0,0)'>\
    <a href='/next' style='position:absolute;left:0;top:0;width:100px;height:100px'>Read  the\n docs</a>\
    <video id=v src='/a.wav' style='position:absolute;left:100px;top:0;width:100px;height:100px;background:#000'></video>\
    <script>const v = document.getElementById('v'), s = document.body.style;\
    setInterval(() => { if (v.readyState < 1) return;\
    const c = (on, a, b) => on ? a : b;\
    s.background = `rgb(${c(v.loop, 255, 64)},${c(v.muted, 255, 64)},${c(v.paused, 64, c(v.controls, 128, 255))})` }, 50);\
    </script></body></html>";

const SIZE: (u32, u32) = (400, 300);
const VIDEO: (f32, f32) = (150.0, 50.0);

fn bgra(r: u8, g: u8, b: u8) -> [u8; 4] {
    [b, g, r, 255]
}

pub fn run(runtime: &Runtime) -> Result<()> {
    let origin = server::spawn(PAGE.to_owned(), None)?;
    let base = format!("http://127.0.0.1:{}", origin.port);
    let profile = runtime.profile("default", None)?;
    let size = ViewSize { width: SIZE.0, height: SIZE.1, scale: 1.0 };
    let options = BrowserOptions { url: &format!("{base}/"), profile: &profile, size, offscreen: false, notify: Arc::new(|| {}) };
    let browser = runtime.create_browser(options)?;
    let result = (|| {
        wait_for(runtime, &browser, bgra(64, 64, 64), SIZE, "video metadata loaded")?;
        browser.set_focus(true);
        let link = menu_at(runtime, &browser, (50.0, 50.0), "menu on a link")?;
        ensure!(link.link_text == "Read the docs", "link text: {:?}", link.link_text);
        browser.menu_command(None);
        media(runtime, &browser)?;
        downloads(runtime, &browser, &base)?;
        print(runtime, &browser)?;
        inspect(runtime, &browser)?;
        browser.navigate(&format!("view-source:{base}/"));
        let url = next(runtime, &browser, "view-source committed", |e| match e {
            Event::UrlChanged { url, .. } if url.starts_with("view-source:") => Some(url),
            _ => None,
        })?;
        println!("offscreen: view-source url {url}");
        // Its own page (light or dark with the system), not the last colour.
        let started = Instant::now();
        loop {
            pump_once(runtime);
            let pixel = browser.frame().and_then(|f| f.pixel(f.width / 2, f.height / 2));
            if pixel.is_some_and(|p| p == bgra(255, 255, 255) || p == [18, 18, 18, 255]) {
                println!("offscreen: view-source painted {pixel:?}");
                return Ok(());
            }
            if started.elapsed() > Duration::from_secs(20) {
                bail!("view-source page: last pixel {pixel:?}");
            }
        }
    })();
    browser.close();
    crate::pump_for(runtime, Duration::from_millis(500));
    result
}

/// Each action changes the page and shows in the next menu's flags.
fn media(runtime: &Runtime, browser: &Browser) -> Result<()> {
    let menu = menu_at(runtime, browser, VIDEO, "menu on a video")?;
    let video = menu.media.context("no media facts")?;
    ensure!(video.kind == MediaKind::Audio && video.src.ends_with("/a.wav") && video.state.paused && !video.state.looping, "video facts");
    let steps = [
        (MediaAction::Loop, bgra(255, 64, 64), "Loop"),
        (MediaAction::Mute, bgra(255, 255, 64), "Mute"),
        (MediaAction::PlayPause, bgra(255, 255, 255), "Play"),
    ];
    for (action, colour, what) in steps {
        browser.menu_command(None);
        browser.media(action);
        wait_for(runtime, browser, colour, SIZE, what)?;
        let menu = menu_at(runtime, browser, VIDEO, &format!("menu after {what}"))?;
        let state = menu.media.context("no media facts")?.state;
        let seen = match action {
            MediaAction::Loop => state.looping,
            MediaAction::Mute => state.muted,
            _ => !state.paused,
        };
        ensure!(seen, "{what}: the menu's flags don't show it ({state:?})");
    }
    browser.menu_command(None);
    browser.media(MediaAction::Controls);
    wait_for(runtime, browser, bgra(255, 255, 128), SIZE, "Show Controls")?;
    let menu = menu_at(runtime, browser, VIDEO, "menu after Show Controls")?;
    let state = menu.media.context("no media facts")?.state;
    ensure!(state.controls, "Show Controls: {state:?}");
    browser.menu_command(None);
    Ok(())
}

fn download_done(runtime: &Runtime, browser: &Browser, id: u32, what: &str) -> Result<std::path::PathBuf> {
    next(runtime, browser, what, |e| match e {
        Event::Download(p) if p.id == id && p.state != DownloadState::Running => Some(p),
        _ => None,
    })
    .and_then(|p| {
        ensure!(p.state == DownloadState::Done, "{what}: {:?}", p.state);
        p.path.context("no path")
    })
}

/// Save As names the file from the server; a page-started download gets
/// a free name next to it.
fn downloads(runtime: &Runtime, browser: &Browser, base: &str) -> Result<()> {
    let dir = std::env::temp_dir().join(format!("silo-web-downloads-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    browser.save_as(&format!("{base}/file.bin"), None);
    let asked = next(runtime, browser, "Save As asked", |e| if let Event::DownloadAsked(r) = e { Some(r) } else { None })?;
    ensure!(asked.save_as && asked.name == "report.txt", "Save As request: {asked:?}");
    let path = dir.join(&asked.name);
    browser.download_to(asked.id, Some(&path));
    let saved = download_done(runtime, browser, asked.id, "Save As done")?;
    ensure!(saved == path && std::fs::read(&path)? == server::DOWNLOAD, "Save As wrote {}", saved.display());
    let quarantine = std::process::Command::new("xattr").args(["-p", "com.apple.quarantine"]).arg(&path).output()?;
    println!("offscreen: quarantine on the download: {:?}", String::from_utf8_lossy(&quarantine.stdout).trim());

    browser.navigate(&format!("{base}/file2"));
    let asked = next(runtime, browser, "page download asked", |e| if let Event::DownloadAsked(r) = e { Some(r) } else { None })?;
    ensure!(!asked.save_as, "a page's download is not Save As");
    let path = silo_web::unique_path(&dir, &asked.name, |_| false);
    ensure!(path.file_name().is_some_and(|n| n == "report (2).txt"), "unique name: {}", path.display());
    browser.download_to(asked.id, Some(&path));
    download_done(runtime, browser, asked.id, "page download done")?;
    browser.save_as(&format!("{base}/file.bin"), None);
    let asked = next(runtime, browser, "cancelled Save As asked", |e| if let Event::DownloadAsked(r) = e { Some(r) } else { None })?;
    browser.download_to(asked.id, None);
    next(runtime, browser, "Save As cancelled", |e| match e {
        Event::Download(p) if p.id == asked.id && p.state == DownloadState::Cancelled => Some(()),
        _ => None,
    })?;
    std::fs::remove_dir_all(&dir)?;
    Ok(())
}

/// The print panel runs modally: a `stopModal` queued for modal-panel mode
/// only ends it (as a cancel), so the pump that ran it takes about that long.
fn print(runtime: &Runtime, browser: &Browser) -> Result<()> {
    let mtm = objc2::MainThreadMarker::new().context("main thread")?;
    let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
    let mode = NSString::from_str("NSModalPanelRunLoopMode");
    let modes: *mut AnyObject = unsafe { msg_send![objc2::class!(NSArray), arrayWithObject: &*mode] };
    let nil: *mut AnyObject = std::ptr::null_mut();
    unsafe {
        let _: () = msg_send![&*app, performSelector: sel!(stopModal), withObject: nil, afterDelay: 2.0f64, inModes: modes];
    }
    browser.print();
    let started = Instant::now();
    let mut longest = Duration::ZERO;
    while started.elapsed() < Duration::from_secs(6) {
        let at = Instant::now();
        pump_once(runtime);
        longest = longest.max(at.elapsed());
    }
    unsafe {
        let _: () = msg_send![objc2::class!(NSObject), cancelPreviousPerformRequestsWithTarget: &*app];
    }
    println!("offscreen: print: longest pump {longest:?}");
    if longest < Duration::from_millis(1500) {
        bail!("print: no modal print panel ran");
    }
    Ok(())
}

fn inspect(runtime: &Runtime, browser: &Browser) -> Result<()> {
    let windows = || -> usize {
        let mtm = objc2::MainThreadMarker::new().expect("main thread");
        let app = objc2_app_kit::NSApplication::sharedApplication(mtm);
        unsafe {
            let list: *mut AnyObject = msg_send![&*app, windows];
            msg_send![list, count]
        }
    };
    let before = windows();
    browser.inspect(Some((VIDEO.0 as i32, VIDEO.1 as i32)));
    let started = Instant::now();
    while !browser.inspecting() {
        pump_once(runtime);
        if started.elapsed() > Duration::from_secs(10) {
            bail!("DevTools did not open");
        }
    }
    crate::pump_for(runtime, Duration::from_secs(2));
    println!("offscreen: DevTools open after {:?}; app windows {before} → {}", started.elapsed(), windows());
    browser.close_inspector();
    let started = Instant::now();
    while browser.inspecting() {
        pump_once(runtime);
        if started.elapsed() > Duration::from_secs(10) {
            bail!("DevTools did not close");
        }
    }
    crate::pump_for(runtime, Duration::from_millis(500));
    Ok(())
}
