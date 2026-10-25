//! gpui-shaped keystrokes → CEF key events (macOS native key codes).

use cef::{KeyEvent, KeyEventType};

use crate::input::{IS_REPEAT, Modifiers};

/// One key transition. `key` uses gpui's key names (`"a"`, `"enter"`, `"left"`, `"f5"`, …) and
/// `key_char` is the text the key produces (gpui `Keystroke::key_char`).
#[derive(Clone, Debug)]
pub struct KeyInput<'a> {
    pub key: &'a str,
    pub key_char: Option<&'a str>,
    pub modifiers: Modifiers,
    pub down: bool,
    pub repeat: bool,
}

/// Events to send for one transition: RAWKEYDOWN (+ CHAR when it types) on down, KEYUP on up.
pub(crate) fn key_events(input: &KeyInput) -> Vec<KeyEvent> {
    if let Some((edge, key)) = line_delete(input) {
        // Select to the line's edge, then delete the selection.
        let plain = Modifiers::default();
        let delete = |down| key_events(&KeyInput { key, key_char: None, modifiers: plain, down, repeat: input.repeat });
        if !input.down {
            return delete(false);
        }
        let select = |down| key_events(&KeyInput { key: edge, key_char: None, modifiers: Modifiers { shift: true, ..plain }, down, repeat: false });
        return [select(true), select(false), delete(true)].concat();
    }
    let line_edge = line_edge(input);
    let input = &KeyInput {
        key: line_edge.unwrap_or(input.key),
        modifiers: Modifiers { command: input.modifiers.command && line_edge.is_none(), ..input.modifiers },
        ..input.clone()
    };
    let typed = typed_char(input);
    let Some(code) = windows_key_code(input.key).or(typed.map(|_| 0)) else {
        return Vec::new();
    };
    let mut modifiers = input.modifiers.flags();
    if input.repeat {
        modifiers |= IS_REPEAT;
    }
    let base = KeyEvent {
        windows_key_code: code,
        native_key_code: mac_key_code(code),
        modifiers,
        character: typed.or(function_char(input.key)).unwrap_or(0),
        unmodified_character: unmodified(input.key).or(typed).or(function_char(input.key)).unwrap_or(0),
        ..Default::default()
    };
    if !input.down {
        return vec![KeyEvent { type_: KeyEventType::KEYUP, ..base }];
    }
    let mut events = vec![KeyEvent { type_: KeyEventType::RAWKEYDOWN, ..base.clone() }];
    let shortcut = input.modifiers.command || input.modifiers.control;
    if typed.is_some() && !shortcut {
        events.push(KeyEvent { type_: KeyEventType::CHAR, ..base });
    }
    events
}

/// ⌘⌫ / ⌘⌦: delete to the line's start / end (AppKit's
/// `deleteToBeginningOfLine:`, which windowless key events don't carry, as
/// [`line_edge`]): the line edge to select to and the key that deletes.
fn line_delete(input: &KeyInput) -> Option<(&'static str, &'static str)> {
    let m = input.modifiers;
    if !m.command || m.control || m.alt || m.shift {
        return None;
    }
    match input.key {
        "backspace" => Some(("home", "backspace")),
        "delete" => Some(("end", "delete")),
        _ => None,
    }
}

/// ⌘← / ⌘→ (⇧ to select) as Home / End: Chromium on macOS moves the caret to the line
/// edge from AppKit edit commands, which windowless key events don't carry.
fn line_edge(input: &KeyInput) -> Option<&'static str> {
    let m = input.modifiers;
    if !m.command || m.control || m.alt {
        return None;
    }
    match input.key {
        "left" => Some("home"),
        "right" => Some("end"),
        _ => None,
    }
}

/// The UTF-16 unit this key types, if it is a single BMP character.
fn typed_char(input: &KeyInput) -> Option<u16> {
    let text = match input.key {
        "enter" | "return" => "\r",
        "tab" => "\t",
        "space" => " ",
        "backspace" => "\u{8}",
        "escape" => "\u{1b}",
        _ => input.key_char?,
    };
    let mut units = text.encode_utf16();
    let unit = units.next()?;
    units.next().is_none().then_some(unit)
}

/// AppKit's `NS*FunctionKey` character for a key that types nothing (what
/// `-[NSEvent characters]` holds). CEF builds a native event from it on macOS:
/// with no character, an arrow's release reached the page as a second press,
/// so one arrow moved twice.
fn function_char(key: &str) -> Option<u16> {
    let c = match key {
        "up" => 0xF700,
        "down" => 0xF701,
        "left" => 0xF702,
        "right" => 0xF703,
        "insert" => 0xF727,
        "delete" => 0xF728,
        "home" => 0xF729,
        "end" => 0xF72B,
        "pageup" => 0xF72C,
        "pagedown" => 0xF72D,
        _ => {
            let n = key.strip_prefix('f')?.parse::<u16>().ok()?;
            return (1..=24).contains(&n).then_some(0xF703 + n);
        }
    };
    Some(c)
}

