//! Capture names and the theme keys they map to.

use crate::theme;

/// A highlights-query capture name, such as `function.method`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Capture(pub(super) &'static str);

/// The key that clears highlighting under it (markdown marks a fence's
/// code `@none` so the fence's own colour does not tint it).
pub const NONE: &str = "none";

impl Capture {
    #[must_use]
    #[allow(dead_code, reason = "the tests read captures by name")]
    pub const fn name(self) -> &'static str {
        self.0
    }

    /// The theme key this capture takes; see [`key_for`].
    #[must_use]
    pub fn key(self) -> Option<&'static str> {
        key_for(self.0)
    }
}

/// The theme key for capture `name`: the longest dotted prefix of it that
/// is a key of [`theme::SYNTAX`] (or [`NONE`]), as Zed's `HighlightMap`
/// does: `function.method.call` → `function.method` if the theme has it,
/// else `function`. Unknown names have no key.
#[must_use]
pub fn key_for(name: &str) -> Option<&'static str> {
    let mut prefix = name;
    loop {
        if prefix == NONE {
            return Some(NONE);
        }
        if let Some((key, _)) = theme::SYNTAX.iter().find(|(key, _)| *key == prefix) {
            return Some(key);
        }
        prefix = prefix.rsplit_once('.')?.0;
    }
}

/// Whether a capture paints: it has a theme key, and is not a predicate
/// helper (`@_name`) or an injection/locals capture. Captures that do not
/// paint leave the colour of what encloses them, as tree-sitter-highlight
/// does with names a theme does not know.
pub(super) fn paints(name: &str) -> bool {
    !name.starts_with('_') && !name.starts_with("injection.") && !name.starts_with("local.") && key_for(name).is_some()
}
