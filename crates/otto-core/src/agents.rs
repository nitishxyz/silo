//! Agent definitions: markdown with frontmatter, Claude Code compatible.
use otto_proto::{AgentDef, AgentScope};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

/// Built-in agent names; a user or project file of the same name overrides one.
pub const BUILTINS: &[&str] = &["build"];
#[derive(Clone, Debug, Default)]
pub struct Agent {
    pub name: String,
    pub description: String,
    /// `provider/model` or a model id; `None` inherits.
    pub model: Option<String>,
    /// Tools shown to the model; `*` suffix patterns (`mcp__github__*`) allowed.
    /// Empty with `loadable` empty: every tool, each with its own default loading.
    pub tools: Vec<String>,
    /// Tools granted behind `load_tools` (`loadable`). A tool in neither list is not granted.
    pub loadable: Vec<String>,
    /// Removed from the grant (`disallowedTools`).
    pub disallowed: Vec<String>,
    pub effort: Option<String>,
    pub approval_mode: Option<otto_proto::ApprovalMode>,
    pub prompt: String,
    pub scope: AgentScope,
    /// The file it was read from; `None` for built-ins.
    pub path: Option<PathBuf>,
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

fn parse(path: &Path, text: &str, scope: AgentScope) -> Agent {
    let (mut f, prompt) = frontmatter(text);
    let name = f.remove("name").unwrap_or_else(|| path.file_stem().unwrap_or_default().to_string_lossy().into_owned());
    // Claude Code's `inherit` and its family aliases mean "the session's model".
    let model = f.remove("model").filter(|m| !matches!(m.as_str(), "" | "inherit" | "sonnet" | "opus" | "haiku"));
    let approval_mode = f.remove("approval_mode").or_else(|| f.remove("permissionMode")).and_then(|m| match m.as_str() {
        "auto" | "bypassPermissions" | "acceptEdits" => Some(otto_proto::ApprovalMode::Auto),
        "ask_on_effects" | "default" => Some(otto_proto::ApprovalMode::AskOnEffects),
        "ask_all" => Some(otto_proto::ApprovalMode::AskAll),
        _ => None,
    });
    Agent {
        name,
        description: f.remove("description").unwrap_or_default(),
        model,
        tools: tools(f.remove("tools")),
        loadable: tools(f.remove("loadable")),
        disallowed: tools(f.remove("disallowedTools").or_else(|| f.remove("disallowed_tools"))),
        effort: f.remove("effort"),
        approval_mode,
        prompt: prompt.trim().to_owned(),
        scope,
        path: Some(path.to_owned()),
    }
}

fn dirs(root: &Path, user: &Path, scope: AgentScope) -> Vec<PathBuf> {
    match scope {
        AgentScope::Builtin => vec![],
        AgentScope::User => vec![user.join("agents")],
        AgentScope::Project => vec![root.join(".claude/agents"), root.join(".otto/agents")],
    }
}

fn scan(dir: &Path, scope: AgentScope) -> Vec<Agent> {
    let Ok(entries) = std::fs::read_dir(dir) else { return vec![] };
    let mut paths: Vec<_> = entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "md")).collect();
    paths.sort();
    paths.into_iter().filter_map(|path| Some(parse(&path, &std::fs::read_to_string(&path).ok()?, scope))).collect()
}

/// Built-ins, then user `agents/`, project `.claude/agents/` and `.otto/agents/`;
/// later definitions replace earlier ones of the same name.
pub fn discover(root: &Path, user: &Path) -> Vec<Agent> {
    let build = Agent {
        name: "build".into(),
        description: "Implements changes end to end".into(),
        prompt: "Complete the user's task accurately and verify the result.".into(),
        ..Default::default()
    };
    let mut agents = BTreeMap::from([(build.name.clone(), build)]);
    for scope in [AgentScope::User, AgentScope::Project] {
        for dir in dirs(root, user, scope) {
            agents.extend(scan(&dir, scope).into_iter().map(|a| (a.name.clone(), a)));
        }
    }
    agents.into_values().collect()
}

/// `name`'s definition within `scope`, the one discovery picks there.
fn find(root: &Path, user: &Path, scope: AgentScope, name: &str) -> Option<Agent> {
    dirs(root, user, scope).iter().flat_map(|dir| scan(dir, scope)).rfind(|a| a.name == name)
}

impl Agent {
    #[must_use]
    pub fn def(self) -> AgentDef {
        AgentDef {
            overrides_builtin: self.scope != AgentScope::Builtin && BUILTINS.contains(&self.name.as_str()),
            name: self.name,
            description: self.description,
            model: self.model,
            effort: self.effort,
            approval_mode: self.approval_mode,
            prompt: self.prompt,
            tools: self.tools,
            loadable: self.loadable,
            disallowed: self.disallowed,
            scope: self.scope,
        }
    }
}

fn check_name(name: &str) -> crate::Result<()> {
    if (1..=64).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-') {
        Ok(())
    } else {
        Err(crate::Error::Invalid("agent names use 1-64 letters, digits, _ or -".into()))
    }
}