fn single(text: &str) -> Option<char> {
    let mut chars = text.chars();
    let first = chars.next()?;
    chars.next().is_none().then_some(first)
}

fn unmodified(key: &str) -> Option<u16> {
    let c = single(key)?;
    let base = match c {
        '!' => '1',
        '@' => '2',
        '#' => '3',
        '$' => '4',
        '%' => '5',
        '^' => '6',
        '&' => '7',
        '*' => '8',
        '(' => '9',
        ')' => '0',
        ':' => ';',
        '+' => '=',
        '<' => ',',
        '_' => '-',
        '>' => '.',
        '?' => '/',
        '~' => '`',
        '{' => '[',
        '|' => '\\',
        '}' => ']',
        '"' => '\'',
        c if c.is_ascii_alphabetic() => c.to_ascii_lowercase(),
        c => c,
    };
    u16::try_from(u32::from(base)).ok()
}

/// Chromium's portable `KeyboardCode` (Windows virtual key) for gpui key names.
fn windows_key_code(key: &str) -> Option<i32> {
    let code = match key {
        "enter" | "return" => 0x0D,
        "backspace" => 0x08,
        "tab" => 0x09,
        "shift" => 0x10,
        "control" => 0x11,
        "alt" => 0x12,
        "capslock" => 0x14,
        "escape" => 0x1B,
        "space" => 0x20,
        "pageup" => 0x21,
        "pagedown" => 0x22,
        "end" => 0x23,
        "home" => 0x24,
        "left" => 0x25,
        "up" => 0x26,
        "right" => 0x27,
        "down" => 0x28,
        "insert" => 0x2D,
        "delete" => 0x2E,
        "platform" => 0x5B,
        "menu" => 0x5D,
        ";" | ":" => 0xBA,
        "=" | "+" => 0xBB,
        "," | "<" => 0xBC,
        "-" | "_" => 0xBD,
        "." | ">" => 0xBE,
        "/" | "?" => 0xBF,
        "`" | "~" => 0xC0,
        "[" | "{" => 0xDB,
        "\\" | "|" => 0xDC,
        "]" | "}" => 0xDD,
        "'" | "\"" => 0xDE,
        "!" => 0x31,
        "@" => 0x32,
        "#" => 0x33,
        "$" => 0x34,
        "%" => 0x35,
        "^" => 0x36,
        "&" => 0x37,
        "*" => 0x38,
        "(" => 0x39,
        ")" => 0x30,
        _ => {
            if let Some(n) = key.strip_prefix('f').and_then(|n| n.parse::<i32>().ok()) {
                return (1..=24).contains(&n).then_some(0x6F + n);
            }
            let c = single(key)?;
            if c.is_ascii_alphanumeric() {
                i32::from(c.to_ascii_uppercase() as u8)
            } else {
                return None;
            }
        }
    };
    Some(code)
}

