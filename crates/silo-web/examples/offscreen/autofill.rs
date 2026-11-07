//! Submitting a card form doesn't take the engine down: Chrome's autofill offers to save the
//! card in a bubble anchored to a Chrome browser window, which a windowless page doesn't have
//! (`SaveCardBubbleControllerImpl::DoShowBubble` crashed the app on a checkout page). The card is
//! typed as a user would, submitted, and the engine pumped a while after the next page loads.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use silo_web::{BrowserOptions, Modifiers, MouseButton, Runtime, ViewSize};

use crate::{pump_for, pump_once, server};

/// Fields 40 px tall from the top, then the submit button.
const PAGE: &str = "<html><head><style>body{margin:0}input,button{display:block;box-sizing:border-box;\
    width:100%;height:40px;margin:0;font-size:16px}</style></head><body>\
    <form action='/done' method='post'>\
    <input name='name' autocomplete='cc-name'>\
    <input name='number' autocomplete='cc-number'>\
    <input name='exp' autocomplete='cc-exp'>\
    <input name='cvc' autocomplete='cc-csc'>\
    <input name='address' autocomplete='street-address'>\
    <input name='zip' autocomplete='postal-code'>\
    <button type='submit'>Pay</button></form></body></html>";

const FIELDS: [&str; 6] = ["Ada Lovelace", "4111111111111111", "12/39", "123", "1 Main Street", "94110"];

pub fn run(runtime: &Runtime) -> Result<()> {
    let origin = server::spawn(PAGE.to_owned(), None)?;
    let profile = runtime.profile("default", None)?;
    let size = ViewSize { width: 400, height: 300, scale: 1.0 };
    let url = format!("http://127.0.0.1:{}/", origin.port);
    let browser = runtime.create_browser(BrowserOptions { url: &url, profile: &profile, size, offscreen: false, notify: Arc::new(|| {}) })?;
    let result = (|| {
        let started = Instant::now();
        while browser.frame().is_none() {
            pump_once(runtime);
            if started.elapsed() > Duration::from_secs(20) {
                bail!("the card form never painted");
            }
        }
        pump_for(runtime, Duration::from_millis(500));
        browser.set_focus(true);
        let none = Modifiers::default();
        let click = |y: f32| {
            browser.mouse_move(200.0, y, none, &[]);
            browser.mouse_button(200.0, y, MouseButton::Left, true, 1, none);
            browser.mouse_button(200.0, y, MouseButton::Left, false, 1, none);
        };
        for (i, text) in FIELDS.iter().enumerate() {
            click(20.0 + 40.0 * i as f32);
            pump_for(runtime, Duration::from_millis(150));
            browser.insert_text(text);
            pump_for(runtime, Duration::from_millis(150));
        }
        click(20.0 + 40.0 * FIELDS.len() as f32);
        let submitted = Instant::now();
        while !origin.requests.lock().unwrap().iter().any(|r| r.starts_with("POST /done")) {
            pump_once(runtime);
            if submitted.elapsed() > Duration::from_secs(20) {
                bail!("the card form was never submitted: {:?}", origin.requests.lock().unwrap());
            }
        }
        // The save offer comes once the next page commits.
        pump_for(runtime, Duration::from_secs(4));
        println!("offscreen: autofill: a card form submitted, no save bubble, engine still up");
        Ok(())
    })();
    browser.close();
    pump_for(runtime, Duration::from_millis(500));
    result
}
