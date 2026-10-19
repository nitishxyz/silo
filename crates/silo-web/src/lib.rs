//! CEF boundary for browser blocks: macOS, windowless, accelerated paint into IOSurfaces.
//!
//! Flow: `Runtime::init` (after NSApp exists) → `Runtime::profile` → `Runtime::create_browser`
//! → feed input, `pump` when `Config::schedule` fires, paint `Browser::frame().buffer` with
//! gpui's `window.paint_surface`.

#![cfg(target_os = "macos")]

mod browser;
mod cr_app;
mod frame;
mod handlers;
mod input;
mod keyboard;
pub mod layout;
mod profile;
pub mod proxy;
mod runtime;
mod shared;

pub use browser::{Browser, BrowserOptions};
pub use frame::{BGRA, Frame};
pub use input::{Modifiers, MouseButton};
pub use keyboard::KeyInput;
pub use profile::Profile;
pub use runtime::{Config, Runtime, Schedule};
pub use shared::{Cursor, Event, ViewSize};

/// Entry point of the `<App> Helper*.app` subprocesses; returns the process exit code.
pub fn run_helper() -> i32 {
    let Ok(exe) = std::env::current_exe() else {
        return 1;
    };
    let loader = cef::library_loader::LibraryLoader::new(&exe, true);
    if !loader.load() {
        eprintln!(
            "silo-web helper: cannot load CEF framework relative to {}",
            exe.display()
        );
        return 1;
    }
    let _ = cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);
    let args = cef::args::Args::new();
    cef::execute_process(Some(args.as_main_args()), None, std::ptr::null_mut())
}
