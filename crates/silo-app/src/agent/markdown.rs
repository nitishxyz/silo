//! Markdown for assistant text: parsed into a few block kinds with styled
//! inline runs; the view draws them. Pure.

use std::ops::Range;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

/// GitHub-flavoured extras: tables, task lists, ~~strike~~. Not smart punctuation (it would rewrite
/// code-like text such as `--flag` or quotes), nor `ENABLE_GFM` (it swallows `> [!NOTE]` labels).
const OPTIONS: Options = Options::ENABLE_STRIKETHROUGH.union(Options::ENABLE_TABLES).union(Options::ENABLE_TASKLISTS);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub strike: bool,
}

/// Text with styled ranges (non-overlapping, sorted) and links.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Inline {
    pub text: String,
    pub spans: Vec<(Range<usize>, Style)>,
    pub links: Vec<(Range<usize>, String)>,
}

impl Inline {
    fn push(&mut self, text: &str, style: Style) {
        if text.is_empty() {
            return;
        }
        let start = self.text.len();
        self.text.push_str(text);
        if style == Style::default() {
            return;
        }
        match self.spans.last_mut() {
            Some((range, s)) if *s == style && range.end == start => range.end = self.text.len(),
            _ => self.spans.push((start..self.text.len(), style)),
        }
    }
}

/// A table column's alignment (from its delimiter row: `:--`, `:-:`, `--:`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Align {
    #[default]
    None,
    Left,
    Center,
    Right,
}

impl From<Alignment> for Align {
    fn from(a: Alignment) -> Self {
        match a {
            Alignment::None => Self::None,
            Alignment::Left => Self::Left,
            Alignment::Center => Self::Center,
            Alignment::Right => Self::Right,
        }
    }
}

/// A list item: its blocks, and for a task (`- [ ]` / `- [x]`) whether it's checked.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Item {
    pub task: Option<bool>,
    pub blocks: Vec<Block>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    Para(Inline),
    Heading(u8, Inline),
    Code {
        lang: Option<String>,
        text: String,
    },
    List {
        start: Option<u64>,
        items: Vec<Item>,
    },
    Quote(Vec<Block>),
    Rule,
    /// Every row (and the head) has one cell per column. While streaming the head may be empty: a table
    /// begun, its header row not complete yet (nothing to draw).
    Table {
        align: Vec<Align>,
        head: Vec<Inline>,
        rows: Vec<Vec<Inline>>,
    },
}

enum Frame {
    Blocks(Vec<Block>),
    List(Option<u64>, Vec<Item>),
    Item(Option<bool>, Vec<Block>),
    Inline {
        heading: Option<u8>,
        inline: Inline,
        implicit: bool,
    },
    Code(Option<String>, String),
    /// A table being built: its cells so far, `row` the one being read.
    Table {
        align: Vec<Align>,
        head: Vec<Inline>,
        rows: Vec<Vec<Inline>>,
        row: Vec<Inline>,
    },
}

struct Builder {
    stack: Vec<Frame>,
    style: [u32; 4],
    links: Vec<(usize, String)>,
    /// Source offset of the top-level block being built, and of each one built.
    at: usize,
    starts: Vec<usize>,
}

impl Builder {
    fn style(&self) -> Style {
        Style { bold: self.style[0] > 0, italic: self.style[1] > 0, code: false, strike: self.style[2] > 0 }
    }

    fn add(&mut self, block: Block) {
        if self.stack.len() == 1 {
            self.starts.push(self.at);
        }
        match self.stack.last_mut() {
            Some(Frame::Blocks(blocks) | Frame::Item(_, blocks)) => blocks.push(block),
            Some(Frame::List(_, items)) => {
                if items.is_empty() {
                    items.push(Item::default());
                }
                items.last_mut().expect("item").blocks.push(block);
            }
            _ => {}
        }
    }

    /// Close a paragraph that tight list items open implicitly.
    fn close_implicit(&mut self) {
        if matches!(self.stack.last(), Some(Frame::Inline { implicit: true, .. })) {
            self.finish_inline();
        }
    }

