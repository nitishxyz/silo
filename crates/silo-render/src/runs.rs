//! Groups a row of cells into text runs so the renderer shapes and paints
//! one line per run instead of one line per cell.
//!
//! A run is a stretch of narrow font cells with one font face (bold/italic);
//! color and faintness change inside a run through per-segment text runs.
//! Blank cells inside a run become spaces so a row of prose is one run.
//! Wide cells, sprite cells and wide-cell spacers end a run: their placement
//! is not one advance per cell. The renderer shapes a run once and then
//! places each glyph cluster at its own column origin (`starts` maps text
//! bytes back to cells), so batching never changes where a character lands.
//!
//! Dependency-free so it can be tested and timed without GPUI.

/// The renderer's view of one cell.
#[derive(Clone, Copy, Debug)]
#[allow(clippy::struct_excessive_bools, reason = "Independent VT style attributes mirrored from the cell")]
pub(crate) struct CellView<'a> {
    pub text: &'a str,
    /// 0 for a wide-cell spacer, 1 narrow, 2 wide.
    pub width: u8,
    /// Drawn procedurally, never shaped.
    pub sprite: bool,
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub foreground: u32,
}

/// One text segment of a run: `bytes` of the run text in one color.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Segment {
    pub bytes: usize,
    pub foreground: u32,
    pub faint: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Run {
    /// First column of the run.
    pub column: usize,
    /// Cells covered, including interior blanks.
    pub cells: usize,
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub segments: Vec<Segment>,
    /// Byte offset in `text` where each covered cell's grapheme starts;
    /// `starts.len() == cells`.
    pub starts: Vec<usize>,
}

/// What each cell of a row needs from the renderer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Single {
    /// Shape this cell alone (wide cells).
    Glyph(usize),
    /// Draw this cell procedurally.
    Sprite(usize),
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct RowPlan {
    pub runs: Vec<Run>,
    pub singles: Vec<Single>,
}

struct Open {
    column: usize,
    cells: usize,
    text: String,
    bold: bool,
    italic: bool,
    segments: Vec<Segment>,
    starts: Vec<usize>,
    /// Blank cells seen since the last text cell; only added if more text
    /// follows in the same run, so runs never end in padding.
    gap: usize,
}

impl Open {
    fn push(&mut self, cell: &CellView<'_>) {
        if self.gap > 0 {
            let last = self.segments.last().copied().unwrap_or(Segment { bytes: 0, foreground: cell.foreground, faint: cell.faint });
            for _ in 0..self.gap {
                self.starts.push(self.text.len());
                self.text.push(' ');
            }
            self.add_segment(self.gap, last.foreground, last.faint);
            self.cells += self.gap;
            self.gap = 0;
        }
        self.starts.push(self.text.len());
        self.text.push_str(cell.text);
        self.add_segment(cell.text.len(), cell.foreground, cell.faint);
        self.cells += 1;
    }

    fn add_segment(&mut self, bytes: usize, foreground: u32, faint: bool) {
        if let Some(last) = self.segments.last_mut()
            && last.foreground == foreground
            && last.faint == faint
        {
            last.bytes += bytes;
        } else {
            self.segments.push(Segment { bytes, foreground, faint });
        }
    }

    fn close(self) -> Run {
        Run {
            column: self.column,
            cells: self.cells,
            text: self.text,
            bold: self.bold,
            italic: self.italic,
            segments: self.segments,
            starts: self.starts,
        }
    }
}

