use std::{collections::HashSet, ops::Range};

const MAX_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RawDisposition {
    Encode,
    PlatformText,
    Ignore,
    Cancel,
}

#[derive(Default)]
pub(crate) struct InputGate {
    forwarded: HashSet<String>,
}

impl InputGate {
    pub fn clear(&mut self) {
        self.forwarded.clear();
    }

    pub fn route(
        &mut self,
        key: &str,
        printable: bool,
        release: bool,
        composing: bool,
    ) -> RawDisposition {
        if release {
            let forwarded = self.forwarded.remove(key);
            return if forwarded && !composing {
                RawDisposition::Encode
            } else {
                RawDisposition::Ignore
            };
        }
        if !composing {
            if self.forwarded.len() < 64 && key.len() <= 64 {
                self.forwarded.insert(key.to_owned());
            }
            return RawDisposition::Encode;
        }
        if key == "escape" {
            RawDisposition::Cancel
        } else if printable {
            RawDisposition::PlatformText
        } else {
            RawDisposition::Ignore
        }
    }
}

/// IME-local text only. Terminal output is never surrounding/editable text.
#[derive(Clone, Default)]
pub(crate) struct Composition {
    text: String,
    selected: Range<usize>,
}

impl Composition {
    pub fn text(&self) -> &str {
        &self.text
    }
    pub fn len_utf16(&self) -> usize {
        self.text.encode_utf16().count()
    }
    pub fn selected(&self) -> Range<usize> {
        self.selected.clone()
    }
    pub fn is_active(&self) -> bool {
        !self.text.is_empty()
    }
    pub fn cancel(&mut self) {
        self.text.clear();
        self.selected = 0..0;
    }

    pub fn text_range(&self, range: Range<usize>) -> Option<&str> {
        self.text.get(byte_range(&self.text, range)?)
    }

    pub fn select(&mut self, range: Range<usize>) -> Result<(), &'static str> {
        byte_range(&self.text, range.clone())
            .ok_or("IME selection splits a Unicode scalar or exceeds preedit")?;
        self.selected = range;
        Ok(())
    }

    pub fn mark(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        selected: Option<Range<usize>>,
    ) -> Result<(), &'static str> {
        let start = range.as_ref().map_or(0, |range| range.start);
        let replacement = self.replaced(range, text)?;
        let end = text.encode_utf16().count();
        let selected = selected.unwrap_or(end..end);
        byte_range(text, selected.clone()).ok_or("Invalid IME selection")?;
        self.text = replacement;
        self.selected = (start + selected.start)..(start + selected.end);
        Ok(())
    }

    pub fn delete(&mut self, range: Range<usize>) -> Result<(), &'static str> {
        self.mark(Some(range), "", None)
    }

    pub fn commit(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
    ) -> Result<String, &'static str> {
        let committed = self.replaced(range, text)?;
        self.cancel();
        Ok(committed)
    }

    pub fn batch(
        &mut self,
        delete: Option<Range<usize>>,
        commit: Option<&str>,
        preedit: Option<&str>,
    ) -> Result<Option<String>, &'static str> {
        let mut next = self.clone();
        if let Some(range) = delete {
            next.delete(range)?;
        }
        let committed = commit.map(|text| next.commit(None, text)).transpose()?;
        if let Some(text) = preedit {
            next.mark(None, text, None)?;
        }
        *self = next;
        Ok(committed)
    }

    fn replaced(&self, range: Option<Range<usize>>, text: &str) -> Result<String, &'static str> {
        // Reject command/control injection; clipboard paste keeps its separate VT policy.
        if text.len() > MAX_BYTES || text.chars().any(char::is_control) {
            return Err("IME text exceeds 4096 bytes or contains control characters");
        }
        let range = byte_range(&self.text, range.unwrap_or(0..self.len_utf16()))
            .ok_or("IME replacement is outside preedit or splits a Unicode scalar")?;
        if self.text.len() - range.len() + text.len() > MAX_BYTES {
            return Err("IME composition exceeds 4096 bytes");
        }
        let mut replacement = self.text.clone();
        replacement.replace_range(range, text);
        Ok(replacement)
    }
}

pub(crate) fn byte_range(text: &str, range: Range<usize>) -> Option<Range<usize>> {
    if range.start > range.end {
        return None;
    }
    Some(byte_offset(text, range.start)?..byte_offset(text, range.end)?)
}

fn byte_offset(text: &str, utf16: usize) -> Option<usize> {
    let mut offset = 0;
    for (byte, ch) in text.char_indices() {
        if offset == utf16 {
            return Some(byte);
        }
        offset += ch.len_utf16();
        if offset > utf16 {
            return None;
        }
    }
    (offset == utf16).then_some(text.len())
}

#[cfg(test)]
mod tests {
    use super::{Composition, byte_range};

