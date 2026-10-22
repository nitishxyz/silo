//! Agent definitions: markdown with frontmatter, Claude Code compatible.
use std::{collections::BTreeMap, path::Path};
#[derive(Clone, Debug, Default)]
pub struct Agent {
    pub name: String,
    pub description: String,
    /// `provider/model` or a model id; `None` inherits.
    pub model: Option<String>,
    /// Granted tool names; `*` suffix patterns (`mcp__github__*`) allowed. Empty: all.
    pub tools: Vec<String>,
    /// Removed from the grant (`disallowedTools`).
    pub disallowed: Vec<String>,
    pub effort: Option<String>,
    pub approval_mode: Option<otto_proto::ApprovalMode>,
    pub prompt: String,
}

/// YAML-subset frontmatter: `key: value`, inline `[a, b]` lists, `- item`
/// block lists (joined with `, `), and `|` / `>` block scalars.
pub(crate) fn frontmatter(text: &str) -> (BTreeMap<String, String>, String) {
    let mut fields = BTreeMap::new();
    let mut lines = text.lines().peekable();
    if lines.peek().map(|l| l.trim_end()) != Some("---") {
        return (fields, text.into());
    }
    lines.next();
    let unquote = |v: &str| v.trim().trim_matches('"').trim_matches('\'').to_owned();
    while let Some(line) = lines.next() {
        if line.trim_end() == "---" {
            break;
        }
        let Some((key, value)) = line.split_once(':').filter(|_| !line.starts_with([' ', '\t', '-', '#'])) else { continue };
        let value = value.trim();
        let mut block = Vec::new();
        while let Some(next) = lines.peek() {
            if next.trim_end() == "---" || !(next.starts_with([' ', '\t']) || next.trim().is_empty() || (value.is_empty() && next.starts_with('-'))) {
                break;
            }
            block.push(lines.next().unwrap_or_default());
        }
        let joined = match value {
            "|" | "|-" => block.iter().map(|l| l.trim()).collect::<Vec<_>>().join("\n").trim().to_owned(),
            ">" | ">-" => block.iter().map(|l| l.trim()).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" "),
            "" => block.iter().filter_map(|l| l.trim().strip_prefix('-')).map(unquote).collect::<Vec<_>>().join(", "),
            v => unquote(v),
        };
        fields.insert(key.trim().to_owned(), joined);
    }
    (fields, lines.collect::<Vec<_>>().join("\n"))
}

pub(crate) fn list(value: Option<String>) -> Vec<String> {
    value
        .unwrap_or_default()
        .trim_matches(['[', ']'])
        .split([',', ' '])
        .map(|s| s.trim().trim_matches(['"', '\'']))
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// Tool names with Claude Code's built-in names mapped to otto's.
fn tools(value: Option<String>) -> Vec<String> {
    list(value)
        .into_iter()
        .flat_map(|name| {
            let mapped: &[&str] = match name.as_str() {
                "Read" => &["read"],
                "Write" => &["write"],
                "Edit" | "MultiEdit" => &["edit", "multiedit", "apply_patch"],
                "Bash" => &["shell", "job"],
                "Glob" => &["glob"],
                "Grep" => &["search"],
                "LS" => &["ls", "tree"],
                "WebFetch" => &["web_fetch"],
                "TodoWrite" => &["update_todos"],
                "Task" => &["subagent"],
                _ => &[],
            };
            if mapped.is_empty() { vec![name] } else { mapped.iter().map(|s| (*s).to_owned()).collect() }
        })
        .collect()
}

fn parse(path: &Path, text: &str) -> Agent {
    let (mut f, prompt) = frontmatter(text);
    let name = f.remove("name").unwrap_or_else(|| path.file_stem().unwrap_or_default().to_string_lossy().into_owned());
    // Claude Code's `inherit` and its family aliases mean "the session's model".
    let model = f.remove("model").filter(|m| !matches!(m.as_str(), "" | "inherit" | "sonnet" | "opus" | "haiku"));
    let approval_mode = f.remove("approval_mode").or_else(|| f.remove("permissionMode")).and_then(|m| match m.as_str() {
        "auto" | "bypassPermissions" | "acceptEdits" => Some(otto_proto::ApprovalMode::Auto),
        "ask_on_effects" | "default" => Some(otto_proto::ApprovalMode::AskOnEffects),
        "ask_all" => Some(otto_proto::ApprovalMode::AskAll),
        "plan" => Some(otto_proto::ApprovalMode::Plan),
        _ => None,
    });
    Agent {
        name,
        description: f.remove("description").unwrap_or_default(),
        model,
        tools: tools(f.remove("tools")),
        disallowed: tools(f.remove("disallowedTools").or_else(|| f.remove("disallowed_tools"))),
        effort: f.remove("effort"),
        approval_mode,
        prompt: prompt.trim().to_owned(),
    }
}

/// Built-ins, then user `agents/`, project `.claude/agents/` and `.otto/agents/`;
/// later definitions replace earlier ones of the same name.
pub fn discover(root: &Path, user: &Path) -> Vec<Agent> {
    let mut agents = BTreeMap::new();
    for name in ["build", "plan", "general", "research"] {
        agents.insert(
            name.into(),
            Agent {
                name: name.into(),
                description: match name {
                    "build" => "Implements changes end to end",
                    "plan" => "Inspects and plans without changing anything",
                    "general" => "General-purpose helper for delegated tasks",
                    _ => "Researches a question and reports findings",
                }
                .into(),
                prompt: match name {
                    "plan" => "Inspect and plan. Do not modify files or run processes.",
                    "research" => "Research the question with read-only tools and report concise findings with file references.",
                    _ => "Complete the user's task accurately and verify the result.",
                }
                .into(),
                approval_mode: (name == "plan").then_some(otto_proto::ApprovalMode::Plan),
                ..Default::default()
            },
        );
    }
    for dir in [user.join("agents"), root.join(".claude/agents"), root.join(".otto/agents")] {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "md")).collect();
        paths.sort();
        for path in paths {
            if let Ok(text) = std::fs::read_to_string(&path) {
                let agent = parse(&path, &text);
                agents.insert(agent.name.clone(), agent);
            }
        }
    }
    agents.into_values().collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn frontmatter_lists_and_blocks() {
        let (f, body) =
            super::frontmatter("---\nname: rev\ndescription: >\n  Reviews\n  code\ntools:\n  - read\n  - search\nmodel: inherit\n---\nBody");
        assert_eq!(f["description"], "Reviews code");
        assert_eq!(f["tools"], "read, search");
        assert_eq!(body, "Body");
        let agent = super::parse(std::path::Path::new("rev.md"), "---\ntools: Read, mcp__gh__*\nmodel: inherit\npermissionMode: plan\n---\nx");
        assert_eq!(agent.tools, ["read", "mcp__gh__*"]);
        assert!(agent.model.is_none());
        assert_eq!(agent.approval_mode, Some(otto_proto::ApprovalMode::Plan));
    }
}
