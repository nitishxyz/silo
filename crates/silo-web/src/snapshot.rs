//! Pruned accessibility trees, independent of CEF.

use std::collections::{HashMap, HashSet};

use anyhow::{Result, bail};
use regex::RegexBuilder;
use serde::Deserialize;
use serde_json::Value;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AxNode {
    pub node_id: String,
    #[serde(default)]
    pub ignored: bool,
    #[serde(default)]
    pub role: AxValue,
    #[serde(default)]
    pub name: AxValue,
    #[serde(default)]
    pub value: AxValue,
    #[serde(rename = "backendDOMNodeId")]
    pub backend_dom_node_id: Option<u64>,
    #[serde(default)]
    pub child_ids: Vec<String>,
    #[serde(default)]
    pub properties: Vec<Property>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct AxValue {
    #[serde(default)]
    pub value: Value,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Property {
    pub name: String,
    pub value: AxValue,
}

#[derive(Default)]
pub struct Options {
    pub within: Option<u64>,
    pub depth: Option<u32>,
    pub boxes: HashMap<u64, [f64; 4]>,
}

#[derive(Debug)]
struct Line {
    text: String,
    search: String,
    /// A labelled or valued line, or one whose role carries meaning alone.
    meaningful: bool,
    container: bool,
    parent: Option<usize>,
    depth: usize,
}

/// Refs handed out in one document: a node keeps its ref across snapshots
/// until the page navigates (then a fresh table is used).
#[derive(Debug, Default)]
pub struct Refs {
    by_backend: HashMap<u64, String>,
    by_ref: HashMap<String, u64>,
}

impl Refs {
    /// The node `reference` names.
    #[must_use]
    pub fn get(&self, reference: &str) -> Option<u64> {
        self.by_ref.get(reference).copied()
    }

    fn assign(&mut self, backend: u64) -> String {
        if let Some(reference) = self.by_backend.get(&backend) {
            return reference.clone();
        }
        let reference = format!("e{}", self.by_ref.len() + 1);
        self.by_backend.insert(backend, reference.clone());
        self.by_ref.insert(reference.clone(), backend);
        reference
    }
}

#[derive(Debug, Default)]
pub struct Snapshot {
    /// Refs shown in this snapshot.
    pub refs: HashMap<String, u64>,
    lines: Vec<Line>,
}

fn string(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        _ => value.to_string(),
    }
}

fn quote(s: &str) -> String {
    serde_json::to_string(s).expect("string is JSON")
}

impl Snapshot {
    /// Lines for `nodes`; a node keeps the ref `refs` gave it before.
    pub fn build(nodes: &[AxNode], refs: &mut Refs, options: &Options) -> Result<Self> {
        let by_id: HashMap<&str, &AxNode> = nodes.iter().map(|node| (node.node_id.as_str(), node)).collect();
        let children: HashSet<&str> = nodes.iter().flat_map(|node| node.child_ids.iter().map(String::as_str)).collect();
        let roots: Vec<&AxNode> = match options.within {
            Some(backend) => vec![
                nodes
                    .iter()
                    .find(|node| node.backend_dom_node_id == Some(backend))
                    .ok_or_else(|| anyhow::anyhow!("snapshot subtree is no longer present; take a new snapshot"))?,
            ],
            None => nodes.iter().filter(|node| !children.contains(node.node_id.as_str())).collect(),
        };
        let mut builder = Builder { snapshot: Self::default(), by_id, visited: HashSet::new(), refs, options };
        for root in roots {
            builder.visit(root, None, 0, &[]);
        }
        let mut snapshot = builder.snapshot;
        snapshot.prune();
        Ok(snapshot)
    }

    /// Drop unlabelled containers left with no children (empty paragraphs
    /// and groups), repeatedly, keeping parents valid.
    fn prune(&mut self) {
        loop {
            let parents = self.parents();
            let keep: Vec<bool> = self.lines.iter().enumerate().map(|(i, l)| l.meaningful || !l.container || parents[i]).collect();
            if keep.iter().all(|k| *k) {
                return;
            }
            let mut index = Vec::with_capacity(self.lines.len());
            let mut next = 0;
            for k in &keep {
                index.push(next);
                next += usize::from(*k);
            }
            let lines = std::mem::take(&mut self.lines);
            self.lines = lines
                .into_iter()
                .zip(keep)
                .filter(|(_, k)| *k)
                .map(|(mut line, _)| {
                    line.parent = line.parent.map(|p| index[p]);
                    line
                })
                .collect();
        }
    }

    /// Whether each line has a child line.
    fn parents(&self) -> Vec<bool> {
        let mut has = vec![false; self.lines.len()];
        for line in &self.lines {
            if let Some(parent) = line.parent {
                has[parent] = true;
            }
        }
        has
    }

    fn render(&self, index: usize, parents: &[bool]) -> String {
        let line = &self.lines[index];
        let colon = if line.container && parents[index] { ":" } else { "" };
        format!("{}{}{colon}\n", "  ".repeat(line.depth), line.text)
    }

    pub fn text(&self) -> String {
        let parents = self.parents();
        (0..self.lines.len()).map(|i| self.render(i, &parents)).collect()
    }

    /// Matching nodes and their ancestor paths; omitted siblings become `...`.
    pub fn find(&self, text: Option<&str>, regex: Option<&str>) -> Result<String> {
        let matcher: Box<dyn Fn(&str) -> bool> = match (text, regex) {
            (Some(text), None) => {
                let text = text.to_lowercase();
                Box::new(move |s| s.to_lowercase().contains(&text))
            }
            (None, Some(pattern)) => {
                let (pattern, flags) = pattern.strip_prefix('/').and_then(|s| s.rsplit_once('/')).unwrap_or((pattern, ""));
                if flags.chars().any(|flag| !matches!(flag, 'i' | 'm' | 's')) {
                    bail!("supported regex flags are i, m and s");
                }
                let regex = RegexBuilder::new(pattern)
                    .case_insensitive(flags.contains('i'))
                    .multi_line(flags.contains('m'))
                    .dot_matches_new_line(flags.contains('s'))
                    .size_limit(1 << 20)
                    .build()?;
                Box::new(move |s| regex.is_match(s))
            }
            _ => bail!("provide exactly one of text or regex"),
        };
        let mut included = HashSet::new();
        let mut count = 0;
        for (index, line) in self.lines.iter().enumerate() {
            if matcher(&line.search) {
                count += 1;
                let mut at = Some(index);
                while let Some(index) = at {
                    included.insert(index);
                    at = self.lines[index].parent;
                }
            }
        }
        let mut output = format!("Found {count} match{}:\n", if count == 1 { "" } else { "es" });
        if count == 0 {
            return Ok(output);
        }
        let mut gap = false;
        let parents = self.parents();
        for (index, line) in self.lines.iter().enumerate() {
            if included.contains(&index) {
                output.push_str(&self.render(index, &parents));
                gap = false;
            } else if !gap {
                output.push_str(&format!("{}...\n", "  ".repeat(line.depth)));
                gap = true;
            }
        }
        Ok(output)
    }
}

struct Builder<'a> {
    snapshot: Snapshot,
    by_id: HashMap<&'a str, &'a AxNode>,
    visited: HashSet<&'a str>,
    refs: &'a mut Refs,
    options: &'a Options,
}

