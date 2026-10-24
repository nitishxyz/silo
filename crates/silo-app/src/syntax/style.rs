//! The gpui side: captures to `HighlightStyle`s from the theme's syntax
//! table, for `StyledText::with_highlights`.

use std::ops::Range;

use gpui::{FontStyle, FontWeight, HighlightStyle, UnderlineStyle, px, rgb};

use super::{
    capture::{Capture, NONE},
    highlight::highlight,
    language::Language,
};
use crate::theme;

/// The style `capture` takes in the current theme on background `bg`, or
/// `None` when the theme has no key for it.
#[must_use]
pub fn capture_style(capture: Capture, bg: u32) -> Option<HighlightStyle> {
    let key = capture.key().filter(|&key| key != NONE)?;
    let style = theme::syntax_style(key)?;
    Some(HighlightStyle {
        color: Some(rgb(theme::syntax_color(style.ink, bg)).into()),
        font_weight: style.bold.then_some(FontWeight::BOLD),
        font_style: style.italic.then_some(FontStyle::Italic),
        underline: style.underline.then_some(UnderlineStyle { thickness: px(1.0), color: None, wavy: false }),
        ..HighlightStyle::default()
    })
}

/// `spans` (from [`highlight`]) as highlights for `StyledText`, drawn on
/// background `bg`. Spans whose capture has no style are left out.
#[must_use]
pub fn styles(spans: &[(Range<usize>, Capture)], bg: u32) -> Vec<(Range<usize>, HighlightStyle)> {
    let mut resolved: Vec<(Capture, Option<HighlightStyle>)> = Vec::new();
    spans
        .iter()
        .filter_map(|(range, capture)| {
            let style = match resolved.iter().find(|(c, _)| c == capture) {
                Some((_, style)) => *style,
                None => {
                    let style = capture_style(*capture, bg);
                    resolved.push((*capture, style));
                    style
                }
            };
            Some((range.clone(), style?))
        })
        .collect()
}

/// [`highlight`] then [`styles`]: `source` as `lang`, ready for
/// `StyledText::with_highlights`.
#[must_use]
#[allow(dead_code, reason = "the tests' shorthand")]
pub fn highlight_styles(lang: Language, source: &str, bg: u32) -> Vec<(Range<usize>, HighlightStyle)> {
    styles(&highlight(lang, source), bg)
}
