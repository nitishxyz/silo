//! `type --app [--keys 200] [--hz 10] [--delay 3]`: posts keystrokes to the
//! frontmost window (focus a Silo pane during the delay) for the in-app
//! photon trace (docs/PHASE5.md §6). Run Silo with `SILO_LATENCY_TRACE=<file>`.
//! Needs Accessibility permission for the invoking terminal, granted once.
//! Keys are US-layout virtual keycodes.

use std::time::Duration;

use anyhow::{Context, bail};

const TEXT: &[u8] = b"the quick brown fox jumps over the lazy dog";
const RETURN: u16 = 36;

/// US ANSI layout, unshifted then shifted, in macOS virtual keycode order.
const LAYOUT: [(u8, u8); 48] = [
    (b'a', b'A'), (b's', b'S'), (b'd', b'D'), (b'f', b'F'), (b'h', b'H'), (b'g', b'G'), (b'z', b'Z'), (b'x', b'X'),
    (b'c', b'C'), (b'v', b'V'), (0, 0), (b'b', b'B'), (b'q', b'Q'), (b'w', b'W'), (b'e', b'E'), (b'r', b'R'),
    (b'y', b'Y'), (b't', b'T'), (b'1', b'!'), (b'2', b'@'), (b'3', b'#'), (b'4', b'$'), (b'6', b'^'), (b'5', b'%'),
    (b'=', b'+'), (b'9', b'('), (b'7', b'&'), (b'-', b'_'), (b'8', b'*'), (b'0', b')'), (b']', b'}'), (b'o', b'O'),
    (b'u', b'U'), (b'[', b'{'), (b'i', b'I'), (b'p', b'P'), (0, 0), (b'l', b'L'), (b'j', b'J'), (b'\'', b'"'),
    (b'k', b'K'), (b';', b':'), (b'\\', b'|'), (b',', b'<'), (b'/', b'?'), (b'n', b'N'), (b'm', b'M'), (b'.', b'>'),
];

/// The virtual keycode for an ASCII byte and whether it needs shift.
fn keycode(byte: u8) -> Option<(u16, bool)> {
    match byte {
        b' ' => Some((49, false)),
        b'`' => Some((50, false)),
        b'~' => Some((50, true)),
        0 => None,
        _ => (0u16..).zip(LAYOUT).find_map(|(code, (plain, shifted))| {
            (byte == plain).then_some((code, false)).or((byte == shifted).then_some((code, true)))
        }),
    }
}

fn main() -> anyhow::Result<()> {
    let (mut app, mut keys, mut hz, mut delay) = (false, 200usize, 10u32, 3u64);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let mut value = || args.next().with_context(|| format!("{arg} needs a value"));
        match arg.as_str() {
            "--app" => app = true,
            "--keys" => keys = value()?.parse().context("--keys")?,
            "--hz" => hz = value()?.parse().context("--hz")?,
            "--delay" => delay = value()?.parse().context("--delay")?,
            other => bail!("unknown argument {other}"),
        }
    }
    if !app || hz == 0 {
        bail!("usage: type --app [--keys N] [--hz N] [--delay SECONDS]");
    }
    cg::check()?;
    println!("typing {keys} keys at {hz} Hz in {delay} s: focus a Silo pane");
    std::thread::sleep(Duration::from_secs(delay));
    for (i, &byte) in TEXT.iter().cycle().take(keys).enumerate() {
        let (code, shift) = keycode(byte).with_context(|| format!("no keycode for {:?}", char::from(byte)))?;
        cg::post(code, shift, Some(byte));
        if i % TEXT.len() == TEXT.len() - 1 {
            cg::post(RETURN, false, None);
        }
        std::thread::sleep(Duration::from_secs(1) / hz);
    }
    Ok(())
}

#[cfg(target_os = "macos")]
mod cg {
    use std::ffi::c_void;

    const SHIFT: u64 = 0x0002_0000;

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> u8;
        fn CGEventCreateKeyboardEvent(source: *const c_void, keycode: u16, down: bool) -> *mut c_void;
        fn CGEventSetFlags(event: *mut c_void, flags: u64);
        fn CGEventKeyboardSetUnicodeString(event: *mut c_void, length: usize, string: *const u16);
        fn CGEventPost(tap: u32, event: *mut c_void);
        fn CFRelease(cf: *const c_void);
    }

    pub fn check() -> anyhow::Result<()> {
        // SAFETY: no arguments; reads this process's trust state.
        if unsafe { AXIsProcessTrusted() } == 0 {
            anyhow::bail!("grant Accessibility to this terminal (System Settings > Privacy & Security)");
        }
        Ok(())
    }

    /// Key down and up for `keycode` (with shift), carrying `text`.
    pub fn post(keycode: u16, shift: bool, text: Option<u8>) {
        for down in [true, false] {
            // SAFETY: a fresh event, posted to the HID tap and released once.
            unsafe {
                let event = CGEventCreateKeyboardEvent(std::ptr::null(), keycode, down);
                if event.is_null() {
                    continue;
                }
                CGEventSetFlags(event, if shift { SHIFT } else { 0 });
                if let Some(byte) = text {
                    CGEventKeyboardSetUnicodeString(event, 1, &u16::from(byte));
                }
                CGEventPost(0, event);
                CFRelease(event);
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod cg {
    pub fn check() -> anyhow::Result<()> {
        anyhow::bail!("type --app posts macOS CGEvents")
    }

    pub fn post(_: u16, _: bool, _: Option<u8>) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn us_keycodes() {
        assert_eq!(keycode(b'a'), Some((0, false)));
        assert_eq!(keycode(b'T'), Some((17, true)));
        assert_eq!(keycode(b'5'), Some((23, false)));
        assert_eq!(keycode(b'?'), Some((44, true)));
        assert_eq!(keycode(b' '), Some((49, false)));
        assert_eq!(keycode(b'~'), Some((50, true)));
        assert!(TEXT.iter().all(|&b| keycode(b).is_some()));
        assert_eq!(keycode(b'\t'), None);
    }
}