    /// The inline being built (if it's on top), its trailing whitespace trimmed.
    fn take_inline(&mut self) -> Option<(Option<u8>, Inline)> {
        if !matches!(self.stack.last(), Some(Frame::Inline { .. })) {
            return None;
        }
        let Some(Frame::Inline { heading, mut inline, .. }) = self.stack.pop() else { unreachable!("checked above") };
        let trimmed = inline.text.trim_end().len();
        inline.text.truncate(trimmed);
        for (range, _) in &mut inline.spans {
            range.end = range.end.min(trimmed);
        }
        inline.spans.retain(|(r, _)| r.start < r.end);
        for (range, _) in &mut inline.links {
            range.end = range.end.min(trimmed);
        }
        inline.links.retain(|(r, _)| r.start < r.end);
        Some((heading, inline))
    }

    fn finish_inline(&mut self) {
        if let Some((heading, inline)) = self.take_inline() {
            if inline.text.is_empty() {
                return;
            }
            self.add(match heading {
                Some(level) => Block::Heading(level, inline),
                None => Block::Para(inline),
            });
        }
    }

    /// A table cell ends: its text joins the row being read.
    fn finish_cell(&mut self) {
        let cell = self.take_inline().map(|(_, inline)| inline).unwrap_or_default();
        if let Some(Frame::Table { row, .. }) = self.stack.last_mut() {
            row.push(cell);
        }
    }

    /// The table being built is complete (or the text ended): every row gets one cell per column.
    fn finish_table(&mut self) {
        if let Some(Frame::Table { align, mut head, mut rows, row }) = self.stack.pop() {
            if !row.is_empty() {
                rows.push(row);
            }
            let cols = align.len();
            head.resize(cols, Inline::default());
            for row in &mut rows {
                row.resize(cols, Inline::default());
            }
            self.add(Block::Table { align, head, rows });
        }
    }

    fn inline(&mut self) -> &mut Inline {
        if !matches!(self.stack.last(), Some(Frame::Inline { .. })) {
            self.stack.push(Frame::Inline { heading: None, inline: Inline::default(), implicit: true });
        }
        match self.stack.last_mut() {
            Some(Frame::Inline { inline, .. }) => inline,
            _ => unreachable!("pushed above"),
        }
    }

    fn text(&mut self, text: &str, code: bool) {
        if let Some(Frame::Code(_, body)) = self.stack.last_mut() {
            body.push_str(text);
            return;
        }
        let style = Style { code, ..self.style() };
        self.inline().push(text, style);
    }
}

/// All of `source` at once (the view parses through [`Doc`]).
#[cfg(test)]
#[must_use]
pub fn parse(source: &str) -> Vec<Block> {
    parse_top(source).0
}