/// Frontmatter otto reads back to the same definition.
pub(crate) fn render(def: &AgentDef) -> String {
    let mut text = String::from("---\n");
    let mut scalar = |key: &str, value: &str| {
        let lines: Vec<&str> = value.lines().map(str::trim).collect();
        if value.trim().is_empty() {
            return;
        }
        if lines.len() == 1 && !value.starts_with(['"', '\'', ' ']) && !value.ends_with(['"', '\'', ' ']) && !matches!(value, "|" | "|-" | ">" | ">-")
        {
            text.push_str(&format!("{key}: {value}\n"));
        } else {
            text.push_str(&format!("{key}: |\n"));
            lines.iter().for_each(|line| text.push_str(&format!("  {line}\n")));
        }
    };
    scalar("name", &def.name);
    scalar("description", &def.description);
    scalar("model", def.model.as_deref().unwrap_or_default());
    scalar("effort", def.effort.as_deref().unwrap_or_default());
    let mode = def.approval_mode.map(|m| serde_json::to_value(m).ok().and_then(|v| v.as_str().map(String::from)).unwrap_or_default());
    scalar("approval_mode", mode.as_deref().unwrap_or_default());
    scalar("tools", &def.tools.join(", "));
    scalar("loadable", &def.loadable.join(", "));
    scalar("disallowedTools", &def.disallowed.join(", "));
    text.push_str(&format!("---\n{}\n", def.prompt.trim()));
    text
}

/// Write `def` to its scope's folder (`root` is the project for `Project`); a
/// different `previous` removes that scope's old definition (rename).
pub(crate) fn save(root: Option<&Path>, user: &Path, def: &AgentDef, previous: Option<&str>) -> crate::Result<()> {
    check_name(&def.name)?;
    previous.map(check_name).transpose()?;
    let (dir, root) = match (def.scope, root) {
        (AgentScope::Builtin, _) => return Err(crate::Error::Invalid("built-in agents can't be changed".into())),
        (AgentScope::User, root) => (user.join("agents"), root.unwrap_or(user)),
        (AgentScope::Project, Some(root)) => (root.join(".otto/agents"), root),
        (AgentScope::Project, None) => return Err(crate::Error::Invalid("project agents need cwd".into())),
    };
    let target = dir.join(format!("{}.md", def.name));
    let mut stale: Vec<PathBuf> =
        previous.filter(|p| *p != def.name).and_then(|p| find(root, user, def.scope, p)).and_then(|a| a.path).into_iter().collect();
    // Another file of this folder defining the same name would shadow the new one.
    stale.extend(scan(&dir, def.scope).into_iter().filter(|a| a.name == def.name).filter_map(|a| a.path));
    std::fs::create_dir_all(&dir)?;
    let temporary = dir.join(format!(".{}.md.tmp", def.name));
    std::fs::write(&temporary, render(def))?;
    std::fs::rename(&temporary, &target)?;
    for path in stale.into_iter().filter(|p| *p != target) {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

/// Remove the file `name` was read from in `scope`.
pub(crate) fn delete(root: Option<&Path>, user: &Path, name: &str, scope: AgentScope) -> crate::Result<()> {
    check_name(name)?;
    let root = match (scope, root) {
        (AgentScope::Builtin, _) => return Err(crate::Error::Invalid("built-in agents can't be deleted".into())),
        (AgentScope::Project, None) => return Err(crate::Error::Invalid("project agents need cwd".into())),
        (_, root) => root.unwrap_or(user),
    };
    let path = find(root, user, scope, name).and_then(|a| a.path).ok_or_else(|| otto_store::Error::NotFound(format!("agent {name}")))?;
    Ok(std::fs::remove_file(path)?)
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
        let agent = super::parse(
            std::path::Path::new("rev.md"),
            "---\ntools: Read, mcp__gh__*\nloadable: [WebFetch, mcp__x__*]\nmodel: inherit\npermissionMode: default\n---\nx",
            otto_proto::AgentScope::User,
        );
        assert_eq!(agent.tools, ["read", "mcp__gh__*"]);
        assert_eq!(agent.loadable, ["web_fetch", "mcp__x__*"]);
        assert!(agent.model.is_none());
        assert_eq!(agent.approval_mode, Some(otto_proto::ApprovalMode::AskOnEffects));
        assert!(super::parse(std::path::Path::new("p.md"), "---\npermissionMode: plan\n---\n", otto_proto::AgentScope::User).approval_mode.is_none());
    }

    #[test]
    fn rendered_frontmatter_reads_back_the_same_definition() {
        let def = otto_proto::AgentDef {
            name: "rev-1".into(),
            description: "Reviews: \"the diff\"\nthen reports".into(),
            model: Some("anthropic/claude-sonnet-5-5".into()),
            effort: Some("high".into()),
            approval_mode: Some(otto_proto::ApprovalMode::AskAll),
            prompt: "Review.\n\n- one\n---\ntwo".into(),
            tools: vec!["read".into(), "mcp__gh__*".into()],
            loadable: vec!["web_search".into()],
            disallowed: vec!["shell".into()],
            scope: otto_proto::AgentScope::Project,
            overrides_builtin: false,
        };
        let back = super::parse(std::path::Path::new("x.md"), &super::render(&def), otto_proto::AgentScope::Project).def();
        assert_eq!(back, def);
        let empty = otto_proto::AgentDef {
            name: "e".into(),
            description: String::new(),
            model: None,
            effort: None,
            approval_mode: None,
            prompt: String::new(),
            tools: vec![],
            loadable: vec![],
            disallowed: vec![],
            ..def
        };
        assert_eq!(super::parse(std::path::Path::new("x.md"), &super::render(&empty), otto_proto::AgentScope::Project).def(), empty);
        for bad in ["", "a b", "../x", "x.md", &"a".repeat(65)] {
            assert!(super::check_name(bad).is_err(), "{bad}");
        }
    }
}
