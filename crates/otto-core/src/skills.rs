//! `SKILL.md` folders (Agent Skills format, as Claude Code reads them), and the
//! skills bundled in the binary.
use std::path::{Path, PathBuf};

/// Skills embedded in the binary (`skills/<name>/SKILL.md`), discovered first.
pub const BUNDLED: &[(&str, &str)] = &[
    ("studio-onboarding", include_str!("../skills/studio-onboarding/SKILL.md")),
    ("worldbuilding", include_str!("../skills/worldbuilding/SKILL.md")),
    ("character-design", include_str!("../skills/character-design/SKILL.md")),
    ("prop-design", include_str!("../skills/prop-design/SKILL.md")),
    ("location-design", include_str!("../skills/location-design/SKILL.md")),
    ("style-guide", include_str!("../skills/style-guide/SKILL.md")),
    ("image-prompting", include_str!("../skills/image-prompting/SKILL.md")),
    ("series-writing", include_str!("../skills/series-writing/SKILL.md")),
    ("shot-listing", include_str!("../skills/shot-listing/SKILL.md")),
    ("video-prompting", include_str!("../skills/video-prompting/SKILL.md")),
    ("acting-direction", include_str!("../skills/acting-direction/SKILL.md")),
    ("dialogue-scenes", include_str!("../skills/dialogue-scenes/SKILL.md")),
    ("action-choreography", include_str!("../skills/action-choreography/SKILL.md")),
    ("animation-direction", include_str!("../skills/animation-direction/SKILL.md")),
    ("continuity", include_str!("../skills/continuity/SKILL.md")),
    ("sound-design", include_str!("../skills/sound-design/SKILL.md")),
    ("assembly", include_str!("../skills/assembly/SKILL.md")),
];

/// Where a skill's `SKILL.md` comes from.
#[derive(Clone, Debug)]
pub enum Source {
    /// The `SKILL.md` file; its folder holds the skill's other files.
    File(PathBuf),
    /// Embedded in the binary; no other files.
    Bundled(&'static str),
}

#[derive(Clone, Debug)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub source: Source,
    /// `allowed-tools`, informational: the session grant still applies.
    pub allowed_tools: Vec<String>,
    /// `disable-model-invocation: true`: listed for users, hidden from the model.
    pub user_only: bool,
}

impl Skill {
    fn parse(fallback: &str, text: &str, source: Source) -> Self {
        let (mut f, _) = crate::agents::frontmatter(text);
        Skill {
            name: f.remove("name").unwrap_or_else(|| fallback.to_owned()),
            description: f.remove("description").unwrap_or_default(),
            source,
            allowed_tools: crate::agents::list(f.remove("allowed-tools")),
            user_only: f.remove("disable-model-invocation").is_some_and(|v| v == "true"),
        }
    }
    /// The `SKILL.md` file; `None` for bundled skills.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        match &self.source {
            Source::File(path) => Some(path),
            Source::Bundled(_) => None,
        }
    }
}

/// Bundled skills, then user `skills/`, project `.claude/skills/`,
/// `.agents/skills/` and `.otto/skills/`; later ones replace earlier skills of the same name.
pub fn discover(root: &Path, user: &Path) -> Vec<Skill> {
    let mut skills: std::collections::BTreeMap<_, _> =
        BUNDLED.iter().map(|(name, text)| Skill::parse(name, text, Source::Bundled(text))).map(|s| (s.name.clone(), s)).collect();
    for dir in [user.join("skills"), root.join(".claude/skills"), root.join(".agents/skills"), root.join(".otto/skills")] {
        let Ok(entries) = std::fs::read_dir(dir) else { continue };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path().join("SKILL.md");
            if let Ok(text) = std::fs::read_to_string(&path) {
                let skill = Skill::parse(&entry.file_name().to_string_lossy(), &text, Source::File(path));
                skills.insert(skill.name.clone(), skill);
            }
        }
    }
    skills.into_values().collect()
}

/// What the `skill` tool returns: the body without frontmatter, and where its files are.
pub(crate) fn load(skill: &Skill) -> std::io::Result<String> {
    Ok(match &skill.source {
        Source::File(path) => {
            let text = std::fs::read_to_string(path)?;
            let dir = path.parent().unwrap_or(Path::new("."));
            format!("Skill {} (files in {}):\n\n{}", skill.name, dir.display(), crate::agents::frontmatter(&text).1.trim())
        }
        Source::Bundled(text) => format!("Skill {} (bundled):\n\n{}", skill.name, crate::agents::frontmatter(text).1.trim()),
    })
}