    #[test]
    fn utf16_scalars_surrogates_and_combining_ranges() {
        let text = "a\u{1f600}e\u{301}\u{65e5}";
        assert_eq!(byte_range(text, 1..3), Some(1..5));
        assert!(byte_range(text, 2..3).is_none());
        assert!(byte_range(text, 0..99).is_none());
        assert!(byte_range(text, std::ops::Range { start: 3, end: 1 }).is_none());
        let mut state = Composition::default();
        state.mark(None, text, Some(1..3)).unwrap();
        assert_eq!(state.len_utf16(), 6);
        assert_eq!(state.text_range(3..5), Some("e\u{301}"));
        state.select(5..6).unwrap();
        assert_eq!(state.selected(), 5..6);
        assert!(state.select(2..2).is_err());
        assert_eq!(state.selected(), 5..6);
    }

    #[test]
    fn preedit_updates_commit_once_and_cancellation_never_emits_text() {
        let mut state = Composition::default();
        state.mark(None, "\u{306b}", None).unwrap();
        state.mark(None, "\u{65e5}\u{672c}", Some(1..2)).unwrap();
        assert!(state.is_active());
        assert_eq!(
            state.commit(None, "\u{65e5}\u{672c}\u{8a9e}").unwrap(),
            "\u{65e5}\u{672c}\u{8a9e}"
        );
        assert!(!state.is_active());
        assert_eq!(state.commit(None, "").unwrap(), "");
        state.mark(None, "pending", None).unwrap();
        state.cancel();
        assert_eq!(state.text(), "");
        assert_eq!(state.selected(), 0..0);
    }

    #[test]
    fn replacements_and_delete_are_local_and_transactional() {
        let mut state = Composition::default();
        state.mark(None, "a\u{1f600}b", None).unwrap();
        assert!(state.delete(2..3).is_err());
        assert_eq!(state.text(), "a\u{1f600}b");
        state.delete(1..3).unwrap();
        assert_eq!(state.text(), "ab");
        assert_eq!(state.commit(Some(1..2), "c").unwrap(), "ac");
        assert!(state.delete(0..1).is_err());
    }

    #[test]
    fn unsafe_and_oversized_text_fail_closed() {
        let mut state = Composition::default();
        state.mark(None, "safe", None).unwrap();
        for text in ["\n", "\r", "\x1b[201~", "\0", "\u{85}"] {
            assert!(state.commit(None, text).is_err());
            assert_eq!(state.text(), "safe");
        }
        assert!(state.mark(None, &"x".repeat(4097), None).is_err());
        state.mark(None, &"x".repeat(4096), None).unwrap();
        assert!(state.mark(Some(0..0), "x", None).is_err());
    }

    #[test]
    fn selected_range_is_relative_to_inserted_preedit() {
        let mut state = Composition::default();
        state.mark(None, "ab", None).unwrap();
        state.mark(Some(1..2), "\u{1f600}c", Some(0..2)).unwrap();
        assert_eq!(state.text(), "a\u{1f600}c");
        assert_eq!(state.selected(), 1..3);
        assert!(state.mark(Some(1..3), "\u{1f600}", Some(1..2)).is_err());
        assert_eq!(state.text(), "a\u{1f600}c");
    }

    #[test]
    fn raw_keys_and_composed_text_have_only_one_delivery_path() {
        use super::{InputGate, RawDisposition};
        let mut gate = InputGate::default();
        assert_eq!(gate.route("a", true, false, false), RawDisposition::Encode);
        assert_eq!(
            gate.route("dead_acute", false, false, true),
            RawDisposition::Ignore
        );
        assert_eq!(
            gate.route("eacute", true, false, true),
            RawDisposition::PlatformText
        );
        // Commit cleared preedit, but its physical key release must not reach the PTY.
        assert_eq!(
            gate.route("eacute", true, true, false),
            RawDisposition::Ignore
        );
        // XKB can name the release by physical key instead of composed keysym.
        assert_eq!(gate.route("e", true, true, false), RawDisposition::Ignore);
        assert_eq!(
            gate.route("dead_acute", false, true, false),
            RawDisposition::Ignore
        );
        assert_eq!(gate.route("a", true, true, false), RawDisposition::Encode);
        assert_eq!(
            gate.route("escape", false, false, true),
            RawDisposition::Cancel
        );
        assert_eq!(
            gate.route("escape", false, true, false),
            RawDisposition::Ignore
        );
        gate.clear();
        assert_eq!(
            gate.route("enter", false, false, false),
            RawDisposition::Encode
        );
    }

    #[test]
    fn ime_transactions_apply_delete_commit_preedit_atomically() {
        let mut state = Composition::default();
        state.mark(None, "a\u{1f600}", None).unwrap();
        assert_eq!(
            state
                .batch(Some(1..3), Some("\u{65e5}"), Some("next"))
                .unwrap(),
            Some("\u{65e5}".into())
        );
        assert_eq!(state.text(), "next");
        assert!(state.batch(None, Some("committed"), Some("\n")).is_err());
        assert_eq!(state.text(), "next");
        assert_eq!(state.batch(None, None, Some("")).unwrap(), None);
        assert!(!state.is_active());
    }
}
