//! `SKILL.md` folders (Agent Skills format, as Claude Code reads them).
use std::path::{Path, PathBuf};
#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// The `SKILL.md` file; its folder holds the skill's other files.
    pub path: PathBuf,
    /// `allowed-tools`, informational: the session grant still applies.
    pub allowed_tools: Vec<String>,
    /// `disable-model-invocation: true`: listed for users, hidden from the model.
    pub user_only: bool,
}
/// User `skills/`, then project `.claude/skills/`, `.agents/skills/` and
/// `.otto/skills/`; later folders replace earlier skills of the same name.
pub fn discover(root: &Path, user: &Path) -> Vec<Skill> {
    let mut skills = std::collections::BTreeMap::new();
    for dir in [user.join("skills"), root.join(".claude/skills"), root.join(".agents/skills"), root.join(".otto/skills")] {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path().join("SKILL.md");
            if let Ok(text) = std::fs::read_to_string(&path) {
                let (mut f, _) = crate::agents::frontmatter(&text);
                let name = f.remove("name").unwrap_or_else(|| entry.file_name().to_string_lossy().into_owned());
                skills.insert(
                    name.clone(),
                    Skill {
                        name,
                        description: f.remove("description").unwrap_or_default(),
                        path,
                        allowed_tools: crate::agents::list(f.remove("allowed-tools")),
                        user_only: f.remove("disable-model-invocation").is_some_and(|v| v == "true"),
                    },
                );
            }
        }
    }
    skills.into_values().collect()
}
/// What the `skill` tool returns: the body without frontmatter, and where its files are.
pub(crate) fn load(skill: &Skill) -> std::io::Result<String> {
    let text = std::fs::read_to_string(&skill.path)?;
    let dir = skill.path.parent().unwrap_or(Path::new("."));
    Ok(format!("Skill {} (files in {}):\n\n{}", skill.name, dir.display(), crate::agents::frontmatter(&text).1.trim()))
}
