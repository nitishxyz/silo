//! One arrow press moves a text field's caret one place (it moved two).

use anyhow::{Result, bail};
use silo_web::{Browser, Event, KeyInput, Modifiers, Runtime};

use crate::wait_event;

/// The title reports `keydowns:caret` after each arrow's release.
const PAGE: &str = "data:text/html,<html><head><title>ready</title></head><body><input autofocus value='abcdef'><script>\
    var i=document.querySelector('input'),n=0;i.focus();i.setSelectionRange(0,0);\
    document.onkeydown=function(){n++;document.title='d'+n+':'+i.selectionStart};document.onkeyup=function(){document.title='k'+n+':'+i.selectionStart}</script></body></html>";

pub fn run(runtime: &Runtime, browser: &Browser) -> Result<()> {
    browser.navigate(PAGE);
    wait_event(runtime, browser, "arrows page", |e| matches!(e, Event::Loading(false)))?;
    crate::pump_for(runtime, std::time::Duration::from_millis(500));
    browser.set_focus(true);
    let none = Modifiers::default();
    browser.key(&KeyInput { key: "right", key_char: None, modifiers: none, down: true, repeat: false });
    browser.key(&KeyInput { key: "right", key_char: None, modifiers: none, down: false, repeat: false });
    let started = std::time::Instant::now();
    loop {
        crate::pump_for(runtime, std::time::Duration::from_millis(20));
        for e in browser.take_events() {
            if let Event::TitleChanged(t) = &e {
                println!("offscreen: arrows title {t}");
            }
            if let Event::TitleChanged(t) = e
                && t.starts_with('k')
            {
                println!("offscreen: arrows: {t}");
                return if t == "k1:1" { Ok(()) } else { bail!("one right arrow gave {t}, not k1:1") };
            }
        }
        if started.elapsed().as_secs() > 10 {
            bail!("arrows: no title");
        }
    }
}
