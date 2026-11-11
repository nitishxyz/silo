//! Undo and redo for the small text inputs (the composer's [`crate::agent::editor::Editor`]
//! and [`crate::text_field::TextField`]): whole-state snapshots, with a run of typing or of
//! deleting kept as one step, as macOS text fields do.

use std::time::{Duration, Instant};

/// Steps kept.
const LIMIT: usize = 200;
/// A pause longer than this starts a new step.
const PAUSE: Duration = Duration::from_secs(1);

/// What an edit was, for grouping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Typed characters (a word boundary ends the run).
    Type,
    /// Backspace or delete of a character.
    Delete,
    /// Anything else (paste, cut, a word or line deleted): a step of its own.
    Other,
}

pub struct History<S> {
    undo: Vec<S>,
    redo: Vec<S>,
    /// The open run the next edit of the same kind joins.
    run: Option<(Kind, Instant)>,
}

impl<S> Default for History<S> {
    fn default() -> Self {
        Self { undo: Vec::new(), redo: Vec::new(), run: None }
    }
}

impl<S> History<S> {
    /// Record `before`, the state an edit of `kind` is about to change, unless it joins the open run.
    pub fn record(&mut self, before: S, kind: Kind, now: Instant) {
        let joins = kind != Kind::Other && self.run.is_some_and(|(k, at)| k == kind && now.duration_since(at) <= PAUSE);
        if !joins {
            self.undo.push(before);
            if self.undo.len() > LIMIT {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
        self.run = (kind != Kind::Other).then_some((kind, now));
    }

    /// The next edit starts a new step (the caret moved, a word ended).
    pub fn seal(&mut self) {
        self.run = None;
    }

    /// The state to go back to, with `current` kept for redo.
    pub fn undo(&mut self, current: S) -> Option<S> {
        let prev = self.undo.pop()?;
        self.redo.push(current);
        self.run = None;
        Some(prev)
    }

    /// The state to go forward to, with `current` kept for undo.
    pub fn redo(&mut self, current: S) -> Option<S> {
        let next = self.redo.pop()?;
        self.undo.push(current);
        self.run = None;
        Some(next)
    }

    pub fn clear(&mut self) {
        *self = Self { undo: Vec::new(), redo: Vec::new(), run: None };
    }
}

/// The kind of inserting `text` by typing: a run of word characters joins, whitespace or punctuation
/// ends the run after it.
pub fn typed(text: &str) -> Kind {
    if text.chars().count() == 1 { Kind::Type } else { Kind::Other }
}

/// Typing `text` ends a word: the next character starts a new step.
pub fn ends_word(text: &str) -> bool {
    text.chars().any(|c| c.is_whitespace())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_group_and_redo_clears() {
        let t = Instant::now();
        let mut h = History::default();
        // Typing "ab" is one step; a paste is its own.
        h.record("", Kind::Type, t);
        h.record("a", Kind::Type, t);
        h.record("ab", Kind::Other, t);
        assert_eq!(h.undo("ab!!"), Some("ab"));
        assert_eq!(h.undo("ab"), Some(""));
        assert_eq!(h.undo(""), None);
        assert_eq!(h.redo(""), Some("ab"));
        assert_eq!(h.redo("ab"), Some("ab!!"));
        assert_eq!(h.redo("ab!!"), None);
        // A new edit drops what could be redone.
        h.undo("ab!!");
        h.record("ab", Kind::Type, t);
        assert_eq!(h.redo("abc"), None);
        // A pause, a seal or a different kind starts a new step.
        let mut h = History::default();
        h.record("", Kind::Type, t);
        h.record("a", Kind::Type, t + Duration::from_secs(2));
        h.record("ab", Kind::Delete, t + Duration::from_secs(2));
        h.seal();
        h.record("a", Kind::Delete, t + Duration::from_secs(2));
        assert_eq!(h.undo(""), Some("a"));
        assert_eq!(h.undo("a"), Some("ab"));
        assert_eq!(h.undo("ab"), Some("a"));
        assert_eq!(h.undo("a"), Some(""));
    }
}