/// The blocks of `source` and where each top-level one starts.
fn parse_top(source: &str) -> (Vec<Block>, Vec<usize>) {
    let mut b = Builder { stack: vec![Frame::Blocks(Vec::new())], style: [0; 4], links: Vec::new(), at: 0, starts: Vec::new() };
    for (event, range) in Parser::new_ext(source, OPTIONS).into_offset_iter() {
        if b.stack.len() == 1 && matches!(event, Event::Start(_) | Event::Rule) {
            b.at = range.start;
        }
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    b.close_implicit();
                    b.stack.push(Frame::Inline { heading: None, inline: Inline::default(), implicit: false });
                }
                Tag::Heading { level, .. } => {
                    b.close_implicit();
                    let level = match level {
                        HeadingLevel::H1 => 1,
                        HeadingLevel::H2 => 2,
                        HeadingLevel::H3 => 3,
                        _ => 4,
                    };
                    b.stack.push(Frame::Inline { heading: Some(level), inline: Inline::default(), implicit: false });
                }
                Tag::CodeBlock(kind) => {
                    b.close_implicit();
                    let lang = match kind {
                        CodeBlockKind::Fenced(lang) => Some(lang.to_string()).filter(|l| !l.is_empty()),
                        CodeBlockKind::Indented => None,
                    };
                    b.stack.push(Frame::Code(lang, String::new()));
                }
                Tag::List(start) => {
                    b.close_implicit();
                    b.stack.push(Frame::List(start, Vec::new()));
                }
                Tag::Item => {
                    b.close_implicit();
                    b.stack.push(Frame::Item(None, Vec::new()));
                }
                Tag::Table(align) => {
                    b.close_implicit();
                    let align = align.into_iter().map(Align::from).collect();
                    b.stack.push(Frame::Table { align, head: Vec::new(), rows: Vec::new(), row: Vec::new() });
                }
                Tag::TableCell => b.stack.push(Frame::Inline { heading: None, inline: Inline::default(), implicit: false }),
                Tag::BlockQuote(_) => {
                    b.close_implicit();
                    b.stack.push(Frame::Blocks(Vec::new()));
                }
                Tag::Emphasis => b.style[1] += 1,
                Tag::Strong => b.style[0] += 1,
                Tag::Strikethrough => b.style[2] += 1,
                Tag::Link { dest_url, .. } => {
                    let at = b.inline().text.len();
                    b.links.push((at, dest_url.to_string()));
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph | TagEnd::Heading(_) => b.finish_inline(),
                TagEnd::CodeBlock => {
                    if let Some(Frame::Code(lang, text)) = b.stack.pop() {
                        b.add(Block::Code { lang, text: text.trim_end_matches('\n').to_owned() });
                    }
                }
                TagEnd::Item => {
                    b.close_implicit();
                    if let Some(Frame::Item(task, blocks)) = b.stack.pop()
                        && let Some(Frame::List(_, items)) = b.stack.last_mut()
                    {
                        items.push(Item { task, blocks });
                    }
                }
                TagEnd::TableCell => b.finish_cell(),
                TagEnd::TableHead => {
                    if let Some(Frame::Table { head, row, .. }) = b.stack.last_mut() {
                        *head = std::mem::take(row);
                    }
                }
                TagEnd::TableRow => {
                    if let Some(Frame::Table { rows, row, .. }) = b.stack.last_mut() {
                        rows.push(std::mem::take(row));
                    }
                }
                TagEnd::Table => b.finish_table(),
                TagEnd::List(_) => {
                    b.close_implicit();
                    if let Some(Frame::List(start, items)) = b.stack.pop() {
                        b.add(Block::List { start, items });
                    }
                }
                TagEnd::BlockQuote(_) => {
                    b.close_implicit();
                    if let Some(Frame::Blocks(blocks)) = b.stack.pop() {
                        b.add(Block::Quote(blocks));
                    }
                }
                TagEnd::Emphasis => b.style[1] = b.style[1].saturating_sub(1),
                TagEnd::Strong => b.style[0] = b.style[0].saturating_sub(1),
                TagEnd::Strikethrough => b.style[2] = b.style[2].saturating_sub(1),
                TagEnd::Link => {
                    if let Some((start, url)) = b.links.pop() {
                        let inline = b.inline();
                        let end = inline.text.len();
                        if end > start {
                            inline.links.push((start..end, url));
                        }
                    }
                }
                _ => {}
            },
            Event::Text(text) | Event::Html(text) | Event::InlineHtml(text) | Event::InlineMath(text) | Event::DisplayMath(text) => {
                b.text(&text, false)
            }
            Event::Code(text) => b.text(&text, true),
            Event::SoftBreak => b.text(" ", false),
            Event::HardBreak => b.text("\n", false),
            Event::Rule => {
                b.close_implicit();
                b.add(Block::Rule);
            }
            // The innermost item is a task (the marker may come inside its first paragraph).
            Event::TaskListMarker(checked) => {
                if let Some(Frame::Item(task, _)) = b.stack.iter_mut().rev().find(|f| matches!(f, Frame::Item(..))) {
                    *task = Some(checked);
                }
            }
            _ => {}
        }
    }
    while b.stack.len() > 1 {
        match b.stack.last() {
            Some(Frame::Inline { .. }) if matches!(b.stack.iter().rev().nth(1), Some(Frame::Table { .. })) => b.finish_cell(),
            Some(Frame::Inline { .. }) => b.finish_inline(),
            Some(Frame::Table { .. }) => b.finish_table(),
            Some(Frame::Code(..)) => {
                if let Some(Frame::Code(lang, text)) = b.stack.pop() {
                    b.add(Block::Code { lang, text: text.trim_end_matches('\n').to_owned() });
                }
            }
            _ => {
                b.stack.pop();
            }
        }
    }
    match b.stack.pop() {
        Some(Frame::Blocks(blocks)) => (blocks, b.starts),
        _ => (Vec::new(), Vec::new()),
    }
}