impl<'a> Builder<'a> {
    /// `shown`: the nearest shown ancestor's name and value; text repeating
    /// either is left out.
    fn visit(&mut self, node: &'a AxNode, parent: Option<usize>, depth: usize, shown: &[String]) {
        if !self.visited.insert(&node.node_id) || depth > 256 || self.options.depth.is_some_and(|limit| depth > limit as usize) {
            return;
        }
        let role = node.role.value.as_str().unwrap_or("generic");
        let name = string(&node.name.value);
        let value = string(&node.value.value);
        let text_role = matches!(role, "StaticText" | "InlineTextBox");
        let folded = node.ignored || matches!(role, "generic" | "none" | "presentation" | "InlineTextBox" | "LabelText" | "MenuListPopup");
        let duplicate = text_role && (name.trim().is_empty() || shown.iter().any(|s| s.trim() == name.trim()));
        let (parent, child_depth) = if folded || duplicate {
            (parent, depth)
        } else {
            let role = match role {
                "RootWebArea" => "document",
                "StaticText" => "text",
                _ => role,
            };
            let mut text = if role == "text" { format!("- text: {}", quote(&name)) } else { format!("- {role}") };
            if role != "text" && !name.is_empty() {
                text.push_str(&format!(" {}", quote(&name)));
            }
            for property in &node.properties {
                let value = &property.value.value;
                match property.name.as_str() {
                    "checked" if value == true || value == "true" => text.push_str(" [checked]"),
                    "checked" if value == "mixed" => text.push_str(" [checked=mixed]"),
                    "disabled" | "expanded" | "selected" if value == true => text.push_str(&format!(" [{}]", property.name)),
                    "expanded" if value == false => text.push_str(" [expanded=false]"),
                    "level" => text.push_str(&format!(" [level={}]", string(value))),
                    _ => {}
                }
            }
            if let Some(backend) = node.backend_dom_node_id.filter(|_| role != "text") {
                let reference = self.refs.assign(backend);
                text.push_str(&format!(" [ref={reference}]"));
                self.snapshot.refs.insert(reference, backend);
                if let Some([x, y, w, h]) = self.options.boxes.get(&backend) {
                    text.push_str(&format!(" [box={x:.0},{y:.0},{w:.0},{h:.0}]"));
                }
            }
            if !value.is_empty() {
                text.push_str(&format!(": {}", quote(&value)));
            }
            let index = self.snapshot.lines.len();
            let meaningful = !name.is_empty() || !value.is_empty() || !matches!(role, "paragraph" | "group" | "section" | "Section" | "div");
            self.snapshot.lines.push(Line {
                text,
                search: format!("{role} {name} {value}"),
                meaningful,
                container: role != "text" && value.is_empty(),
                parent,
                depth,
            });
            (Some(index), depth + 1)
        };
        let own = [name, value];
        let shown = if folded || duplicate { shown } else { &own[..] };
        for child in &node.child_ids {
            if let Some(child) = self.by_id.get(child.as_str()).copied() {
                self.visit(child, parent, child_depth, shown);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tree() -> Vec<AxNode> {
        serde_json::from_value(json!([
            {"nodeId":"1","role":{"value":"RootWebArea"},"name":{"value":"Form"},"backendDOMNodeId":1,"childIds":["2"]},
            {"nodeId":"2","ignored":true,"role":{"value":"generic"},"childIds":["3","4","5","7"]},
            {"nodeId":"3","role":{"value":"heading"},"name":{"value":"Sign up"},"backendDOMNodeId":3,"properties":[{"name":"level","value":{"value":2}}]},
            {"nodeId":"4","role":{"value":"textbox"},"name":{"value":"Email"},"value":{"value":"a@b.c"},"backendDOMNodeId":4},
            {"nodeId":"5","role":{"value":"button"},"name":{"value":"Submit"},"backendDOMNodeId":5,"childIds":["6"],"properties":[{"name":"disabled","value":{"value":true}}]},
            {"nodeId":"6","role":{"value":"StaticText"},"name":{"value":"Submit"}},
            {"nodeId":"7","role":{"value":"checkbox"},"name":{"value":"Terms"},"backendDOMNodeId":7,"properties":[{"name":"checked","value":{"value":"true"}}]}
        ])).unwrap()
    }

    #[test]
    fn recorded_form_prunes_and_formats() {
        let snapshot = Snapshot::build(&tree(), &mut Refs::default(), &Options::default()).unwrap();
        assert_eq!(
            snapshot.text(),
            "- document \"Form\" [ref=e1]:\n  - heading \"Sign up\" [level=2] [ref=e2]\n  - textbox \"Email\" [ref=e3]: \"a@b.c\"\n  - button \"Submit\" [disabled] [ref=e4]\n  - checkbox \"Terms\" [checked] [ref=e5]\n"
        );
        assert_eq!(snapshot.refs["e4"], 5);
    }

    #[test]
    fn subtree_depth_boxes_and_stable_refs() {
        let options = Options { within: Some(5), depth: Some(0), boxes: HashMap::from([(5, [10., 20., 80., 24.])]) };
        let mut refs = Refs::default();
        let snapshot = Snapshot::build(&tree(), &mut refs, &options).unwrap();
        assert_eq!(snapshot.text(), "- button \"Submit\" [disabled] [ref=e1] [box=10,20,80,24]\n");
        let snapshot = Snapshot::build(&tree(), &mut refs, &Options { depth: Some(0), ..Options::default() }).unwrap();
        assert_eq!(snapshot.refs.len(), 1);
        assert!(snapshot.refs.contains_key("e2"));
        let snapshot = Snapshot::build(&tree(), &mut refs, &Options::default()).unwrap();
        assert!(snapshot.text().contains("- button \"Submit\" [disabled] [ref=e1]"), "a node keeps its ref");
        assert_eq!(refs.get("e1"), Some(5));
        assert!(Snapshot::build(&tree(), &mut refs, &Options { within: Some(999), ..Options::default() }).is_err());
    }

    #[test]
    fn labels_values_and_empty_containers_fold() {
        let nodes: Vec<AxNode> = serde_json::from_value(json!([
            {"nodeId":"1","role":{"value":"form"},"backendDOMNodeId":1,"childIds":["2","4","6","7"]},
            {"nodeId":"2","role":{"value":"LabelText"},"backendDOMNodeId":2,"childIds":["3"]},
            {"nodeId":"3","role":{"value":"StaticText"},"name":{"value":"Email"},"backendDOMNodeId":3},
            {"nodeId":"4","role":{"value":"textbox"},"name":{"value":"Email"},"value":{"value":"a@b.c"},"backendDOMNodeId":4,"childIds":["5"]},
            {"nodeId":"5","role":{"value":"StaticText"},"name":{"value":"a@b.c"},"backendDOMNodeId":5},
            {"nodeId":"6","role":{"value":"paragraph"},"backendDOMNodeId":6},
            {"nodeId":"7","role":{"value":"paragraph"},"backendDOMNodeId":7,"childIds":["8"]},
            {"nodeId":"8","role":{"value":"StaticText"},"name":{"value":"Done"},"backendDOMNodeId":8}
        ]))
        .unwrap();
        let snapshot = Snapshot::build(&nodes, &mut Refs::default(), &Options::default()).unwrap();
        assert_eq!(
            snapshot.text(),
            "- form [ref=e1]:\n  - text: \"Email\"\n  - textbox \"Email\" [ref=e2]: \"a@b.c\"\n  - paragraph [ref=e4]:\n    - text: \"Done\"\n"
        );
    }

    #[test]
    fn find_paths_and_regex() {
        let snapshot = Snapshot::build(&tree(), &mut Refs::default(), &Options::default()).unwrap();
        let found = snapshot.find(Some("SUBMIT"), None).unwrap();
        assert!(found.starts_with("Found 1 match:\n- document"));
        assert!(found.contains("  ...\n  - button"));
        assert!(!found.contains("textbox"));
        assert!(snapshot.find(None, Some("/email|terms/i")).unwrap().starts_with("Found 2 matches:"));
        assert!(snapshot.find(None, Some("[")).is_err());
        assert!(snapshot.find(None, Some("/email/z")).is_err());
        assert_eq!(snapshot.find(Some("not on this page"), None).unwrap(), "Found 0 matches:\n");
        assert!(snapshot.find(None, None).is_err());
        assert!(snapshot.find(Some("x"), Some("x")).is_err());
    }

    #[test]
    fn generic_wrappers_and_expanded_states() {
        let nodes: Vec<AxNode> = serde_json::from_value(json!([
            {"nodeId":"1","role":{"value":"generic"},"childIds":["2","3"]},
            {"nodeId":"2","role":{"value":"button"},"name":{"value":"More"},"backendDOMNodeId":2,"properties":[{"name":"expanded","value":{"value":true}}]},
            {"nodeId":"3","role":{"value":"checkbox"},"name":{"value":"Some"},"backendDOMNodeId":3,"properties":[{"name":"checked","value":{"value":"mixed"}},{"name":"expanded","value":{"value":false}}]}
        ])).unwrap();
        let snapshot = Snapshot::build(&nodes, &mut Refs::default(), &Options { depth: Some(0), ..Options::default() }).unwrap();
        assert_eq!(snapshot.text(), "- button \"More\" [expanded] [ref=e1]\n- checkbox \"Some\" [checked=mixed] [expanded=false] [ref=e2]\n");
    }

    #[test]
    fn cycles_and_escaped_names() {
        let nodes: Vec<AxNode> = serde_json::from_value(json!([
            {"nodeId":"1","role":{"value":"button"},"name":{"value":"say \"hi\"\nnow"},"backendDOMNodeId":1,"childIds":["1"]}
        ]))
        .unwrap();
        let snapshot = Snapshot::build(&nodes, &mut Refs::default(), &Options { within: Some(1), ..Options::default() }).unwrap();
        assert!(snapshot.text().contains("\"say \\\"hi\\\"\\nnow\""));
        assert_eq!(snapshot.refs.len(), 1);
    }
}
