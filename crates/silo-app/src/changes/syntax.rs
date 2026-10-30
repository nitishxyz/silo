//! Syntax colours for a diff: each side's lines put back together (the old
//! side's context and removed lines, the new side's context and added
//! ones), parsed as the file's language, and the spans handed back per
//! hunk and line. Hunks are parts of a file, so a construct cut by a gap
//! may colour oddly; the lines inside a hunk read as in Files.

use std::{ops::Range, rc::Rc};

use super::wire::{FileDiff, LineKind};
use crate::syntax::{Capture, Language};

/// Spans per hunk, per line (ranges in the line's text).
pub(crate) type DiffSyntax = Rc<Vec<HunkSyntax>>;
/// One hunk's spans, per line.
pub(crate) type HunkSyntax = Vec<Vec<(Range<usize>, Capture)>>;

/// The work of colouring a diff, to run off the UI thread.
pub(crate) struct Job {
    lang: Language,
    old: Side,
    new: Side,
    /// Per hunk and line: (new side?, line index in that side's text).
    at: Vec<Vec<Option<(bool, usize)>>>,
}

#[derive(Default)]
struct Side {
    text: String,
    lines: usize,
}

impl Side {
    fn push(&mut self, line: &str) -> usize {
        self.text.push_str(line);
        self.text.push('\n');
        self.lines += 1;
        self.lines - 1
    }
}

/// The job for `diff`; `None` for plain text or a binary file.
pub(crate) fn job(diff: &FileDiff) -> Option<Job> {
    if diff.binary {
        return None;
    }
    let first = diff.hunks.first().and_then(|h| h.lines.first()).map(|l| l.text.as_str());
    let lang = Language::detect(&diff.path, first)?;
    let (mut old, mut new) = (Side::default(), Side::default());
    let at = diff
        .hunks
        .iter()
        .map(|h| {
            h.lines
                .iter()
                .map(|l| match l.kind {
                    LineKind::Context => {
                        old.push(&l.text);
                        Some((true, new.push(&l.text)))
                    }
                    LineKind::Added => Some((true, new.push(&l.text))),
                    LineKind::Removed => Some((false, old.push(&l.text))),
                    LineKind::NoNewline | LineKind::Unknown => None,
                })
                .collect()
        })
        .collect();
    Some(Job { lang, old, new, at })
}

/// One diff line's text, tabs expanded: its syntax colours (`spans`,
/// readable on `bg`) with the changed words' `word` background over them.
pub(crate) fn styled(text: &str, spans: Option<&[(Range<usize>, Capture)]>, words: &[Range<usize>], word: gpui::Hsla, bg: u32) -> gpui::StyledText {
    use crate::files::model::{expand_tabs, to_shown};
    let mut shown = expand_tabs(text).into_owned();
    if shown.is_empty() {
        shown.push(' ');
    }
    let map = |r: &Range<usize>| to_shown(text, r.start.min(text.len()))..to_shown(text, r.end.min(text.len()));
    let spans: Vec<_> =
        spans.map(|s| crate::syntax::styles(s, bg)).unwrap_or_default().iter().map(|(r, s)| (map(r), *s)).filter(|(r, _)| r.start < r.end).collect();
    let backs: Vec<(Range<usize>, gpui::Hsla)> = words.iter().map(|r| (map(r), word)).collect();
    gpui::StyledText::new(shown).with_highlights(crate::files::text::combine(&spans, &backs))
}

/// What [`Job::run`] gives back, before it is shared on the UI thread.
pub(crate) struct Coloured(Vec<HunkSyntax>);

impl Coloured {
    pub fn into_shared(self) -> DiffSyntax {
        Rc::new(self.0)
    }
}

impl Job {
    pub fn run(self) -> Coloured {
        let lines = |side: &Side| crate::syntax::by_line(&side.text, &crate::syntax::highlight(self.lang, &side.text));
        let (old, new) = (lines(&self.old), lines(&self.new));
        Coloured(
            self.at
                .iter()
                .map(|h| {
                    h.iter()
                        .map(|at| match at {
                            Some((true, i)) => new.get(*i).cloned().unwrap_or_default(),
                            Some((false, i)) => old.get(*i).cloned().unwrap_or_default(),
                            None => Vec::new(),
                        })
                        .collect()
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::changes::wire::{DiffLine, FileStatus, Hunk};

    #[test]
    fn each_side_is_coloured_as_its_own_text() {
        let line = |kind, text: &str| DiffLine { kind, old: None, new: None, text: text.into() };
        let hunk = Hunk {
            id: "h".into(),
            old_start: 1,
            old_lines: 2,
            new_start: 1,
            new_lines: 2,
            header: String::new(),
            lines: vec![
                line(LineKind::Context, "fn a() {"),
                line(LineKind::Removed, "    let x = 1;"),
                line(LineKind::Added, "    let y = \"two\";"),
            ],
        };
        let diff = FileDiff { path: "a.rs".into(), old_path: None, status: FileStatus::Modified, binary: false, hunks: vec![hunk], truncated: false };
        let job = job(&diff).expect("rust");
        assert_eq!(job.old.text, "fn a() {\n    let x = 1;\n");
        assert_eq!(job.new.text, "fn a() {\n    let y = \"two\";\n");
        let lines = job.run().0;
        assert_eq!(lines[0].len(), 3);
        assert!(lines[0].iter().all(|l| !l.is_empty()), "every line has colours: {lines:?}");
        // The string on the added line is found in the added line's own text.
        assert!(lines[0][2].iter().any(|(r, _)| &"    let y = \"two\";"[r.clone()] == "\"two\""));
        let plain = FileDiff { path: "notes".into(), ..diff };
        assert!(super::job(&plain).is_none());
    }
}
