//! Lenient YAML-subset frontmatter for world entities: `key: value`
//! scalars (plain, quoted, numbers, booleans, null), inline `[a, b]` and
//! `{…}` (JSON, or `{k: v, …}`), `- item` lists whose items may be
//! one-level tables (`- file: refs/a.png` with more `k: v` lines under it),
//! one level of `k: v` maps and `|`/`>` blocks. Edits replace only the keys
//! they touch, so everything else (unknown keys, comments, layout) stays as
//! written.

use serde_json::{Map, Value};

/// A markdown file split into its frontmatter and body.
#[derive(Clone, Debug, Default)]
pub struct Doc {
    /// Top-level entries in order: the key (`None`: comments or blank
    /// lines before the first key) and its lines.
    blocks: Vec<(Option<String>, Vec<String>)>,
    /// Whether the file had a frontmatter.
    had: bool,
    pub body: String,
}

fn key_of(line: &str) -> Option<&str> {
    if line.starts_with([' ', '\t', '#', '-']) {
        return None;
    }
    let (key, _) = line.split_once(':')?;
    let key = key.trim();
    (!key.is_empty() && !key.contains(['"', '\'', '[', '{'])).then_some(key)
}

impl Doc {
    pub fn parse(text: &str) -> Self {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut lines = text.split_inclusive('\n');
        let first = lines.clone().next().map(str::trim_end);
        if first != Some("---") {
            return Self { blocks: Vec::new(), had: false, body: text.to_owned() };
        }
        lines.next();
        let mut blocks: Vec<(Option<String>, Vec<String>)> = Vec::new();
        let mut consumed = text.split_inclusive('\n').next().map_or(0, str::len);
        let mut closed = false;
        for line in lines {
            consumed += line.len();
            let bare = line.trim_end_matches(['\n', '\r']);
            if bare.trim_end() == "---" || bare.trim_end() == "..." {
                closed = true;
                break;
            }
            match key_of(bare) {
                Some(key) => blocks.push((Some(key.to_owned()), vec![bare.to_owned()])),
                None => match blocks.last_mut() {
                    Some((_, lines)) => lines.push(bare.to_owned()),
                    None => blocks.push((None, vec![bare.to_owned()])),
                },
            }
        }
        if !closed {
            // Not a frontmatter after all.
            return Self { blocks: Vec::new(), had: false, body: text.to_owned() };
        }
        let body = text[consumed.min(text.len())..].to_owned();
        Self { blocks, had: true, body }
    }

    /// The frontmatter as JSON (unknown keys included).
    pub fn fields(&self) -> Map<String, Value> {
        let mut out = Map::new();
        for (key, lines) in &self.blocks {
            let Some(key) = key else { continue };
            out.insert(key.clone(), block_value(lines));
        }
        out
    }

    /// Set `key` to `value` (`null` removes it), keeping every other line.
    /// A list is written on one line (`[a, b]`), unless it holds a table or
    /// replaces a list written one item a line: then one item a line.
    pub fn set(&mut self, key: &str, value: &Value) {
        let at = self.blocks.iter().position(|(k, _)| k.as_deref() == Some(key));
        let lines = |was: Option<&[String]>| match value {
            Value::Array(items) if !items.is_empty() && (items.iter().any(Value::is_object) || was.is_some_and(is_sequence)) => {
                block_list(key, items)
            }
            _ => vec![format!("{key}: {}", inline(value))],
        };
        match (value, at) {
            (Value::Null, Some(at)) => drop(self.blocks.remove(at)),
            (Value::Null, None) => {}
            (_, Some(at)) => self.blocks[at].1 = lines(Some(&self.blocks[at].1)),
            (_, None) => self.blocks.push((Some(key.to_owned()), lines(None))),
        }
        self.had = self.had || !self.blocks.is_empty();
    }

    pub fn render(&self) -> String {
        if !self.had || self.blocks.is_empty() {
            return self.body.clone();
        }
        let mut out = String::from("---\n");
        for (_, lines) in &self.blocks {
            for line in lines {
                out.push_str(line);
                out.push('\n');
            }
        }
        out.push_str("---\n");
        out.push_str(&self.body);
        out
    }
}

