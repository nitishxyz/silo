//! Small HTML helpers for `web_fetch` and `web_search` (no DOM): elements
//! and simple CSS selectors, the page's main content, and markdown.

use regex::Regex;
use reqwest::Url;
use std::sync::LazyLock;

macro_rules! re {
    ($s:expr) => {{
        static R: LazyLock<Regex> = LazyLock::new(|| Regex::new($s).expect("static regex"));
        &*R
    }};
}

const VOID: &[&str] = &["area", "base", "br", "col", "embed", "hr", "img", "input", "link", "meta", "param", "source", "track", "wbr"];

/// Comments and elements whose content is never text.
#[must_use]
pub fn clean(html: &str) -> String {
    let html = re!(r"(?s)<!--.*?-->").replace_all(html, "");
    re!(r"(?is)<(script|style|template|svg|noscript)\b[^>]*>.*?</(?:script|style|template|svg|noscript)\s*>").replace_all(&html, "").into_owned()
}

/// One element: its tag name (lower case), raw attributes and byte ranges.
#[derive(Clone, Debug)]
pub struct Node {
    pub name: String,
    pub attrs: String,
    /// `<` of the start tag; just after its `>`; the end tag's `<`; just after the end tag.
    pub start: usize,
    pub open_end: usize,
    pub close_start: usize,
    pub end: usize,
}

impl Node {
    #[must_use]
    pub fn attr(&self, name: &str) -> Option<String> {
        attr(&self.attrs, name)
    }
    #[must_use]
    pub fn has_class(&self, class: &str) -> bool {
        self.attr("class").is_some_and(|c| c.split_whitespace().any(|c| c == class))
    }
    fn contains(&self, other: &Node) -> bool {
        self.start < other.start && other.end <= self.end
    }
}