/// Plan one row. Cells are in column order; each blank cell is `text == ""`.
pub(crate) fn plan_row(cells: &[CellView<'_>]) -> RowPlan {
    let mut plan = RowPlan::default();
    let mut open: Option<Open> = None;
    for (column, cell) in cells.iter().enumerate() {
        if cell.width == 0 {
            if let Some(run) = open.take() {
                plan.runs.push(run.close());
            }
            continue;
        }
        if cell.text.is_empty() {
            if let Some(run) = &mut open {
                run.gap += 1;
            }
            continue;
        }
        if cell.sprite || cell.width != 1 {
            if let Some(run) = open.take() {
                plan.runs.push(run.close());
            }
            plan.singles.push(if cell.sprite { Single::Sprite(column) } else { Single::Glyph(column) });
            continue;
        }
        match &mut open {
            Some(run) if run.bold == cell.bold && run.italic == cell.italic => run.push(cell),
            _ => {
                if let Some(run) = open.take() {
                    plan.runs.push(run.close());
                }
                let mut run = Open {
                    column,
                    cells: 0,
                    text: String::new(),
                    bold: cell.bold,
                    italic: cell.italic,
                    segments: Vec::new(),
                    starts: Vec::new(),
                    gap: 0,
                };
                run.push(cell);
                open = Some(run);
            }
        }
    }
    if let Some(run) = open.take() {
        plan.runs.push(run.close());
    }
    plan
}

/// The cell (offset from the run's first column) whose grapheme contains
/// text byte `byte`, given the run's `starts`.
pub(crate) fn cell_of(starts: &[usize], byte: usize) -> usize {
    starts.partition_point(|&start| start <= byte).saturating_sub(1)
}

#[cfg(test)]
mod tests {
    use super::{CellView, Run, Segment, Single, cell_of, plan_row};

    fn cell(text: &str) -> CellView<'_> {
        CellView { text, width: 1, sprite: false, bold: false, italic: false, faint: false, foreground: 0x00ff_ffff }
    }

    #[test]
    fn prose_with_blanks_is_one_run_without_trailing_padding() {
        let cells = [cell("l"), cell("s"), cell(""), cell("-"), cell("a"), cell(""), cell("")];
        let plan = plan_row(&cells);
        assert!(plan.singles.is_empty());
        assert_eq!(
            plan.runs,
            vec![Run {
                column: 0,
                cells: 5,
                text: "ls -a".into(),
                bold: false,
                italic: false,
                segments: vec![Segment { bytes: 5, foreground: 0x00ff_ffff, faint: false }],
                starts: vec![0, 1, 2, 3, 4],
            }]
        );
    }

    #[test]
    fn color_changes_stay_in_one_run_as_segments_but_face_changes_split() {
        let mut red = cell("r");
        red.foreground = 0x00ff_0000;
        let mut bold = cell("b");
        bold.bold = true;
        let mut faint = cell("f");
        faint.faint = true;
        let cells = [cell("a"), red, faint, bold, cell("z")];
        let plan = plan_row(&cells);
        assert_eq!(plan.runs.len(), 3);
        assert_eq!(plan.runs[0].text, "arf");
        assert_eq!(
            plan.runs[0].segments,
            vec![
                Segment { bytes: 1, foreground: 0x00ff_ffff, faint: false },
                Segment { bytes: 1, foreground: 0x00ff_0000, faint: false },
                Segment { bytes: 1, foreground: 0x00ff_ffff, faint: true },
            ]
        );
        assert!(plan.runs[1].bold);
        assert_eq!(plan.runs[1].column, 3);
        assert_eq!(plan.runs[2].column, 4);
    }

    #[test]
    fn wide_sprite_and_spacer_cells_break_runs_and_become_singles() {
        let mut wide = cell("\u{754c}");
        wide.width = 2;
        let mut spacer = cell("");
        spacer.width = 0;
        let mut sprite = cell("\u{2500}");
        sprite.sprite = true;
        let cells = [cell("a"), wide, spacer, cell("b"), sprite, cell(""), cell("c")];
        let plan = plan_row(&cells);
        assert_eq!(plan.singles, vec![Single::Glyph(1), Single::Sprite(4)]);
        let columns: Vec<_> = plan.runs.iter().map(|run| (run.column, run.text.as_str())).collect();
        assert_eq!(columns, vec![(0, "a"), (3, "b"), (6, "c")]);
    }

    #[test]
    fn multibyte_graphemes_keep_byte_accurate_segments_and_one_cell_each() {
        let mut blue = cell("e\u{301}");
        blue.foreground = 0x0000_00ff;
        let cells = [cell("\u{e9}"), blue];
        let plan = plan_row(&cells);
        assert_eq!(plan.runs.len(), 1);
        assert_eq!(plan.runs[0].cells, 2);
        assert_eq!(plan.runs[0].text, "\u{e9}e\u{301}");
        assert_eq!(plan.runs[0].segments[0].bytes, 2);
        assert_eq!(plan.runs[0].segments[1].bytes, 3);
        let total: usize = plan.runs[0].segments.iter().map(|s| s.bytes).sum();
        assert_eq!(total, plan.runs[0].text.len());
        assert_eq!(plan.runs[0].starts, vec![0, 2]);
        let starts = &plan.runs[0].starts;
        assert_eq!(cell_of(starts, 0), 0);
        assert_eq!(cell_of(starts, 1), 0);
        assert_eq!(cell_of(starts, 2), 1);
        assert_eq!(cell_of(starts, 4), 1);
    }

    #[test]
    fn interior_gap_inherits_previous_color_so_segments_cover_the_text() {
        let mut red = cell("r");
        red.foreground = 0x00ff_0000;
        let cells = [red, cell(""), cell("x")];
        let plan = plan_row(&cells);
        assert_eq!(plan.runs[0].text, "r x");
        assert_eq!(plan.runs[0].segments[0].bytes, 2);
        assert_eq!(plan.runs[0].segments[0].foreground, 0x00ff_0000);
        assert_eq!(plan.runs[0].segments[1].bytes, 1);
    }
}
