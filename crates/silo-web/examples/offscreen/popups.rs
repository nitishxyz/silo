//! New tabs and context menus on a 400x300@1x page served over http: the
//! menu's facts for a link, an image, a text field and a selection, an engine
//! command answered from the menu, ⌘-/middle-/⇧⌘-clicked links, and a
//! `window.open` popup that keeps its opener (`postMessage`, `window.close()`
//! seen as `closed`) once adopted.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail, ensure};
use silo_web::{Browser, BrowserOptions, ContextMenu, Event, MenuCommand, Modifiers, MouseButton, NewTab, Runtime, ViewSize};

use crate::{pump_once, server, wait_for};

/// `/`: a link, an image, a misspelled field, selected text and an opener
/// button on grey; purple once the field's text is all selected; green once
/// the popup said hello and then closed itself. `/popup`: says hello, closes.
const PAGE: &str = "<html><body style='margin:0;height:100vh;background:rgb(64,64,64)'>\
    <a href='/next' style='position:absolute;left:0;top:0;width:100px;height:100px;background:rgb(0,0,255)'>link</a>\
    <a href='/blank' target=_blank style='position:absolute;left:0;top:100px;width:100px;height:100px;background:rgb(0,0,128)'>blank</a>\
    <img src='/favicon.png' style='position:absolute;left:100px;top:0;width:100px;height:100px'>\
    <input id=f value='helo wrold' style='position:absolute;left:200px;top:0;width:100px;height:100px'>\
    <div id=open style='position:absolute;left:300px;top:0;width:100px;height:100px;background:rgb(255,0,0)'></div>\
    <p id=t style='position:absolute;left:0;top:220px;margin:0'>selected words</p><script>\
    if (location.pathname == '/popup') { opener.postMessage('hi', '*'); setTimeout(() => window.close(), 300); }\
    else { let w, got = false; const s = document.body.style, f = document.getElementById('f');\
    getSelection().selectAllChildren(document.getElementById('t'));\
    f.addEventListener('select', () => { if (f.selectionEnd - f.selectionStart == f.value.length) s.background = 'rgb(128,0,255)' });\
    addEventListener('message', e => { if (e.data == 'hi' && e.source === w) got = true });\
    document.getElementById('open').onclick = () => { w = window.open('/popup', 'signin', 'width=420,height=320') };\
    setInterval(() => { if (got && w && w.closed) s.background = 'rgb(0,255,0)' }, 50) }\
    </script></body></html>";

