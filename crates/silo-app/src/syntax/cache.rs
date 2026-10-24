//! Highlighted lines kept for texts drawn again and again (the agent
//! pane's code blocks redraw every frame they show): parsing once per
//! distinct text, on the UI thread, for small texts only.

use std::{
    cell::RefCell,
    hash::{Hash as _, Hasher as _},
    ops::Range,
    rc::Rc,
};

use super::{
    capture::Capture,
    highlight::{by_line, highlight},
    language::Language,
};

/// Spans per line, ranges relative to each line's start.
pub type Lines = Rc<Vec<Vec<(Range<usize>, Capture)>>>;

/// Texts kept.
const KEPT: usize = 64;
/// Texts longer than this aren't highlighted here (parse them off the UI
/// thread instead, as the Files block does).
pub const CACHED_MAX_BYTES: usize = 64 * 1024;

thread_local! {
    /// Newest last.
    static CACHE: RefCell<Vec<(Language, u64, Lines)>> = const { RefCell::new(Vec::new()) };
}

fn key(text: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// `text`'s highlighted lines as `lang`, parsed on first use; `None` for
/// a text over [`CACHED_MAX_BYTES`].
#[must_use]
pub fn cached_lines(lang: Language, text: &str) -> Option<Lines> {
    if text.len() > CACHED_MAX_BYTES {
        return None;
    }
    let k = key(text);
    if let Some(found) = CACHE.with_borrow_mut(|cache| {
        let at = cache.iter().position(|(l, h, _)| *l == lang && *h == k)?;
        let entry = cache.remove(at);
        let lines = entry.2.clone();
        cache.push(entry);
        Some(lines)
    }) {
        return Some(found);
    }
    let lines: Lines = Rc::new(by_line(text, &highlight(lang, text)));
    CACHE.with_borrow_mut(|cache| {
        if cache.len() >= KEPT {
            cache.remove(0);
        }
        cache.push((lang, k, lines.clone()));
    });
    Some(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts_are_parsed_once_and_big_ones_not_here() {
        let a = cached_lines(Language::Rust, "fn a() {}\nlet x = 1;").unwrap();
        let again = cached_lines(Language::Rust, "fn a() {}\nlet x = 1;").unwrap();
        assert!(Rc::ptr_eq(&a, &again), "the second draw reuses the first parse");
        assert_eq!(a.len(), 2, "one entry per line");
        assert!(!Rc::ptr_eq(&a, &cached_lines(Language::Python, "fn a() {}\nlet x = 1;").unwrap()), "per language");
        assert!(cached_lines(Language::Rust, &"x".repeat(CACHED_MAX_BYTES + 1)).is_none());
        for i in 0..KEPT + 1 {
            let _ = cached_lines(Language::Rust, &format!("let v{i} = {i};"));
        }
        assert_eq!(CACHE.with_borrow(Vec::len), KEPT, "the oldest go");
    }
}
