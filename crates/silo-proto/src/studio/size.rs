//! Node sizes and spacing on a canvas, in canvas units (docs/STUDIO.md §5): one source for the server's
//! placement (nodes added without coordinates, `layout`) and the app's board (what it draws, what it makes),
//! so a card the agent places is the card the board draws, its ports and output clear of its neighbours.

use super::{GenKind, SourceMedia};

/// Between placed things (vertically, and beside nodes that are not cards).
pub const GAP: f64 = 40.0;
/// A landscape, square or audio card's width.
pub const CARD_W: f64 = 340.0;
/// A tall card is never narrower (9:16: 264).
pub const TALL_MIN_W: f64 = 264.0;
pub const CARD_HEADER: f64 = 32.0;
pub const CARD_FOOTER: f64 = 36.0;
/// The face of an audio card: the compact player.
pub const AUDIO_FACE: f64 = 64.0;

/// A document card (a text asset: the script): its file name over a preview of the text.
pub const DOC_W: f64 = 320.0;
pub const DOC_H: f64 = 400.0;

/// The prompt: padding around its text, one line's height, an average glyph's advance (for estimating).
pub const PROMPT_PAD: f64 = 10.0;
pub const PROMPT_LINE: f64 = 18.0;
pub const PROMPT_GLYPH: f64 = 7.2;
pub const PROMPT_MIN: f64 = 44.0;
/// Lines a card's prompt shows collapsed (then an ellipsis), and open (edited, or the single selected card;
/// it scrolls beyond).
pub const PROMPT_LINES: usize = 4;
pub const PROMPT_LINES_OPEN: usize = 12;
/// The prompt area's tallest, collapsed and open.
#[allow(clippy::cast_precision_loss)]
pub const PROMPT_MAX: f64 = PROMPT_LINES as f64 * PROMPT_LINE + 2.0 * PROMPT_PAD;
#[allow(clippy::cast_precision_loss)]
pub const PROMPT_OPEN_MAX: f64 = PROMPT_LINES_OPEN as f64 * PROMPT_LINE + 2.0 * PROMPT_PAD;

/// Port modules float this far left of a card, this big; the output dot sits as far right of a node.
pub const PORT_GAP: f64 = 10.0;
pub const PORT_SIZE: f64 = 28.0;
/// Ports and output dots are hit this far around them.
pub const PORT_HIT: f64 = 6.0;
/// The output dot's diameter, hovered.
pub const OUTPUT_DOT: f64 = 14.0;
/// The room a card's ports take on its left.
pub const PORT_GUTTER: f64 = PORT_GAP + PORT_SIZE;
/// The room a node's output dot takes on its right, with its hit slop.
pub const OUTPUT_GUTTER: f64 = PORT_GAP + PORT_SIZE / 2.0 + OUTPUT_DOT / 2.0 + PORT_HIT;
/// Between two nodes when the right one is a card (its ports) or the left one's output faces a card.
pub const CARD_GAP: f64 = GAP + PORT_GUTTER + OUTPUT_GUTTER;

/// `"W:H"` as `w / h`.
#[must_use]
pub fn parse_aspect(s: &str) -> Option<f64> {
    let (w, h) = s.trim().split_once(':')?;
    let (w, h) = (w.trim().parse::<f64>().ok()?, h.trim().parse::<f64>().ok()?);
    (w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0).then(|| w / h)
}

/// `aspect` (`w / h`) kept sane: 0.2 to 5, 16:9 when it is none.
#[must_use]
pub fn sane(aspect: f64) -> f64 {
    if aspect.is_finite() && aspect > 0.0 { aspect.clamp(0.2, 5.0) } else { 16.0 / 9.0 }
}

/// Its face is the compact player.
#[must_use]
pub fn is_audio(makes: GenKind) -> bool {
    makes.media() == SourceMedia::Audio
}

/// A new card's width: [`CARD_W`], for a tall face `CARD_W * sqrt(aspect)` (about the same area) but at
/// least [`TALL_MIN_W`].
#[must_use]
pub fn card_width(makes: GenKind, aspect: f64) -> f64 {
    let a = sane(aspect);
    if is_audio(makes) || a >= 1.0 { CARD_W } else { (CARD_W * a.sqrt()).round().max(TALL_MIN_W) }
}

/// A card's face height at width `w`.
#[must_use]
pub fn face_height(makes: GenKind, w: f64, aspect: f64) -> f64 {
    if is_audio(makes) { AUDIO_FACE } else { w / sane(aspect) }
}

/// The prompt area for `lines` of text: collapsed (at most [`PROMPT_LINES`]) or open.
#[must_use]
pub fn prompt_height(lines: usize, open: bool) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let h = lines as f64 * PROMPT_LINE + 2.0 * PROMPT_PAD;
    h.clamp(PROMPT_MIN, if open { PROMPT_OPEN_MAX } else { PROMPT_MAX })
}

/// The lines `text` takes in a card `w` wide, estimated (the app measures them).
#[must_use]
pub fn estimate_lines(text: &str, w: f64) -> usize {
    let per_line = ((w - 2.0 * PROMPT_PAD) / PROMPT_GLYPH).floor().max(8.0);
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    text.lines().map(|l| (l.chars().count() as f64 / per_line).ceil().max(1.0) as usize).sum::<usize>().max(1)
}

/// A card's height, collapsed: header, face, a prompt of `prompt_lines`, footer.
#[must_use]
pub fn card_height(makes: GenKind, w: f64, aspect: f64, prompt_lines: usize) -> f64 {
    CARD_HEADER + face_height(makes, w, aspect) + prompt_height(prompt_lines, false) + CARD_FOOTER
}
