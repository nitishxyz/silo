//! Keys in a text field: one arrow moves the caret one place (it moved two),
//! ⌘→ goes to the line's end and ⌘⌫ deletes to its start.

use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use silo_web::{Browser, Event, KeyInput, Modifiers, Runtime};

use crate::wait_event;

/// After each key's release the title is `k<keydowns>:<caret>:<value>`.
const PAGE: &str = "data:text/html,<html><head><title>ready</title></head><body><input autofocus value='abcdef'><script>\
    var i=document.querySelector('input'),n=0;i.focus();i.setSelectionRange(0,0);\
    document.onkeydown=function(){n++};document.onkeyup=function(){document.title='k'+n+':'+i.selectionStart+':'+i.value}</script></body></html>";

fn press(browser: &Browser, key: &str, modifiers: Modifiers) {
    for down in [true, false] {
        browser.key(&KeyInput { key, key_char: None, modifiers, down, repeat: false });
    }
}

/// The first `k…` title `ok` accepts, within 10 s.
fn title(runtime: &Runtime, browser: &Browser, what: &str, ok: impl Fn(&str) -> bool) -> Result<String> {
    let started = Instant::now();
    loop {
        crate::pump_for(runtime, Duration::from_millis(20));
        for e in browser.take_events() {
            if let Event::TitleChanged(t) = e
                && t.starts_with('k')
            {
                println!("offscreen: keys: {what}: {t}");
                if ok(&t) {
                    return Ok(t);
                }
            }
        }
        if started.elapsed() > Duration::from_secs(10) {
            bail!("keys: {what}: no matching title");
        }
    }
}

pub fn run(runtime: &Runtime, browser: &Browser) -> Result<()> {
    browser.navigate(PAGE);
    wait_event(runtime, browser, "keys page", |e| matches!(e, Event::Loading(false)))?;
    crate::pump_for(runtime, Duration::from_millis(500));
    browser.set_focus(true);
    let command = Modifiers { command: true, ..Modifiers::default() };
    press(browser, "right", Modifiers::default());
    let t = title(runtime, browser, "right arrow", |_| true)?;
    if t != "k1:1:abcdef" {
        bail!("one right arrow gave {t}, not k1:1:abcdef");
    }
    press(browser, "right", command);
    title(runtime, browser, "⌘→", |t| t.ends_with(":6:abcdef"))?;
    press(browser, "backspace", command);
    title(runtime, browser, "⌘⌫", |t| t.ends_with(":0:"))?;
    unhandled_keys_skip_the_menu(runtime, browser)
}

/// A key the page leaves unhandled never reaches the app's menu: CEF offered
/// it to `performKeyEquivalent:`, where a bare `f` matched AppKit's fn-F
/// Enter Full Screen. Here a menu item on a bare `f` opens the About panel (a window).
fn unhandled_keys_skip_the_menu(runtime: &Runtime, browser: &Browser) -> Result<()> {
    use objc2::runtime::{AnyClass, AnyObject, Sel};
    use objc2::{msg_send, sel};
    use objc2_foundation::NSString;
    let app: *mut AnyObject = unsafe { msg_send![AnyClass::get(c"NSApplication").unwrap(), sharedApplication] };
    let menu_class = AnyClass::get(c"NSMenu").unwrap();
    let item_class = AnyClass::get(c"NSMenuItem").unwrap();
    let windows = || -> usize {
        let list: *mut AnyObject = unsafe { msg_send![app, windows] };
        unsafe { msg_send![list, count] }
    };
    let before = windows();
    unsafe {
        let bar: *mut AnyObject = msg_send![menu_class, new];
        let top: *mut AnyObject = msg_send![item_class, new];
        let sub: *mut AnyObject = msg_send![menu_class, new];
        let title = NSString::from_str("Hide on f");
        let key = NSString::from_str("f");
        let action: Sel = sel!(orderFrontStandardAboutPanel:);
        let item: *mut AnyObject = msg_send![item_class, alloc];
        let item: *mut AnyObject = msg_send![item, initWithTitle: &*title, action: action, keyEquivalent: &*key];
        let _: () = msg_send![item, setKeyEquivalentModifierMask: 0usize];
        let _: () = msg_send![sub, addItem: item];
        let _: () = msg_send![top, setSubmenu: sub];
        let _: () = msg_send![bar, addItem: top];
        let _: () = msg_send![app, setMainMenu: bar];
    }
    browser.navigate("data:text/html,<html><head><title>plain</title></head><body>no field</body></html>");
    wait_event(runtime, browser, "plain page", |e| matches!(e, Event::Loading(false)))?;
    crate::pump_for(runtime, Duration::from_millis(300));
    for down in [true, false] {
        browser.key(&KeyInput { key: "f", key_char: Some("f"), modifiers: Modifiers::default(), down, repeat: false });
    }
    crate::pump_for(runtime, Duration::from_millis(500));
    let ran_menu = windows() > before;
    unsafe {
        let _: () = msg_send![app, setMainMenu: std::ptr::null::<AnyObject>()];
    }
    println!("offscreen: keys: f on a page reached the menu: {ran_menu}");
    if ran_menu {
        bail!("an unhandled f ran the menu's f item");
    }
    Ok(())
}