/// Whether `key` may be written as a frontmatter key.
pub fn valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= 64 && key_of(&format!("{key}: x")) == Some(key) && !key.chars().any(char::is_control)
}

fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Whether a block's lines are a `key:` with `- item` lines under it.
fn is_sequence(lines: &[String]) -> bool {
    let rest = lines[0].split_once(':').map_or("", |(_, r)| r).trim();
    (rest.is_empty() || rest.starts_with('#'))
        && lines[1..].iter().map(|l| l.trim_start()).find(|l| !l.is_empty() && !l.starts_with('#')).is_some_and(is_item)
}

/// Whether `text` (trimmed at the start) is a `- item` line.
fn is_item(text: &str) -> bool {
    text == "-" || text.starts_with("- ") || text.starts_with("-\t")
}

/// `items` one a line under `key:`; a table's keys one a line under its
/// dash, `file`, `name` and `defines` first.
fn block_list(key: &str, items: &[Value]) -> Vec<String> {
    const FIRST: [&str; 4] = ["file", "path", "name", "defines"];
    let mut out = vec![format!("{key}:")];
    for item in items {
        match item {
            Value::Object(map) if !map.is_empty() => {
                let keys = FIRST.iter().filter_map(|k| map.get_key_value(*k)).chain(map.iter().filter(|(k, _)| !FIRST.contains(&k.as_str())));
                for (i, (k, v)) in keys.enumerate() {
                    out.push(format!("{}{k}: {}", if i == 0 { "  - " } else { "    " }, block_scalar(v)));
                }
            }
            other => out.push(format!("  - {}", block_scalar(other))),
        }
    }
    out
}

/// `value` as a scalar of a block list: as [`inline`], but a string with
/// commas or brackets inside is plain (no flow list to end).
fn block_scalar(value: &Value) -> String {
    match value {
        Value::String(s)
            if !s.is_empty()
                && s.trim() == s
                && !s.starts_with(['-', '?', ':', ',', '[', ']', '{', '}', '#', '&', '*', '!', '|', '>', '\'', '"', '%', '@', '`'])
                && !s.contains(['\n', '\r', '\t'])
                && !s.contains(": ")
                && !s.contains(" #")
                && !s.ends_with(':')
                && matches!(scalar(s), Value::String(ref t) if t == s) =>
        {
            s.clone()
        }
        other => inline(other),
    }
}

/// `key: value` in a table: the key (no quotes or brackets, no ` #`) and
/// the rest; `None` for a plain scalar (`refs/a.png`, `"a: b"`).
fn pair(text: &str) -> Option<(&str, &str)> {
    if text.starts_with(['"', '\'', '[', '{', '#']) {
        return None;
    }
    let (key, value) = match text.split_once(": ").or_else(|| text.split_once(":\t")) {
        Some(kv) => kv,
        None => (text.strip_suffix(':')?, ""),
    };
    let key = key.trim();
    (!key.is_empty() && !key.contains(['"', '\'', '[', '{', '#']) && !key.contains(" #")).then_some((key, value.trim()))
}

/// A `- item` list (the block's lines after its key, blanks and comments
/// left out): each item a scalar, or a table when its dash line is a
/// `k: v` (the more indented lines under it add to it).
fn sequence(lines: &[&str]) -> Value {
    let base = indent(lines[0]);
    let mut items: Vec<Vec<&str>> = Vec::new();
    for line in lines {
        if indent(line) <= base && is_item(line.trim_start()) {
            items.push(vec![line]);
        } else if let Some(item) = items.last_mut() {
            item.push(line);
        }
    }
    Value::Array(
        items
            .iter()
            .map(|item| {
                let first = item[0].trim_start()[1..].trim();
                let rest = item[1..].iter().map(|l| l.trim());
                if pair(first).is_none() && !(first.is_empty() && item[1..].iter().any(|l| pair(l.trim()).is_some())) {
                    // A plain item, maybe folded over more lines.
                    let text: Vec<&str> = std::iter::once(first).chain(rest).filter(|t| !t.is_empty()).collect();
                    return scalar(&text.join(" "));
                }
                let mut map = Map::new();
                for text in std::iter::once(first).chain(rest) {
                    if let Some((k, v)) = pair(text) {
                        map.insert(k.to_owned(), scalar(v));
                    }
                }
                Value::Object(map)
            })
            .collect(),
    )
}