/// A growing (streaming) text's blocks: completed top-level blocks keep
/// their identity and are never parsed again, only the last (open) one is.
#[derive(Debug, Default)]
pub struct Doc {
    src: String,
    /// The text may still grow (a reply streaming): its open table is shown as it will be (see [`Doc::update`]).
    open: bool,
    /// `src[..stable_end]` parsed into `blocks[..stable]`, final.
    stable_end: usize,
    stable: usize,
    blocks: Vec<Block>,
}

impl Doc {
    /// Parse `text`; whatever it shares with the last text's completed blocks is kept. `open`: the text may
    /// still grow (streaming). Then a table at its end doesn't flicker as it arrives: its header row is a
    /// table already (not a paragraph of pipes) while the delimiter row is still to come or half typed, and
    /// it holds only its complete rows (a row still being typed comes with its newline).
    pub fn update(&mut self, text: &str, open: bool) {
        if text == self.src && open == self.open {
            return;
        }
        if !text.starts_with(&self.src[..self.stable_end]) {
            *self = Self::default();
        }
        let open_text = &text[self.stable_end..];
        let (mut tail, mut starts) = parse_top(open_text);
        self.blocks.truncate(self.stable);
        // A block followed by another one is complete: it's final from here on. Not a table right above the
        // last block, though: a row just begun (`|`) isn't one yet, but will be.
        let mut keep = tail.len().saturating_sub(1);
        if keep > 0 && matches!(tail[keep - 1], Block::Table { .. }) && !blank_before(open_text, starts[keep]) {
            keep -= 1;
        }
        if keep > 0 {
            let cut = open_text[..starts[keep]].rfind('\n').map_or(0, |i| i + 1);
            if cut > 0 {
                let rest = tail.split_off(keep);
                self.stable += tail.len();
                self.stable_end += cut;
                self.blocks.append(&mut tail);
                tail = rest;
                starts = starts.split_off(keep).into_iter().map(|s| s - cut).collect();
            }
        }
        if open {
            settle_open_table(&text[self.stable_end..], &starts, &mut tail);
        }
        self.blocks.append(&mut tail);
        self.src.clear();
        self.src.push_str(text);
        self.open = open;
    }

    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }
}

/// Whether a blank line comes right before `at` (a block's start) in `source`.
fn blank_before(source: &str, at: usize) -> bool {
    let mut lines = source[..at].rsplit('\n');
    lines.next();
    lines.next().is_some_and(|l| l.trim().is_empty())
}

/// A streaming text's last block, when it's a table being written (`blocks` parsed from `source`, starting
/// at `starts`): a paragraph ending in a header row (`| a | b`), maybe a delimiter row begun (`|:--|-`),
/// reads as the table it's becoming; a table's row still being typed (no newline yet, or only its first
/// pipe) is left out until complete.
fn settle_open_table(source: &str, starts: &[usize], blocks: &mut Vec<Block>) {
    let n = blocks.len();
    if n == 0 || starts.len() != n {
        return;
    }
    // A row just begun under a table (`|`, no cell yet) reads as a paragraph: it's the table's next row.
    if n > 1
        && matches!(blocks[n - 2], Block::Table { .. })
        && matches!(&blocks[n - 1], Block::Para(_))
        && !blank_before(source, starts[n - 1])
        && source[starts[n - 1]..].chars().all(|c| matches!(c, '|' | ' ' | '\t' | '\n'))
    {
        blocks.pop();
        return;
    }
    let last_at = starts[n - 1];
    let Some(last) = blocks.last_mut() else { return };
    match last {
        Block::Table { rows, .. } => {
            if !source.ends_with('\n') {
                rows.pop();
            }
        }
        Block::Para(_) => {
            if let Some(mut guess) = source.get(last_at..).and_then(table_ahead) {
                blocks.pop();
                blocks.append(&mut guess);
            }
        }
        _ => {}
    }
}

