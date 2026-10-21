//! Program-supplied text (status reports, notifications, command lines)
//! made safe to show: [`untrusted`] runs on the server before such text
//! leaves it.

/// Longest effective app name of a status report, in chars.
pub const MAX_APP: usize = 64;
/// Longest status or notification title.
pub const MAX_TITLE: usize = 128;
/// Longest status message.
pub const MAX_MESSAGE: usize = 512;
/// Longest notification body.
pub const MAX_BODY: usize = 1024;
/// Longest command line.
pub const MAX_COMMAND: usize = 4096;

/// Unicode format characters (`Cf`): bidi embeddings, overrides and
/// isolates, zero-width characters, the BOM, soft hyphens and the like.
fn format_char(c: char) -> bool {
    matches!(
        u32::from(c),
        0x00ad
            | 0x0600..=0x0605
            | 0x061c
            | 0x06dd
            | 0x070f
            | 0x0890..=0x0891
            | 0x08e2
            | 0x180e
            | 0x200b..=0x200f
            | 0x202a..=0x202e
            | 0x2060..=0x2064
            | 0x2066..=0x206f
            | 0xfeff
            | 0xfff9..=0xfffb
            | 0x110bd
            | 0x110cd
            | 0x13430..=0x1343f
            | 0x1bca0..=0x1bca3
            | 0x1d173..=0x1d17a
            | 0xe0001
            | 0xe0020..=0xe007f
    )
}

/// `s` without control characters (C0, DEL, C1) and Unicode format
/// characters, whitespace runs collapsed to one space, trimmed, and cut to
/// `max_chars` characters.
#[must_use]
pub fn untrusted(s: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max_chars * 4));
    let mut count = 0;
    let mut space = false;
    for c in s.chars() {
        if c.is_whitespace() {
            space = true;
            continue;
        }
        if c.is_control() || format_char(c) {
            continue;
        }
        let needed = usize::from(space && count > 0) + 1;
        if count + needed > max_chars {
            break;
        }
        if space && count > 0 {
            out.push(' ');
            count += 1;
        }
        space = false;
        out.push(c);
        count += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_controls_and_format_chars_collapses_and_truncates() {
        assert_eq!(untrusted("  vim\t\n main.rs  ", 64), "vim main.rs");
        assert_eq!(untrusted("a\u{1b}[31mb\u{7}\u{9b}c", 64), "a[31mbc");
        assert_eq!(untrusted("safe\u{202e}txt.exe\u{2066}\u{200b}\u{feff}\u{ad}\u{61c}", 64), "safetxt.exe");
        assert_eq!(untrusted("abcdef", 3), "abc");
        assert_eq!(untrusted("ab cd", 3), "ab");
        assert_eq!(untrusted("日本語テキスト", 3), "日本語");
        assert_eq!(untrusted("", 3), "");
    }
}
