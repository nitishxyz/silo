//! Syntax highlighting with tree-sitter (docs/FILES_BLOCK.md §3), shared by
//! the Files block viewer and the agent pane's code blocks.
//!
//! Small next to Zed's (no buffer, syntax map or registry): detect a
//! [`Language`], [`highlight`] the whole text into capture spans, then
//! [`styles`] turns them into gpui highlights from the theme's syntax table.
//! Grammars are the crates Zed uses; the queries are each grammar's own
//! (see `queries/NOTICE`; Zed's are GPL and not used).
// Used by the Files block and agent pane next; nothing calls it yet.
#![allow(dead_code)]

mod capture;
mod config;
mod highlight;
mod language;
mod style;
#[cfg(test)]
mod tests;

// Used by the Files block and agent pane next; nothing calls it yet.
#[allow(unused_imports)]
pub use capture::{Capture, NONE, key_for};
// Used by the Files block and agent pane next; nothing calls it yet.
#[allow(unused_imports)]
pub use highlight::{MAX_BYTES, MAX_LINE_CHARS, by_line, highlight, highlightable};
// Used by the Files block and agent pane next; nothing calls it yet.
#[allow(unused_imports)]
pub use language::Language;
// Used by the Files block and agent pane next; nothing calls it yet.
#[allow(unused_imports)]
pub use style::{capture_style, highlight_styles, styles};
