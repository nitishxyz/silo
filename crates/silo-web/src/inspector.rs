//! Inspect: Chromium's DevTools for a page, in a window of its own (a
//! windowed browser CEF makes, Chrome style: Alloy isn't allowed for
//! DevTools, and a windowless page has no view to dock it in). Its Edit
//! chords are mapped here: AppKit would offer them to the app's menu, which
//! only knows Silo's own windows.

#![allow(clippy::transmute_ptr_to_ptr)]

use std::os::raw::c_int;

use cef::rc::Rc as _;
use cef::*;

/// What a ⌘ chord does in the DevTools window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Chord {
    Edit(crate::Edit),
    Close,
}

const COMMAND: u32 = 128;
const SHIFT: u32 = 2;
const CONTROL: u32 = 4;
const ALT: u32 = 8;

fn chord(key: char, modifiers: u32) -> Option<Chord> {
    if modifiers & COMMAND == 0 || modifiers & (CONTROL | ALT) != 0 {
        return None;
    }
    let shift = modifiers & SHIFT != 0;
    Some(match key.to_ascii_lowercase() {
        'z' if shift => Chord::Edit(crate::Edit::Redo),
        _ if shift => return None,
        'z' => Chord::Edit(crate::Edit::Undo),
        'x' => Chord::Edit(crate::Edit::Cut),
        'c' => Chord::Edit(crate::Edit::Copy),
        'v' => Chord::Edit(crate::Edit::Paste),
        'a' => Chord::Edit(crate::Edit::SelectAll),
        'w' => Chord::Close,
        _ => return None,
    })
}

wrap_keyboard_handler! {
    struct InspectorKeys {}

    impl KeyboardHandler {
        fn on_pre_key_event(
            &self,
            browser: Option<&mut Browser>,
            event: Option<&KeyEvent>,
            _os_event: *mut u8,
            _is_keyboard_shortcut: Option<&mut c_int>,
        ) -> c_int {
            let (Some(browser), Some(event)) = (browser, event) else { return 0 };
            if event.type_ != KeyEventType::RAWKEYDOWN {
                return 0;
            }
            let key = char::from_u32(u32::from(event.unmodified_character)).unwrap_or_default();
            let Some(chord) = chord(key, event.modifiers) else { return 0 };
            match chord {
                Chord::Close => {
                    if let Some(host) = browser.host() {
                        host.close_browser(0);
                    }
                }
                Chord::Edit(edit) => {
                    let Some(frame) = browser.focused_frame().or_else(|| browser.main_frame()) else { return 0 };
                    match edit {
                        crate::Edit::Undo => frame.undo(),
                        crate::Edit::Redo => frame.redo(),
                        crate::Edit::Cut => frame.cut(),
                        crate::Edit::Copy => frame.copy(),
                        crate::Edit::Paste => frame.paste(),
                        crate::Edit::SelectAll => frame.select_all(),
                    }
                }
            }
            1
        }
    }
}

wrap_client! {
    struct InspectorClient {
        keys: KeyboardHandler,
    }

    impl Client {
        fn keyboard_handler(&self) -> Option<KeyboardHandler> {
            Some(self.keys.clone())
        }
    }
}

/// Open (or bring forward) `host`'s DevTools, inspecting the element at
/// `at` (view points) when given.
pub(crate) fn show(host: &BrowserHost, at: Option<(i32, i32)>) {
    let info = WindowInfo { window_name: CefString::from("DevTools"), bounds: Rect { x: 0, y: 0, width: 1000, height: 720 }, ..Default::default() };
    let mut client = InspectorClient::new(InspectorKeys::new());
    let at = at.map(|(x, y)| Point { x, y });
    host.show_dev_tools(Some(&info), Some(&mut client), Some(&BrowserSettings::default()), at.as_ref());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_chords_and_close() {
        assert_eq!(chord('c', COMMAND), Some(Chord::Edit(crate::Edit::Copy)));
        assert_eq!(chord('V', COMMAND), Some(Chord::Edit(crate::Edit::Paste)));
        assert_eq!(chord('z', COMMAND | SHIFT), Some(Chord::Edit(crate::Edit::Redo)));
        assert_eq!(chord('w', COMMAND), Some(Chord::Close));
        assert_eq!(chord('c', 0), None, "typing stays DevTools'");
        assert_eq!(chord('c', COMMAND | ALT), None, "⌥⌘C is DevTools' inspect chord");
        assert_eq!(chord('f', COMMAND), None, "DevTools handles its own find");
    }
}