const GREY: [u8; 4] = [64, 64, 64, 255];
const PURPLE: [u8; 4] = [255, 0, 128, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const SIZE: (u32, u32) = (400, 300);

/// Pumps until an event of `browser` matches `pick`.
pub fn next<T>(runtime: &Runtime, browser: &Browser, what: &str, pick: impl Fn(Event) -> Option<T>) -> Result<T> {
    let started = Instant::now();
    loop {
        pump_once(runtime);
        if let Some(found) = browser.take_events().into_iter().find_map(&pick) {
            println!("offscreen: {what} after {:?}", started.elapsed());
            return Ok(found);
        }
        if started.elapsed() > Duration::from_secs(20) {
            bail!("{what}: no matching event in 20s");
        }
    }
}

pub fn click(browser: &Browser, (x, y): (f32, f32), button: MouseButton, mods: Modifiers) {
    browser.mouse_move(x, y, mods, &[]);
    browser.mouse_button(x, y, button, true, 1, mods);
    browser.mouse_button(x, y, button, false, 1, mods);
}

pub fn menu_at(runtime: &Runtime, browser: &Browser, at: (f32, f32), what: &str) -> Result<ContextMenu> {
    click(browser, at, MouseButton::Right, Modifiers::default());
    let menu = next(runtime, browser, what, |e| if let Event::ContextMenu(m) = e { Some(m) } else { None })?;
    println!("offscreen: {what}: {menu:?}");
    Ok(menu)
}

fn new_tab(runtime: &Runtime, browser: &Browser, what: &str) -> Result<NewTab> {
    let tab = next(runtime, browser, what, |e| if let Event::NewTab(t) = e { Some(t) } else { None })?;
    println!("offscreen: {what}: url {} foreground {} popup {} size {:?}", tab.url, tab.foreground, tab.popup.is_some(), tab.size);
    Ok(tab)
}

/// Adopts `tab`'s popup (if any) and closes it, as a closed tab would.
fn discard(runtime: &Runtime, tab: NewTab) -> Result<()> {
    let Some(popup) = tab.popup else { return Ok(()) };
    let page = popup.open(Arc::new(|| {}));
    page.close();
    next(runtime, &page, "discarded popup closed", |e| matches!(e, Event::Closed { by_page: false }).then_some(()))
}

pub fn run(runtime: &Runtime) -> Result<()> {
    let origin = server::spawn(PAGE.to_owned(), None)?;
    let base = format!("http://127.0.0.1:{}", origin.port);
    let profile = runtime.profile("default", None)?;
    let size = ViewSize { width: SIZE.0, height: SIZE.1, scale: 1.0 };
    let options = BrowserOptions { url: &format!("{base}/"), profile: &profile, size, offscreen: false, notify: Arc::new(|| {}) };
    let browser = runtime.create_browser(options)?;
    let result = (|| {
        wait_for(runtime, &browser, GREY, SIZE, "popups page")?;
        browser.set_focus(true);

        let link = menu_at(runtime, &browser, (50.0, 50.0), "menu on a link")?;
        ensure!(link.link.as_deref() == Some(&*format!("{base}/next")) && !link.editable, "link menu");
        browser.menu_command(None);
        let image = menu_at(runtime, &browser, (150.0, 50.0), "menu on an image")?;
        ensure!(image.media.as_ref().map(|m| m.src.as_str()) == Some(&*format!("{base}/favicon.png")), "image menu");
        let png = image_png(runtime, &browser, &format!("{base}/favicon.png"))?;
        ensure!(png.starts_with(b"\x89PNG"), "copy image: PNG bytes");
        browser.menu_command(None);
        let text = menu_at(runtime, &browser, (40.0, 228.0), "menu on selected text")?;
        ensure!(text.selection == "selected words" && !text.editable, "selection menu");
        browser.menu_command(None);
        click(&browser, (250.0, 50.0), MouseButton::Left, Modifiers::default());
        crate::pump_for(runtime, Duration::from_millis(500));
        let field = menu_at(runtime, &browser, (250.0, 50.0), "menu in a text field")?;
        ensure!(field.editable && field.edit.select_all && field.edit.paste, "field menu");
        browser.menu_command(Some(MenuCommand::SelectAll));
        wait_for(runtime, &browser, PURPLE, SIZE, "Select All from the menu")?;

        let command = Modifiers { command: true, ..Modifiers::default() };
        click(&browser, (50.0, 50.0), MouseButton::Left, command);
        let tab = new_tab(runtime, &browser, "⌘-click")?;
        ensure!(tab.url == format!("{base}/next") && !tab.foreground, "⌘-click opens behind");
        discard(runtime, tab)?;
        click(&browser, (50.0, 50.0), MouseButton::Middle, Modifiers::default());
        let tab = new_tab(runtime, &browser, "middle click")?;
        ensure!(!tab.foreground, "middle click opens behind");
        discard(runtime, tab)?;
        click(&browser, (50.0, 50.0), MouseButton::Left, Modifiers { shift: true, ..command });
        let tab = new_tab(runtime, &browser, "⇧⌘-click")?;
        ensure!(tab.foreground, "⇧⌘-click opens in front");
        discard(runtime, tab)?;
        click(&browser, (50.0, 150.0), MouseButton::Left, Modifiers::default());
        let tab = new_tab(runtime, &browser, "target=_blank")?;
        ensure!(tab.url == format!("{base}/blank") && tab.foreground, "target=_blank opens in front");
        discard(runtime, tab)?;

        click(&browser, (350.0, 50.0), MouseButton::Left, Modifiers::default());
        let tab = new_tab(runtime, &browser, "window.open")?;
        ensure!(tab.foreground && tab.size == Some((420, 320)), "window.open is a front tab with its size");
        let Some(popup) = tab.popup else { bail!("window.open: no popup to adopt") };
        let page = popup.open(Arc::new(|| {}));
        next(runtime, &page, "popup closed itself", |e| matches!(e, Event::Closed { by_page: true }).then_some(()))?;
        wait_for(runtime, &browser, GREEN, SIZE, "opener got postMessage and saw closed")
    })();
    browser.close();
    crate::pump_for(runtime, Duration::from_millis(500));
    result
}

fn image_png(runtime: &Runtime, browser: &Browser, url: &str) -> Result<Vec<u8>> {
    let mut rx = browser.image_png(url);
    let started = Instant::now();
    loop {
        pump_once(runtime);
        match rx.try_recv() {
            Ok(Some(png)) => return Ok(png),
            Ok(None) => bail!("image download failed"),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => bail!("image download dropped"),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) if started.elapsed() > Duration::from_secs(10) => bail!("image download timed out"),
            Err(_) => {}
        }
    }
}