#[must_use]
pub fn attr(attrs: &str, name: &str) -> Option<String> {
    re!(r#"([a-zA-Z_:][-a-zA-Z0-9_:.]*)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'=<>`]+)))?"#)
        .captures_iter(attrs)
        .find_map(|c| c[1].eq_ignore_ascii_case(name).then(|| decode(c.get(2).or(c.get(3)).or(c.get(4)).map_or("", |m| m.as_str()))))
}

/// A parsed document: elements matched to their end tags; an element left
/// open ends where its parent does.
pub struct Doc {
    pub html: String,
    pub nodes: Vec<Node>,
}

impl Doc {
    #[must_use]
    pub fn parse(html: &str) -> Self {
        let html = clean(html);
        let mut nodes: Vec<Node> = Vec::new();
        let mut open: Vec<usize> = Vec::new();
        for c in re!(r#"<(/?)([a-zA-Z][a-zA-Z0-9-]*)((?:[^>"']|"[^"]*"|'[^']*')*)>"#).captures_iter(&html) {
            let all = c.get(0).expect("match");
            let name = c[2].to_ascii_lowercase();
            if &c[1] == "/" {
                if let Some(at) = open.iter().rposition(|&i| nodes[i].name == name) {
                    for i in open.drain(at..) {
                        nodes[i].close_start = all.start();
                        nodes[i].end = if nodes[i].name == name { all.end() } else { all.start() };
                    }
                }
                continue;
            }
            let attrs = c[3].to_owned();
            let closed = VOID.contains(&name.as_str()) || attrs.trim_end().ends_with('/');
            let (close_start, end) = if closed { (all.end(), all.end()) } else { (html.len(), html.len()) };
            if !closed {
                open.push(nodes.len());
            }
            nodes.push(Node { name, attrs, start: all.start(), open_end: all.end(), close_start, end });
        }
        Self { html, nodes }
    }
    #[must_use]
    pub fn inner(&self, node: &Node) -> &str {
        &self.html[node.open_end..node.close_start.max(node.open_end)]
    }
    #[must_use]
    pub fn outer(&self, node: &Node) -> &str {
        &self.html[node.start..node.end]
    }
    /// The plain text inside `node`.
    #[must_use]
    pub fn text(&self, node: &Node) -> String {
        text(self.inner(node))
    }
    /// Elements matching `selector`: comma-separated lists of compounds
    /// (`tag`, `#id`, `.class`, `[attr]`, `[attr=v]`, `[attr^=v]`, `[attr*=v]`)
    /// joined by descendant spaces, in document order.
    #[must_use]
    pub fn select(&self, selector: &str) -> Vec<&Node> {
        self.select_in(None, selector)
    }
    /// Like `select`, among the descendants of `scope`.
    #[must_use]
    pub fn select_in(&self, scope: Option<&Node>, selector: &str) -> Vec<&Node> {
        let mut found: Vec<usize> = Vec::new();
        for chain in selector.split(',') {
            let compounds: Vec<Option<Compound>> = chain.split_whitespace().map(Compound::parse).collect();
            if compounds.is_empty() || compounds.iter().any(Option::is_none) {
                continue;
            }
            let compounds: Vec<Compound> = compounds.into_iter().flatten().collect();
            let mut matched: Vec<usize> =
                (0..self.nodes.len()).filter(|&i| scope.is_none_or(|s| s.contains(&self.nodes[i])) && compounds[0].matches(&self.nodes[i])).collect();
            for compound in &compounds[1..] {
                matched = (0..self.nodes.len())
                    .filter(|&i| compound.matches(&self.nodes[i]) && matched.iter().any(|&a| self.nodes[a].contains(&self.nodes[i])))
                    .collect();
            }
            found.extend(matched);
        }
        found.sort_unstable();
        found.dedup();
        found.into_iter().map(|i| &self.nodes[i]).collect()
    }
    /// The innermost element around `node` matching `selector`.
    #[must_use]
    pub fn closest(&self, node: &Node, selector: &str) -> Option<&Node> {
        self.select(selector).into_iter().filter(|a| a.contains(node)).max_by_key(|a| a.start)
    }
    #[must_use]
    pub fn title(&self) -> Option<String> {
        let title = self.select("title").first().map(|t| self.text(t)).filter(|t| !t.is_empty());
        title.or_else(|| self.select("meta[property=og:title]").first().and_then(|m| m.attr("content")).map(|t| t.trim().to_owned()))
    }
    /// The page's main content as HTML: `main` (or `[role=main]`), else the
    /// largest `article`, else the body, without navigation and asides (and,
    /// for the body, headers and footers). A candidate holding little of the
    /// body's text loses to the body.
    #[must_use]
    pub fn main_content(&self) -> String {
        let body = self.select("body").first().copied();
        let body_text = body.map_or_else(|| text(&self.html), |b| self.text(b)).chars().count();
        let main = self.select("main, [role=main]").into_iter().next();
        let article = self.select("article").into_iter().max_by_key(|a| self.text(a).chars().count());
        let pick = main.or(article).filter(|n| {
            let n = self.text(n).chars().count();
            n >= 200 || n * 3 >= body_text
        });
        let (range, drop) = match pick {
            Some(n) => ((n.open_end, n.close_start.max(n.open_end)), "nav, aside, [role=navigation]"),
            None => (
                body.map_or((0, self.html.len()), |b| (b.open_end, b.close_start.max(b.open_end))),
                "nav, aside, header, footer, [role=navigation], [role=banner], [role=contentinfo]",
            ),
        };
        let mut cuts: Vec<(usize, usize)> =
            self.select(drop).into_iter().filter(|n| n.start >= range.0 && n.end <= range.1).map(|n| (n.start, n.end)).collect();
        cuts.sort_unstable();
        let mut out = String::new();
        let mut at = range.0;
        for (start, end) in cuts {
            if start >= at {
                out.push_str(&self.html[at..start]);
                at = end;
            }
        }
        out.push_str(&self.html[at..range.1]);
        out
    }
}

struct Compound {
    tag: Option<String>,
    parts: Vec<(char, String, String)>,
}

impl Compound {
    fn parse(text: &str) -> Option<Self> {
        let c = re!(r"^([a-zA-Z][a-zA-Z0-9-]*|\*)?((?:[#.][-\w]+|\[[^\]]+\])*)$").captures(text)?;
        let tag = c.get(1).map(|m| m.as_str().to_ascii_lowercase()).filter(|t| t != "*");
        let mut parts = Vec::new();
        for p in re!(r"([#.])([-\w]+)|\[\s*([-\w:]+)\s*(?:([\^*]?=)\s*(?:'([^']*)'|\x22([^\x22]*)\x22|([^\]\s]*)))?\s*\]").captures_iter(&c[2]) {
            if let Some(kind) = p.get(1) {
                parts.push((kind.as_str().chars().next().unwrap_or('.'), p[2].to_owned(), String::new()));
            } else {
                let value = p.get(5).or(p.get(6)).or(p.get(7)).map_or("", |m| m.as_str()).to_owned();
                let op = match p.get(4).map(|m| m.as_str()) {
                    None => '?',
                    Some("=") => '=',
                    Some("^=") => '^',
                    Some(_) => '*',
                };
                parts.push((op, p[3].to_owned(), value));
            }
        }
        (tag.is_some() || !parts.is_empty()).then_some(Self { tag, parts })
    }
    fn matches(&self, node: &Node) -> bool {
        self.tag.as_ref().is_none_or(|t| *t == node.name)
            && self.parts.iter().all(|(kind, name, value)| match kind {
                '#' => node.attr("id").as_deref() == Some(name.as_str()),
                '.' => node.has_class(name),
                '?' => node.attr(name).is_some(),
                '=' => node.attr(name).as_deref() == Some(value.as_str()),
                '^' => node.attr(name).is_some_and(|v| v.starts_with(value.as_str())),
                _ => node.attr(name).is_some_and(|v| v.contains(value.as_str())),
            })
    }
}

/// Character references decoded in one pass (`&amp;lt;` stays `&lt;`).
#[must_use]
pub fn decode(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    re!(r"&(#[xX][0-9a-fA-F]{1,6}|#[0-9]{1,7}|[a-zA-Z][a-zA-Z0-9]{1,8});")
        .replace_all(text, |c: &regex::Captures| {
            let name = &c[1];
            let ch = if let Some(hex) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
                u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
            } else if let Some(dec) = name.strip_prefix('#') {
                dec.parse().ok().and_then(char::from_u32)
            } else {
                Some(match name {
                    "amp" => '&',
                    "lt" => '<',
                    "gt" => '>',
                    "quot" => '"',
                    "apos" => '\'',
                    "nbsp" => ' ',
                    "mdash" => '—',
                    "ndash" => '–',
                    "hellip" => '…',
                    "lsquo" => '‘',
                    "rsquo" => '’',
                    "ldquo" => '“',
                    "rdquo" => '”',
                    "middot" => '·',
                    "bull" => '•',
                    "copy" => '©',
                    "reg" => '®',
                    "trade" => '™',
                    "times" => '×',
                    "laquo" => '«',
                    "raquo" => '»',
                    _ => return c[0].to_owned(),
                })
            };
            ch.map_or_else(|| c[0].to_owned(), String::from)
        })
        .into_owned()
}

