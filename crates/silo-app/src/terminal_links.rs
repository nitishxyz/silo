//! Links in a terminal: an OSC 8 hyperlink under the pointer, else an
//! address written out in the row's text (`https://…`, `file://…`,
//! `localhost:5173`, …), found by column so wide characters count as the
//! two cells they take.

use silo_vt::{Cell, Frame, Span};

/// The link at `column` of viewport `row` of `frame`: its span and URL.
#[must_use]
pub fn link_at(frame: &Frame, row: u16, column: u16) -> Option<(Span, String)> {
    if row >= frame.size.rows || column >= frame.size.columns {
        return None;
    }
    if let Some(link) = frame.links.iter().find(|l| l.span.row == row && l.span.start <= column && column < l.span.end) {
        return Some((link.span, link.uri.clone()));
    }
    let columns = usize::from(frame.size.columns);
    let start = usize::from(row) * columns;
    let cells = frame.cells.get(start..start + columns)?;
    let (start, end, url) = url_in_row(cells, column)?;
    Some((Span { row, start, end }, url))
}

/// The address written in `cells` (one row) over `column`: its columns
/// (`end` exclusive) and URL.
fn url_in_row(cells: &[Cell], column: u16) -> Option<(u16, u16, String)> {
    // The row's text, and the column each byte starts in.
    let mut text = String::new();
    let mut at = Vec::new();
    for (i, cell) in cells.iter().enumerate() {
        let shown = if cell.text.is_empty() && cell.width != 0 { " " } else { cell.text.as_str() };
        for _ in 0..shown.len() {
            at.push(u16::try_from(i).unwrap_or(u16::MAX));
        }
        text.push_str(shown);
    }
    let (from, to) = url_spans(&text).into_iter().find(|&(a, b)| at[a] <= column && column <= at[b - 1])?;
    let mut url = text[from..to].to_owned();
    if !url.contains("://") && !url.starts_with("mailto:") {
        url = format!("http://{url}");
    }
    let end = cells.iter().enumerate().skip(usize::from(at[to - 1]) + 1).find(|(_, c)| c.width != 0).map_or(cells.len(), |(i, _)| i);
    Some((at[from], u16::try_from(end).unwrap_or(u16::MAX), url))
}

/// Byte ranges of the addresses in `text`.
fn url_spans(text: &str) -> Vec<(usize, usize)> {
    const SCHEMES: [&str; 6] = ["https://", "http://", "file://", "ftp://", "ssh://", "mailto:"];
    const HOSTS: [&str; 3] = ["localhost:", "127.0.0.1:", "0.0.0.0:"];
    let bytes = text.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // Starts at a word boundary: not in the middle of another token.
        let boundary = i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'/' || bytes[i - 1] == b'.');
        let rest = &text[i..];
        let prefix = boundary
            .then(|| {
                SCHEMES
                    .iter()
                    .find(|s| rest.len() > s.len() && rest.as_bytes()[..s.len()].eq_ignore_ascii_case(s.as_bytes()))
                    .or_else(|| HOSTS.iter().find(|h| rest.starts_with(**h) && rest.as_bytes().get(h.len()).is_some_and(u8::is_ascii_digit)))
            })
            .flatten();
        let Some(prefix) = prefix else {
            i += text[i..].chars().next().map_or(1, char::len_utf8);
            continue;
        };
        let mut end = i + prefix.len();
        while end < bytes.len() && url_byte(bytes[end]) {
            end += 1;
        }
        let end = trim_end(&text[i..end]) + i;
        if end > i + prefix.len() {
            spans.push((i, end));
        }
        i = end.max(i + 1);
    }
    spans
}

/// Bytes an address goes on through (not whitespace, quotes or angle
/// brackets, which end one in prose and shell output).
fn url_byte(b: u8) -> bool {
    b.is_ascii_graphic() && !matches!(b, b'"' | b'\'' | b'`' | b'<' | b'>' | b'{' | b'}' | b'|' | b'\\' | b'^')
}

/// `url` less the punctuation that ends a sentence or closes a bracket it
/// didn't open: the length kept.
fn trim_end(url: &str) -> usize {
    let mut end = url.len();
    loop {
        let Some(last) = url[..end].bytes().last() else { return end };
        let unbalanced = |open: u8, close: u8| {
            last == close && url[..end].bytes().filter(|&b| b == close).count() > url[..end].bytes().filter(|&b| b == open).count()
        };
        if matches!(last, b'.' | b',' | b';' | b':' | b'!' | b'?' | b'*') || unbalanced(b'(', b')') || unbalanced(b'[', b']') {
            end -= 1;
        } else {
            return end;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(text: &str) -> Vec<Cell> {
        text.chars()
            .flat_map(|c| {
                let wide = c == '界';
                let cell = Cell { text: c.to_string(), width: if wide { 2 } else { 1 }, ..Cell::default() };
                if wide { vec![cell, Cell { width: 0, ..Cell::default() }] } else { vec![cell] }
            })
            .collect()
    }

    fn at(text: &str, column: u16) -> Option<(u16, u16, String)> {
        url_in_row(&row(text), column)
    }

    #[test]
    fn addresses_in_text() {
        let line = "  ➜  Local:   http://localhost:5173/ and more";
        let start = u16::try_from(line.chars().position(|c| c == 'h').unwrap()).unwrap();
        assert_eq!(at(line, start + 3), Some((start, start + 22, "http://localhost:5173/".into())));
        assert_eq!(at(line, start - 2), None, "before it");
        assert_eq!(at("see https://example.com/a.", 8).unwrap().2, "https://example.com/a", "a sentence's full stop");
        assert_eq!(at("(https://example.com/x_(y))", 3).unwrap().2, "https://example.com/x_(y)", "its own brackets stay");
        assert_eq!(at("[https://example.com]", 3).unwrap().2, "https://example.com");
        assert_eq!(at("open localhost:3000/api now", 8).unwrap().2, "http://localhost:3000/api", "a bare local address");
        assert_eq!(at("\"https://a.dev/q?x=1&y=2\"", 4).unwrap().2, "https://a.dev/q?x=1&y=2");
        assert_eq!(at("xhttps://a.dev", 4), None, "inside another word");
        assert_eq!(at("https://", 2), None, "a scheme alone");
        assert_eq!(at("no links here", 3), None);
    }

    #[test]
    fn wide_characters_take_two_columns() {
        // 界 takes columns 0-1; the address starts at column 3.
        let (start, end, url) = at("界 https://a.dev x", 5).unwrap();
        assert_eq!((start, end, url.as_str()), (3, 16, "https://a.dev"));
    }

    #[test]
    fn multibyte_text_near_a_scheme_length_is_safe() {
        // A powerline prompt: the 8th byte falls inside \u{e0b4}.
        assert_eq!(at("~ \u{e0b4}\u{e0b4} main \u{e0b4} ok", 4), None);
        assert_eq!(at("\u{e0b4} https://a.dev", 4).unwrap().2, "https://a.dev");
        for s in ["日本語テキスト", "ñañañañañ", "\u{e0b4}\u{e0b4}\u{e0b4}x"] {
            for c in 0..8 {
                let _ = at(s, c);
            }
        }
    }
}
