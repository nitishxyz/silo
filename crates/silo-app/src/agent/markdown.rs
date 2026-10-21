//! Markdown for assistant text: parsed into a few block kinds with styled
//! inline runs; the view draws them. Pure.

use std::ops::Range;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Block {
    Para(Inline),
    Heading(u8, Inline),
    Code { lang: Option<String>, text: String },
    List { start: Option<u64>, items: Vec<Vec<Block>> },
    Quote(Vec<Block>),
    Rule,
}

enum Frame {
    Blocks(Vec<Block>),
    List(Option<u64>, Vec<Vec<Block>>),
    Inline { heading: Option<u8>, inline: Inline, implicit: bool },
    Code(Option<String>, String),
}

struct Builder {
    stack: Vec<Frame>,
    style: [u32; 4],
    links: Vec<(usize, String)>,
}

impl Builder {
    fn style(&self) -> Style {
        Style { bold: self.style[0] > 0, italic: self.style[1] > 0, code: false, strike: self.style[2] > 0 }
    }

    fn add(&mut self, block: Block) {
        match self.stack.last_mut() {
            Some(Frame::Blocks(blocks)) => blocks.push(block),
            Some(Frame::List(_, items)) => {
                if items.is_empty() {
                    items.push(Vec::new());
                }
                items.last_mut().expect("item").push(block);
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

    fn finish_inline(&mut self) {
        if let Some(Frame::Inline { heading, mut inline, .. }) = self.stack.pop() {
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
            if inline.text.is_empty() {
                return;
            }
            self.add(match heading {
                Some(level) => Block::Heading(level, inline),
                None => Block::Para(inline),
            });
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

#[must_use]
pub fn parse(source: &str) -> Vec<Block> {
    let mut b = Builder { stack: vec![Frame::Blocks(Vec::new())], style: [0; 4], links: Vec::new() };
    for event in Parser::new_ext(source, Options::ENABLE_STRIKETHROUGH) {
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
                    b.stack.push(Frame::Blocks(Vec::new()));
                }
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
                    if let Some(Frame::Blocks(blocks)) = b.stack.pop()
                        && let Some(Frame::List(_, items)) = b.stack.last_mut()
                    {
                        items.push(blocks);
                    }
                }
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
            _ => {}
        }
    }
    while b.stack.len() > 1 {
        match b.stack.last() {
            Some(Frame::Inline { .. }) => b.finish_inline(),
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
        Some(Frame::Blocks(blocks)) => blocks,
        _ => Vec::new(),
    }
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
                assert!(matches!(&items[1][0], Block::Para(i) if i.text == "two b" && i.spans.len() == 1));
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
            Block::List { items, .. } => assert!(matches!(&items[0][1], Block::List { items, .. } if items.len() == 1)),
            other => panic!("{other:?}"),
        }
        let blocks = parse("Here:\n\n```sh\nls -la\n");
        assert_eq!(blocks[1], Block::Code { lang: Some("sh".into()), text: "ls -la".into() });
    }
}