fn block_value(lines: &[String]) -> Value {
    let rest = lines[0].split_once(':').map_or("", |(_, r)| r).trim();
    let more: Vec<&str> = lines[1..].iter().map(String::as_str).filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#')).collect();
    if rest.starts_with('|') || rest.starts_with('>') {
        let base = more.iter().map(|l| indent(l)).min().unwrap_or(0);
        let parts: Vec<&str> = lines[1..].iter().map(|l| l.get(base..).unwrap_or("").trim_end()).collect();
        let text = if rest.starts_with('|') {
            parts.join("\n")
        } else {
            parts.iter().map(|p| p.trim()).filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" ")
        };
        return Value::String(text.trim_end().to_owned() + if rest.starts_with('|') && !rest.contains('-') { "\n" } else { "" });
    }
    if !rest.is_empty() && !rest.starts_with('#') {
        return scalar(rest);
    }
    if more.is_empty() {
        return Value::Null;
    }
    if is_item(more[0].trim_start()) {
        return sequence(&more);
    }
    let mut map = Map::new();
    for line in more {
        if let Some((k, v)) = line.trim().split_once(':') {
            map.insert(k.trim().to_owned(), scalar(v.trim()));
        }
    }
    Value::Object(map)
}

/// Split a flow list's inside at top-level commas, quotes respected.
fn split_flow(inner: &str) -> Vec<String> {
    let (mut out, mut cur, mut quote, mut depth) = (Vec::new(), String::new(), None::<char>, 0i32);
    for c in inner.chars() {
        match (quote, c) {
            (Some(q), c) if c == q => {
                quote = None;
                cur.push(c);
            }
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => {
                quote = Some(c);
                cur.push(c);
            }
            (None, '[' | '{') => {
                depth += 1;
                cur.push(c);
            }
            (None, ']' | '}') => {
                depth -= 1;
                cur.push(c);
            }
            (None, ',') if depth == 0 => out.push(std::mem::take(&mut cur)),
            (None, c) => cur.push(c),
        }
    }
    out.push(cur);
    out.into_iter().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()).collect()
}

fn scalar(text: &str) -> Value {
    let text = text.trim();
    if let Some(inner) = text.strip_prefix('"') {
        if let Ok(Value::String(s)) = serde_json::from_str::<Value>(text.split_once(" #").map_or(text, |(a, _)| a).trim()) {
            return Value::String(s);
        }
        if let Ok(Value::String(s)) = serde_json::from_str::<Value>(text) {
            return Value::String(s);
        }
        return Value::String(inner.trim_end_matches('"').to_owned());
    }
    if let Some(inner) = text.strip_prefix('\'') {
        let end = inner.rfind('\'').unwrap_or(inner.len());
        return Value::String(inner[..end].replace("''", "'"));
    }
    if text.starts_with('[') && text.ends_with(']') {
        return Value::Array(split_flow(&text[1..text.len() - 1]).iter().map(|s| scalar(s)).collect());
    }
    if text.starts_with('{') {
        if let Ok(v) = serde_json::from_str(text) {
            return v;
        }
        // `{file: refs/a.png, name: A}`.
        if let Some(inner) = text.strip_suffix('}').map(|t| &t[1..]) {
            let parts = split_flow(inner);
            let pairs: Option<Vec<(&str, &str)>> = parts.iter().map(|p| pair(p)).collect();
            if let Some(pairs) = pairs {
                return Value::Object(pairs.into_iter().map(|(k, v)| (k.to_owned(), scalar(v))).collect());
            }
        }
        return Value::String(text.to_owned());
    }
    let text = text.split_once(" #").map_or(text, |(a, _)| a).trim();
    match text {
        "" | "~" | "null" | "Null" | "NULL" => Value::Null,
        "true" | "True" | "TRUE" | "yes" => Value::Bool(true),
        "false" | "False" | "FALSE" | "no" => Value::Bool(false),
        _ => {
            if let Ok(i) = text.parse::<i64>() {
                return Value::from(i);
            }
            if let Ok(f) = text.parse::<f64>()
                && f.is_finite()
                && text.chars().any(|c| c.is_ascii_digit())
            {
                return Value::from(f);
            }
            Value::String(text.to_owned())
        }
    }
}