/// A delimiter row in the making: only pipes, colons, dashes and spaces (maybe nothing yet).
fn delimiter_prefix(line: &str) -> bool {
    line.chars().all(|c| matches!(c, '|' | ':' | '-' | ' ' | '\t'))
}

/// The cells of a table row: between its pipes (unescaped), less a leading and a trailing one.
fn row_cells(line: &str) -> Vec<&str> {
    let line = line.trim();
    let line = line.strip_prefix('|').unwrap_or(line);
    let line = if line.ends_with('|') && !line.ends_with("\\|") { &line[..line.len() - 1] } else { line };
    let mut cells = Vec::new();
    let (mut from, mut escaped) = (0, false);
    for (i, c) in line.char_indices() {
        match c {
            '|' if !escaped => {
                cells.push(&line[from..i]);
                from = i + 1;
            }
            _ => {}
        }
        escaped = c == '\\' && !escaped;
    }
    cells.push(&line[from..]);
    cells
}

/// The blocks a paragraph `source` that ends in a table's header row (and maybe its delimiter row, begun)
/// becomes once the delimiter row is complete: parsed with it completed (the alignments typed so far kept).
fn table_ahead(source: &str) -> Option<Vec<Block>> {
    let lines: Vec<&str> = source.split('\n').collect();
    let is_header = |l: &str| {
        let indent = l.len() - l.trim_start_matches(' ').len();
        indent < 4 && l.trim_start().starts_with('|') && !row_cells(l).iter().all(|c| c.trim().is_empty())
    };
    let (keep, header, partial) = match lines.as_slice() {
        [before @ .., header, partial] if is_header(header) && delimiter_prefix(partial) => (before, *header, *partial),
        [before @ .., header] if is_header(header) => (before, *header, ""),
        _ => return None,
    };
    let typed = if partial.trim().is_empty() { Vec::new() } else { row_cells(partial) };
    let delimiter: Vec<String> = (0..row_cells(header).len())
        .map(|i| {
            let cell = typed.get(i).map_or("", |c| c.trim());
            let left = if cell.starts_with(':') { ":" } else { "" };
            let right = if cell.len() > 1 && cell.ends_with(':') { ":" } else { "" };
            format!("{left}---{right}")
        })
        .collect();
    let mut guess = keep.join("\n");
    if !keep.is_empty() {
        guess.push('\n');
    }
    guess.push_str(header);
    guess.push_str(&format!("\n|{}|\n", delimiter.join("|")));
    let (mut blocks, _) = parse_top(&guess);
    // The header row still being typed: a table begun, nothing to draw yet.
    if lines.len() == keep.len() + 1
        && let Some(Block::Table { head, .. }) = blocks.last_mut()
    {
        head.clear();
    }
    matches!(blocks.last(), Some(Block::Table { .. })).then_some(blocks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn para(blocks: &[Block], i: usize) -> &Inline {
        match &blocks[i] {
            Block::Para(inline) => inline,
            other => panic!("not a paragraph: {other:?}"),
        }
    }

    #[test]
    fn inline_styles_and_links() {
        let blocks = parse("Use **bold**, *it*, `code` and [docs](https://x.dev).");
        let p = para(&blocks, 0);
        assert_eq!(p.text, "Use bold, it, code and docs.");
        let styled: Vec<(&str, Style)> = p.spans.iter().map(|(r, s)| (&p.text[r.clone()], *s)).collect();
        assert_eq!(
            styled,
            [
                ("bold", Style { bold: true, ..Style::default() }),
                ("it", Style { italic: true, ..Style::default() }),
                ("code", Style { code: true, ..Style::default() }),
            ]
        );
        assert_eq!(p.links, [(23..27, "https://x.dev".to_owned())]);
    }

    #[test]
    fn blocks_headings_lists_code_quotes() {
        let src = "# Title\n\nText\nmore\n\n- one\n- two **b**\n\n1. first\n2. second\n\n```rust\nfn x() {}\n```\n\n> quoted\n\n---\n";
        let blocks = parse(src);
        assert!(matches!(&blocks[0], Block::Heading(1, i) if i.text == "Title"));
        assert_eq!(para(&blocks, 1).text, "Text more");
        match &blocks[2] {
            Block::List { start: None, items } => {
                assert_eq!(items.len(), 2);
                assert!(matches!(&items[1].blocks[0], Block::Para(i) if i.text == "two b" && i.spans.len() == 1));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(&blocks[3], Block::List { start: Some(1), items } if items.len() == 2));
        assert_eq!(blocks[4], Block::Code { lang: Some("rust".into()), text: "fn x() {}".into() });
        assert!(matches!(&blocks[5], Block::Quote(inner) if matches!(&inner[0], Block::Para(i) if i.text == "quoted")));
        assert_eq!(blocks[6], Block::Rule);
    }

    #[test]
    fn nested_lists_and_unclosed_fences_while_streaming() {
        let blocks = parse("- a\n  - b\n");
        match &blocks[0] {
            Block::List { items, .. } => assert!(matches!(&items[0].blocks[1], Block::List { items, .. } if items.len() == 1)),
            other => panic!("{other:?}"),
        }
        let blocks = parse("Here:\n\n```sh\nls -la\n");
        assert_eq!(blocks[1], Block::Code { lang: Some("sh".into()), text: "ls -la".into() });
    }

    const REPLY: &str = "# Plan\n\nFirst **bold** and `code`,\nwrapped on.\n\n- one\n- two *it*\n\n  more of two\n\n1. first\n2. second\n\n\
                         ```rust\nfn x() {\n    1\n}\n```\n\n> quoted\n> on\n\n---\n\nSetext\n---\n\nEnd.\n";

    #[test]
    fn doc_streamed_in_any_steps_matches_a_full_parse() {
        for step in [1, 3, 7, 40] {
            let mut doc = Doc::default();
            let mut end = 0;
            while end < REPLY.len() {
                end = (end + step).min(REPLY.len());
                while !REPLY.is_char_boundary(end) {
                    end += 1;
                }
                doc.update(&REPLY[..end], true);
                assert_eq!(doc.blocks(), parse(&REPLY[..end]), "step {step} at {end}");
            }
            assert!(doc.stable > 5, "completed blocks were kept: {}", doc.stable);
        }
    }

    #[test]
    fn doc_keeps_completed_blocks_and_parses_only_the_open_one() {
        let mut doc = Doc::default();
        doc.update("Para one.\n\n- a\n", true);
        assert_eq!((doc.stable, doc.stable_end), (1, 11));
        let first = doc.blocks()[0].clone();
        doc.update("Para one.\n\n- a\n- b\n\n```\ncode", true);
        assert_eq!(doc.blocks()[0], first);
        assert_eq!((doc.stable, doc.stable_end), (2, 20), "the list is complete once the fence opens");
        assert_eq!(doc.blocks()[2], Block::Code { lang: None, text: "code".into() });
        // The reply completes with the text it streamed (its row keeps its key, so this `Doc`): nothing restarts.
        let streamed = doc.blocks().to_vec();
        doc.update("Para one.\n\n- a\n- b\n\n```\ncode", true);
        assert_eq!((doc.blocks(), doc.stable, doc.stable_end), (&streamed[..], 2, 20));
        // Not a continuation (an edit, a retry): parsed afresh.
        doc.update("Other.", true);
        assert_eq!(doc.blocks(), parse("Other."));
        assert_eq!(doc.stable, 0);
    }

    fn table(blocks: &[Block], i: usize) -> (&[Align], &[Inline], &[Vec<Inline>]) {
        match &blocks[i] {
            Block::Table { align, head, rows } => (align, head, rows),
            other => panic!("not a table: {other:?}"),
        }
    }

    fn texts(cells: &[Inline]) -> Vec<&str> {
        cells.iter().map(|c| c.text.as_str()).collect()
    }

    #[test]
    fn tables_with_alignments() {
        let src = "Intro.\n\n| Name | Count | Note | Plain |\n|:-----|------:|:----:|-------|\n| a | 1 | x | p |\n| bb | 22 | yy | q |\n\nAfter.\n";
        let blocks = parse(src);
        assert_eq!(blocks.len(), 3);
        let (align, head, rows) = table(&blocks, 1);
        assert_eq!(align, [Align::Left, Align::Right, Align::Center, Align::None]);
        assert_eq!(texts(head), ["Name", "Count", "Note", "Plain"]);
        assert_eq!(rows.len(), 2);
        assert_eq!(texts(&rows[1]), ["bb", "22", "yy", "q"]);
        assert_eq!(para(&blocks, 2).text, "After.");
        // Without outer pipes too.
        let bare = parse("a | b\n--|--\n1 | 2\n");
        let (_, head, rows) = table(&bare, 0);
        assert_eq!((texts(head), texts(&rows[0])), (vec!["a", "b"], vec!["1", "2"]));
    }

    #[test]
    fn table_cells_keep_their_inline_styles() {
        let src = "| Item | Where |\n|---|---|\n| **bold** and `code` | [docs](https://x.dev) |\n| ~~old~~ *new* | a \\| b |\n";
        let blocks = parse(src);
        let (_, _, rows) = table(&blocks, 0);
        let cell = &rows[0][0];
        assert_eq!(cell.text, "bold and code");
        let styled: Vec<(&str, Style)> = cell.spans.iter().map(|(r, s)| (&cell.text[r.clone()], *s)).collect();
        assert_eq!(styled, [("bold", Style { bold: true, ..Style::default() }), ("code", Style { code: true, ..Style::default() })]);
        assert_eq!(rows[0][1].text, "docs");
        assert_eq!(rows[0][1].links, [(0..4, "https://x.dev".to_owned())]);
        assert_eq!(rows[1][0].text, "old new");
        assert_eq!(rows[1][0].spans.len(), 2);
        assert_eq!(rows[1][1].text, "a | b", "an escaped pipe stays in its cell");
    }

    #[test]
    fn table_rows_with_missing_or_extra_cells_fit_the_columns() {
        let src = "| a | b | c |\n|---|---|---|\n| 1 |\n| 1 | 2 | 3 | 4 | 5 |\n|  | | x |\n";
        let blocks = parse(src);
        let (align, head, rows) = table(&blocks, 0);
        assert_eq!((align.len(), head.len()), (3, 3));
        assert_eq!(texts(&rows[0]), ["1", "", ""], "missing cells are empty");
        assert_eq!(texts(&rows[1]), ["1", "2", "3"], "extra cells are dropped");
        assert_eq!(texts(&rows[2]), ["", "", "x"]);
        // A delimiter row that doesn't match the header: not a table.
        assert!(matches!(&parse("| a | b |\n|---|\n")[0], Block::Para(_)));
    }

    #[test]
    fn task_lists() {
        let src = "- [ ] todo\n- [x] done **now**\n- plain\n\n1. [X] first\n2. second\n\n- loose\n\n- [ ] task\n\n  more\n";
        let blocks = parse(src);
        let Block::List { start: None, items } = &blocks[0] else { panic!("{:?}", blocks[0]) };
        assert_eq!(items.iter().map(|i| i.task).collect::<Vec<_>>(), [Some(false), Some(true), None]);
        assert!(matches!(&items[0].blocks[0], Block::Para(i) if i.text == "todo"));
        assert!(matches!(&items[1].blocks[0], Block::Para(i) if i.text == "done now" && i.spans.len() == 1));
        let Block::List { start: Some(1), items } = &blocks[1] else { panic!("{:?}", blocks[1]) };
        assert_eq!(items.iter().map(|i| i.task).collect::<Vec<_>>(), [Some(true), None]);
        // A loose list: the marker is inside the item's first paragraph.
        let Block::List { items, .. } = &blocks[2] else { panic!("{:?}", blocks[2]) };
        assert_eq!(items.iter().map(|i| i.task).collect::<Vec<_>>(), [None, Some(false)]);
        assert!(matches!(&items[1].blocks[..], [Block::Para(a), Block::Para(b)] if a.text == "task" && b.text == "more"));
        // Nested: each item its own.
        let Block::List { items, .. } = &parse("- [x] outer\n  - [ ] inner\n")[0] else { panic!() };
        assert_eq!(items[0].task, Some(true));
        assert!(matches!(&items[0].blocks[1], Block::List { items, .. } if items[0].task == Some(false)));
        // Not a task: brackets elsewhere, smart punctuation off.
        let blocks = parse("- a [ ] b\n\nIt's \"quoted\" -- and...\n");
        assert!(matches!(&blocks[0], Block::List { items, .. } if items[0].task.is_none()));
        assert_eq!(para(&blocks, 1).text, "It's \"quoted\" -- and...");
    }

    const TABLE: &str = "Intro.\n\n| Name | Size |\n|:-----|-----:|\n| `a.rs` | 12 |\n| **b** | 3 |\n\nAfter.\n";

    #[test]
    fn a_streaming_table_never_flickers() {
        let header_end = TABLE.find("|\n|:").expect("header") + 2;
        let first_row = TABLE.find("| `a").expect("row");
        let full_table = parse(TABLE);
        let (_, final_head, final_rows) = table(&full_table, 1);
        let mut rows_seen = 0;
        let mut inc = Doc::default();
        for end in 0..=TABLE.len() {
            let prefix = &TABLE[..end];
            // A full parse of any prefix is fine too (no panics), and a table once its delimiter row is complete.
            let full = parse(prefix);
            if end >= first_row {
                assert!(matches!(&full[1], Block::Table { .. }), "{end}: {full:?}");
            }
            let mut doc = Doc::default();
            doc.update(prefix, true);
            let blocks = doc.blocks();
            // Streamed a character at a time it's the same (completed blocks kept).
            inc.update(prefix, true);
            assert_eq!(inc.blocks(), blocks, "{end}");
            // From its first header cell on it's a table, never a paragraph of pipes in between.
            if end > TABLE.find("| N").unwrap() + 2 && end < TABLE.find("After").unwrap() {
                assert_eq!(blocks.len(), 2, "{end}: {blocks:?}");
                let (align, head, rows) = table(blocks, 1);
                if end < header_end {
                    assert!(head.is_empty(), "{end}: the header row being typed isn't drawn");
                } else {
                    assert_eq!(head, final_head, "{end}: the header is final once its newline came");
                    assert_eq!(align.len(), 2);
                }
                // Only complete rows, never fewer than before; each one as it ends up.
                assert!(rows.len() >= rows_seen, "{end}");
                rows_seen = rows.len();
                assert_eq!(rows, &final_rows[..rows.len()], "{end}");
            }
            // The stream ends where it is: what's there is parsed as is.
            doc.update(prefix, false);
            assert_eq!(doc.blocks(), parse(prefix), "{end}");
        }
        assert_eq!(rows_seen, 2);
        // The alignments typed so far are kept while the delimiter row is written.
        let mut doc = Doc::default();
        doc.update("| a | b |\n|:-:|-", true);
        assert_eq!(table(doc.blocks(), 0).0, [Align::Center, Align::None]);
        // Not a table after all: a paragraph again.
        doc.update("| a | b |\nnot a table", true);
        assert!(matches!(&doc.blocks()[0], Block::Para(_)));
        // A paragraph right above the header row stays one.
        doc.update("Some text\n| a | b |\n|--", true);
        assert!(matches!(doc.blocks(), [Block::Para(p), Block::Table { .. }] if p.text == "Some text"), "{:?}", doc.blocks());
    }

    #[test]
    fn a_streamed_table_in_any_steps_ends_as_a_full_parse() {
        for step in [1, 2, 5, 13] {
            let mut doc = Doc::default();
            let mut end = 0;
            while end < TABLE.len() {
                end = (end + step).min(TABLE.len());
                doc.update(&TABLE[..end], true);
            }
            doc.update(TABLE, false);
            assert_eq!(doc.blocks(), parse(TABLE), "step {step}");
        }
    }
}
