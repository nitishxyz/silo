//! Syntax highlighting with tree-sitter (docs/FILES_BLOCK.md §3), shared by
//! the Files block viewer and the agent pane's code blocks.
//!
//! Small next to Zed's (no buffer, syntax map or registry): detect a
//! [`Language`], [`highlight`] the whole text into capture spans, then
//! [`styles`] turns them into gpui highlights from the theme's syntax table.
//! Grammars are the crates Zed uses; the queries are each grammar's own
//! (see `queries/NOTICE`; Zed's are GPL and not used). The agent pane's
//! code blocks go through [`cached_lines`]; the Files block highlights off
//! the UI thread.

mod cache;
mod capture;
mod config;
mod highlight;
mod language;
mod style;
#[cfg(test)]
mod tests;

pub use cache::{Lines, cached_lines};
#[allow(unused_imports, reason = "the tests use them through `super::*`")]
pub use capture::{Capture, NONE, key_for};
#[allow(unused_imports, reason = "the tests use them through `super::*`")]
pub use highlight::{MAX_BYTES, MAX_LINE_CHARS};
pub use highlight::{by_line, highlight, highlightable};
pub use language::Language;
pub use style::styles;
#[allow(unused_imports, reason = "the tests use them through `super::*`")]
pub use style::{capture_style, highlight_styles};
