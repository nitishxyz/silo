use std::path::{Path, PathBuf};
#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}
pub fn discover(root: &Path, user: &Path) -> Vec<Skill> {
    let mut skills = std::collections::BTreeMap::new();
    for dir in [user.join("skills"), root.join(".agents/skills"), root.join(".otto/skills")] {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path().join("SKILL.md");
                if let Ok(text) = std::fs::read_to_string(&path) {
                    let (mut f, _) = crate::agents::frontmatter(&text);
                    let name = f.remove("name").unwrap_or_else(|| entry.file_name().to_string_lossy().into_owned());
                    skills.insert(name.clone(), Skill { name, description: f.remove("description").unwrap_or_default(), path });
                }
            }
        }
    }
    skills.into_values().collect()
}