/// `text` without quotes where YAML reads it back as the same string.
fn plain(text: &str) -> bool {
    !text.is_empty()
        && text.trim() == text
        && !text.starts_with(['-', '?', ':', ',', '[', ']', '{', '}', '#', '&', '*', '!', '|', '>', '\'', '"', '%', '@', '`'])
        && !text.contains([',', '[', ']', '{', '}', '\n', '\r', '\t'])
        && !text.contains(": ")
        && !text.contains(" #")
        && !text.ends_with(':')
        && matches!(scalar(text), Value::String(ref s) if s == text)
}

/// `value` as one line of YAML.
pub fn inline(value: &Value) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) if plain(s) => s.clone(),
        Value::String(s) => serde_json::to_string(s).unwrap_or_default(),
        Value::Array(items) => format!("[{}]", items.iter().map(inline).collect::<Vec<_>>().join(", ")),
        Value::Object(_) => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn lists_hold_paths_and_tables() {
        let doc = Doc::parse(
            "---\nrefs:\n  - refs/face.png\n  - file: refs/crown-off.png   # the bare head\n    name: Crown off\n    defines: his bare head and hair, without the crown\n  # a comment\n  -\n    file: refs/back.png\n  - a long\n    folded path\n  - \"quoted: not a table\"\nflow: [refs/a.png, {file: refs/b.png, name: B}, {\"file\": \"refs/c.png\"}]\nlinks: [props/mace]\n---\nbody\n",
        );
        let f = doc.fields();
        assert_eq!(
            f["refs"],
            json!([
                "refs/face.png",
                {"file": "refs/crown-off.png", "name": "Crown off", "defines": "his bare head and hair, without the crown"},
                {"file": "refs/back.png"},
                "a long folded path",
                "quoted: not a table",
            ])
        );
        assert_eq!(f["flow"], json!(["refs/a.png", {"file": "refs/b.png", "name": "B"}, {"file": "refs/c.png"}]));
        assert_eq!(f["links"], json!(["props/mace"]));
        assert_eq!(doc.body, "body\n");
    }

    #[test]
    fn a_list_is_written_in_its_style_and_reads_back() {
        let text = "---\nname: King\nrefs:\n  - refs/a.png\n  - refs/b.png\nlinks: [props/mace]\nvoice: none\n---\n\n# King\n";
        let mut doc = Doc::parse(text);
        // A block list stays one item a line; a table's keys go under its dash, file first.
        let refs = json!(["refs/a.png", {"defines": "the crown, alone: [gold]", "file": "refs/b.png", "name": "Crown", "angle": 3}]);
        doc.set("refs", &refs);
        let out = doc.render();
        assert_eq!(
            out,
            "---\nname: King\nrefs:\n  - refs/a.png\n  - file: refs/b.png\n    name: Crown\n    defines: \"the crown, alone: [gold]\"\n    angle: 3\nlinks: [props/mace]\nvoice: none\n---\n\n# King\n"
        );
        assert_eq!(Doc::parse(&out).fields()["refs"], refs, "reads back");
        // A flow list stays on one line; plain strings only.
        doc.set("links", &json!(["props/mace", "props/crown"]));
        doc.set("refs", &json!(["refs/a.png"]));
        let out = doc.render();
        assert!(out.contains("refs:\n  - refs/a.png\nlinks: [props/mace, props/crown]\n"), "{out}");
        // Commas inside a table's value need no quotes; a list emptied is `[]`.
        doc.set("refs", &json!([{"file": "refs/a.png", "defines": "his face, eyes closed"}]));
        assert!(doc.render().contains("  - file: refs/a.png\n    defines: his face, eyes closed\n"));
        doc.set("refs", &json!([]));
        assert!(doc.render().contains("refs: []\n"));
        assert_eq!(Doc::parse(&doc.render()).fields()["refs"], json!([]));
    }
}