/// Plain text: tags removed, references decoded, whitespace collapsed.
#[must_use]
pub fn text(html: &str) -> String {
    let html = clean(html);
    let stripped = re!(r"(?s)<[^>]*>").replace_all(&html, " ");
    decode(&stripped).split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Markdown for `html`: headings, links (absolute against `base`), lists,
/// emphasis, code and paragraphs; images, scripts and styles dropped.
#[must_use]
pub fn markdown(html: &str, base: Option<&Url>) -> String {
    let html = clean(html);
    let html = re!(r"(?is)<(head|select|button|iframe|object)\b[^>]*>.*?</(?:head|select|button|iframe|object)\s*>").replace_all(&html, "");
    let mut blocks: Vec<String> = Vec::new();
    let html = re!(r"(?is)<pre\b[^>]*>(.*?)</pre\s*>").replace_all(&html, |c: &regex::Captures| {
        let code = decode(&re!(r"(?s)<[^>]*>").replace_all(&c[1], ""));
        blocks.push(format!("```\n{}\n```", code.trim_matches('\n')));
        format!("<p>\u{1}{}\u{1}</p>", blocks.len() - 1)
    });
    let html = re!(r"\s+").replace_all(&html, " ");
    let html = re!(r"(?is)<a\b([^>]*)>(.*?)</a\s*>").replace_all(&html, |c: &regex::Captures| {
        let label = inline(&c[2]);
        let href = attr(&c[1], "href").filter(|h| !h.is_empty() && !h.starts_with('#') && !h.to_ascii_lowercase().starts_with("javascript:"));
        match href.and_then(|h| base.map_or_else(|| Url::parse(&h).ok().map(String::from), |b| b.join(&h).ok().map(String::from)).or(Some(h))) {
            Some(href) if !label.is_empty() => format!("[{}]({href})", label.replace(['[', ']'], "")),
            _ => label,
        }
    });
    let html = re!(r"(?is)<h([1-6])\b[^>]*>(.*?)</h[1-6]\s*>").replace_all(&html, |c: &regex::Captures| {
        let level: usize = c[1].parse().unwrap_or(1);
        format!("\n\n{} {}\n\n", "#".repeat(level), inline(&c[2]))
    });
    let html = re!(r"(?is)<code\b[^>]*>(.*?)</code\s*>").replace_all(&html, |c: &regex::Captures| wrap(&c[1], "`"));
    let html = re!(r"(?is)<(?:strong|b)\b[^>]*>(.*?)</(?:strong|b)\s*>").replace_all(&html, |c: &regex::Captures| wrap(&c[1], "**"));
    let html = re!(r"(?is)<(?:em|i)\b[^>]*>(.*?)</(?:em|i)\s*>").replace_all(&html, |c: &regex::Captures| wrap(&c[1], "*"));
    let html = re!(r"(?i)<li\b[^>]*>").replace_all(&html, "\n- ");
    let html = re!(r"(?i)<br\b[^>]*>").replace_all(&html, "\n");
    let html = re!(r"(?i)<hr\b[^>]*>").replace_all(&html, "\n\n---\n\n");
    let html = re!(r"(?i)</(?:td|th)\s*>").replace_all(&html, " ");
    let html = re!(
        r"(?i)</?(?:p|div|section|article|main|header|footer|nav|aside|ul|ol|table|thead|tbody|tr|blockquote|figure|figcaption|dl|dt|dd|form|fieldset|details|summary|address)\b[^>]*>"
    )
    .replace_all(&html, "\n\n");
    let html = re!(r"(?s)<[^>]*>").replace_all(&html, "");
    let text = decode(&html);
    let mut out = String::new();
    let mut blank = false;
    for line in text.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if line.is_empty() || line == "-" {
            blank = !out.is_empty();
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blank { "\n\n" } else { "\n" });
        }
        blank = false;
        match line.strip_prefix('\u{1}').and_then(|l| l.strip_suffix('\u{1}')).and_then(|i| i.parse::<usize>().ok()) {
            Some(i) => out.push_str(&blocks[i]),
            None => out.push_str(&line),
        }
    }
    out
}

fn inline(html: &str) -> String {
    re!(r"(?s)<[^>]*>").replace_all(html, "").split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `mark` around the inline text of `html`, keeping the spaces at its ends outside.
fn wrap(html: &str, mark: &str) -> String {
    let t = inline(html);
    let side = |space: bool| if space { " " } else { "" };
    if t.is_empty() {
        side(!html.is_empty()).to_owned()
    } else {
        format!("{}{mark}{t}{mark}{}", side(html.starts_with(char::is_whitespace)), side(html.ends_with(char::is_whitespace)))
    }
}