/// macOS `kVK_*` hardware code (ANSI layout) for a Windows virtual key; CEF derives DOM `code`
/// from it.
fn mac_key_code(windows: i32) -> i32 {
    const LETTERS: [i32; 26] = [
        0x00, 0x0B, 0x08, 0x02, 0x0E, 0x03, 0x05, 0x04, 0x22, 0x26, 0x28, 0x25, 0x2E, 0x2D, 0x1F, 0x23, 0x0C, 0x0F, 0x01, 0x11, 0x20, 0x09, 0x0D,
        0x07, 0x10, 0x06,
    ];
    const DIGITS: [i32; 10] = [0x1D, 0x12, 0x13, 0x14, 0x15, 0x17, 0x16, 0x1A, 0x1C, 0x19];
    const FKEYS: [i32; 20] = [0x7A, 0x78, 0x63, 0x76, 0x60, 0x61, 0x62, 0x64, 0x65, 0x6D, 0x67, 0x6F, 0x69, 0x6B, 0x71, 0x6A, 0x40, 0x4F, 0x50, 0x5A];
    match windows {
        0x41..=0x5A => LETTERS[(windows - 0x41) as usize],
        0x30..=0x39 => DIGITS[(windows - 0x30) as usize],
        0x70..=0x83 => FKEYS[(windows - 0x70) as usize],
        0x0D => 0x24,
        0x09 => 0x30,
        0x20 => 0x31,
        0x08 => 0x33,
        0x1B => 0x35,
        0x5B => 0x37,
        0x10 => 0x38,
        0x14 => 0x39,
        0x12 => 0x3A,
        0x11 => 0x3B,
        0x2D => 0x72,
        0x24 => 0x73,
        0x21 => 0x74,
        0x2E => 0x75,
        0x23 => 0x77,
        0x22 => 0x79,
        0x25 => 0x7B,
        0x27 => 0x7C,
        0x28 => 0x7D,
        0x26 => 0x7E,
        0xBA => 0x29,
        0xBB => 0x18,
        0xBC => 0x2B,
        0xBD => 0x1B,
        0xBE => 0x2F,
        0xBF => 0x2C,
        0xC0 => 0x32,
        0xDB => 0x21,
        0xDC => 0x2A,
        0xDD => 0x1E,
        0xDE => 0x27,
        _ => 0xFF,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(key: &'a str, key_char: Option<&'a str>, modifiers: Modifiers, down: bool) -> KeyInput<'a> {
        KeyInput { key, key_char, modifiers, down, repeat: false }
    }

    #[test]
    fn letter_press_is_rawkeydown_then_char() {
        let events = key_events(&input("a", Some("a"), Modifiers::default(), true));
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].type_, KeyEventType::RAWKEYDOWN);
        assert_eq!(events[0].windows_key_code, 0x41);
        assert_eq!(events[0].native_key_code, 0x00);
        assert_eq!(events[1].type_, KeyEventType::CHAR);
        assert_eq!(events[1].character, u16::from(b'a'));
    }

    #[test]
    fn command_shortcut_has_no_char_and_carries_the_flag() {
        let command = Modifiers { command: true, ..Default::default() };
        let events = key_events(&input("c", Some("c"), command, true));
        assert_eq!(events.len(), 1);
        assert_ne!(events[0].modifiers & command.flags(), 0);
    }

    #[test]
    fn command_arrows_are_line_edges() {
        let command = Modifiers { command: true, ..Default::default() };
        let home = key_events(&input("left", None, command, true));
        assert_eq!((home[0].windows_key_code, home[0].native_key_code, home[0].modifiers & command.flags()), (0x24, 0x73, 0));
        let select = Modifiers { shift: true, ..command };
        let end = key_events(&input("right", None, select, true));
        assert_eq!(end[0].windows_key_code, 0x23);
        assert_ne!(end[0].modifiers & select.flags(), 0);
        let word = Modifiers { alt: true, ..command };
        assert_eq!(key_events(&input("left", None, word, true))[0].windows_key_code, 0x25);
    }

    #[test]
    fn command_delete_selects_to_the_line_edge_then_deletes() {
        let command = Modifiers { command: true, ..Default::default() };
        let down = key_events(&input("backspace", None, command, true));
        let kinds: Vec<_> = down.iter().map(|e| (e.type_, e.windows_key_code)).collect();
        assert_eq!(
            kinds,
            [(KeyEventType::RAWKEYDOWN, 0x24), (KeyEventType::KEYUP, 0x24), (KeyEventType::RAWKEYDOWN, 0x08), (KeyEventType::CHAR, 0x08)]
        );
        let shift = Modifiers { shift: true, ..Default::default() }.flags();
        assert_ne!(down[0].modifiers & shift, 0, "Home extends the selection");
        assert_eq!(down[2].modifiers & command.flags(), 0, "the delete is plain");
        let up = key_events(&input("backspace", None, command, false));
        assert_eq!((up.len(), up[0].type_, up[0].windows_key_code), (1, KeyEventType::KEYUP, 0x08));
        assert_eq!(key_events(&input("delete", None, command, true))[0].windows_key_code, 0x23, "⌘⌦ selects to End");
        assert_eq!(key_events(&input("backspace", None, Modifiers::default(), true))[0].windows_key_code, 0x08);
    }

    #[test]
    fn shifted_punctuation_maps_to_the_base_key() {
        let shift = Modifiers { shift: true, ..Default::default() };
        let events = key_events(&input("?", Some("?"), shift, true));
        assert_eq!(events[0].windows_key_code, 0xBF);
        assert_eq!(events[0].native_key_code, 0x2C);
        assert_eq!(events[0].unmodified_character, u16::from(b'/'));
    }

    #[test]
    fn named_keys_and_release() {
        let up = key_events(&input("enter", None, Modifiers::default(), false));
        assert_eq!((up.len(), up[0].type_, up[0].native_key_code), (1, KeyEventType::KEYUP, 0x24));
        let f5 = key_events(&input("f5", None, Modifiers::default(), true));
        assert_eq!((f5[0].windows_key_code, f5[0].native_key_code), (0x74, 0x60));
        let left = key_events(&input("left", None, Modifiers::default(), true));
        assert_eq!((left.len(), left[0].native_key_code), (1, 0x7B));
        // AppKit's function-key character, on press and release alike.
        let right_up = key_events(&input("right", None, Modifiers::default(), false));
        assert_eq!((left[0].character, right_up[0].character, right_up[0].unmodified_character), (0xF702, 0xF703, 0xF703));
        assert_eq!(key_events(&input("f5", None, Modifiers::default(), true))[0].character, 0xF708);
        let e_acute = key_events(&input("é", Some("é"), Modifiers::default(), true));
        assert_eq!((e_acute[0].windows_key_code, e_acute[1].character), (0, 0xE9));
        assert!(key_events(&input("unknownkey", None, Modifiers::default(), true)).is_empty());
    }
}
