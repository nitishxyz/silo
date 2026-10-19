//! Navigation checks on an existing 200x100@1x browser: navigate, favicon + popup events,
//! back/forward, reload, zoom, wheel scrolling, text insertion and IME composition — each proven by
//! the centre pixel or an event.

use anyhow::Result;
use silo_web::{Browser, Event, Modifiers, MouseButton, Runtime};

use crate::{server, wait_event, wait_for};

/// Served over http (Chromium reports no favicons for `data:` documents); a click opens a popup.
const CYAN_PAGE: &str = "<html><head><link rel=icon href='/favicon.png'></head>\
    <body style='margin:0;height:100vh;background:rgb(0,255,255)' \
    onmousedown=\"window.open('https://popup.silo-test/')\"></body></html>";
const YELLOW_PAGE: &str = "data:text/html,<html><body style='margin:0;height:100vh;background:rgb(255,255,0)'></body></html>";
/// Grey; orange once reloaded; purple whenever zoomed in (devicePixelRatio > 1.1).
const STATE_PAGE: &str = "data:text/html,<html><body style='margin:0;height:100vh'><script>\
    var t=performance.getEntriesByType('navigation')[0].type;function paint(){document.body.style.background=\
    devicePixelRatio>1.1?'rgb(128,0,255)':t=='reload'?'rgb(255,128,0)':'rgb(128,128,128)'}paint();onresize=paint;\
    </script></body></html>";
/// Dark grey for the first viewport, green below.
const TALL_PAGE: &str = "data:text/html,<html><body style='margin:0'><div style='height:100vh;background:rgb(64,64,64)'>\
    </div><div style='height:200vh;background:rgb(0,128,0)'></div></body></html>";
/// Dark grey; blue once the input holds "hi" (then cleared); purple once an IME commits "にほん".
/// (Composition events have no `on*` attributes, hence `addEventListener`.)
const INPUT_PAGE: &str = "data:text/html;charset=utf-8,<html><body style='margin:0;height:100vh;background:rgb(64,64,64)'>\
    <input autofocus><script>var i=document.querySelector('input'),s=document.body.style;\
    function f(e){if(e.isComposing)return;if(i.value=='hi'){s.background='rgb(0,0,255)';i.value=''}\
    else if(i.value=='\u{306b}\u{307b}\u{3093}')s.background='rgb(128,0,255)'}\
    i.addEventListener('input',f);i.addEventListener('compositionend',f)</script></body></html>";

const CYAN: [u8; 4] = [255, 255, 0, 255];
const YELLOW: [u8; 4] = [0, 255, 255, 255];
const GREY: [u8; 4] = [128, 128, 128, 255];
const ORANGE: [u8; 4] = [0, 128, 255, 255];
const PURPLE: [u8; 4] = [255, 0, 128, 255];
const DARK: [u8; 4] = [64, 64, 64, 255];
const GREEN: [u8; 4] = [0, 128, 0, 255];
const BLUE: [u8; 4] = [255, 0, 0, 255];
const SIZE: (u32, u32) = (200, 100);

pub fn run(runtime: &Runtime, browser: &Browser) -> Result<()> {
    let origin = server::spawn(CYAN_PAGE.to_owned(), None)?;
    browser.navigate(&format!("http://127.0.0.1:{}/", origin.port));
    wait_for(runtime, browser, CYAN, SIZE, "navigate")?;
    wait_event(runtime, browser, "favicon", |e| {
        matches!(e, Event::FaviconChanged(_))
    })?;
    let none = Modifiers::default();
    browser.mouse_button(100.0, 50.0, MouseButton::Left, true, 1, none);
    browser.mouse_button(100.0, 50.0, MouseButton::Left, false, 1, none);
    wait_event(
        runtime,
        browser,
        "popup",
        |e| matches!(e, Event::Popup(url) if url.contains("popup.silo-test")),
    )?;
    browser.navigate(YELLOW_PAGE);
    wait_for(runtime, browser, YELLOW, SIZE, "navigate again")?;
    browser.back();
    wait_for(runtime, browser, CYAN, SIZE, "back")?;
    browser.forward();
    wait_for(runtime, browser, YELLOW, SIZE, "forward")?;
    browser.navigate(STATE_PAGE);
    wait_for(runtime, browser, GREY, SIZE, "state page")?;
    browser.reload(false);
    wait_for(runtime, browser, ORANGE, SIZE, "reload")?;
    browser.set_zoom(1.0);
    wait_for(runtime, browser, PURPLE, SIZE, "zoom in")?;
    browser.set_zoom(0.0);
    wait_for(runtime, browser, ORANGE, SIZE, "zoom reset")?;
    browser.navigate(TALL_PAGE);
    wait_for(runtime, browser, DARK, SIZE, "tall page")?;
    browser.mouse_move(100.0, 50.0, none, &[]);
    browser.wheel(100.0, 50.0, 0.0, -150.0, none);
    wait_for(runtime, browser, GREEN, SIZE, "wheel scroll")?;
    browser.navigate(INPUT_PAGE);
    wait_for(runtime, browser, DARK, SIZE, "input page")?;
    browser.insert_text("hi");
    wait_for(runtime, browser, BLUE, SIZE, "insert_text")?;
    browser.ime_set_composition("\u{306b}\u{307b}\u{3093}", 3..3);
    wait_event(runtime, browser, "IME composing", |e| {
        matches!(e, Event::ImeBounds { .. })
    })?;
    browser.ime_finish();
    wait_for(runtime, browser, PURPLE, SIZE, "IME composition")?;
    Ok(())
}
