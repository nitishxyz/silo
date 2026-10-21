use std::{collections::BTreeMap, path::Path};
#[derive(Clone, Debug, Default)]
pub struct Agent {
    pub name: String,
    pub description: String,
    pub model: Option<String>,
    pub tools: Vec<String>,
    pub effort: Option<String>,
    pub prompt: String,
}
pub(crate) fn frontmatter(text: &str) -> (BTreeMap<String, String>, String) {
    let mut fields = BTreeMap::new();
    let mut lines = text.lines();
    if lines.next() != Some("---") {
        return (fields, text.into());
    }
    for line in lines.by_ref() {
        if line == "---" {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            fields.insert(key.trim().into(), value.trim().trim_matches('"').trim_matches('\'').into());
        }
    }
    (fields, lines.collect::<Vec<_>>().join("\n"))
}
pub fn discover(root: &Path, user: &Path) -> Vec<Agent> {
    let mut agents = BTreeMap::new();
    for name in ["build", "plan", "general", "research"] {
        agents.insert(
            name.into(),
            Agent {
                name: name.into(),
                description: format!("{name} assistant"),
                prompt: if name == "plan" {
                    "Inspect and plan. Do not modify files or run processes.".into()
                } else {
                    "Complete the user's task accurately and verify the result.".into()
                },
                ..Default::default()
            },
        );
    }
    for dir in [user.join("agents"), root.join(".otto/agents")] {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "md")
                    && let Ok(text) = std::fs::read_to_string(&path)
                {
                    let (mut f, prompt) = frontmatter(&text);
                    let name = f.remove("name").unwrap_or_else(|| path.file_stem().unwrap_or_default().to_string_lossy().into_owned());
                    agents.insert(
                        name.clone(),
                        Agent {
                            name,
                            description: f.remove("description").unwrap_or_default(),
                            model: f.remove("model"),
                            tools: f
                                .remove("tools")
                                .unwrap_or_default()
                                .trim_matches(['[', ']'])
                                .split(',')
                                .map(str::trim)
                                .filter(|s| !s.is_empty())
                                .map(String::from)
                                .collect(),
                            effort: f.remove("effort"),
                            prompt,
                        },
                    );
                }
            }
        }
    }
    agents.into_values().collect()
}
