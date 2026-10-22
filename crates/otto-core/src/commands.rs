//! Slash commands: markdown templates, Claude Code compatible.
use crate::Result;
use std::path::{Path, PathBuf};
#[derive(Clone, Debug)]
pub struct Command {
    /// Typed as `/name`; files in subfolders are `folder:name`.
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
    pub path: PathBuf,
}
fn walk(dir: &Path, prefix: &str, out: &mut std::collections::BTreeMap<String, Command>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let stem = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        if path.is_dir() {
            walk(&path, &format!("{prefix}{stem}:"), out);
        } else if path.extension().is_some_and(|e| e == "md")
            && let Ok(text) = std::fs::read_to_string(&path)
        {
            let (mut f, body) = crate::agents::frontmatter(&text);
            let name = format!("{prefix}{stem}");
            let description = f
                .remove("description")
                .unwrap_or_else(|| body.lines().find(|l| !l.trim().is_empty()).unwrap_or_default().chars().take(100).collect());
            out.insert(name.clone(), Command { name, description, argument_hint: f.remove("argument-hint"), path });
        }
    }
}
/// User `commands/`, then project `.claude/commands/` and `.otto/commands/`;
/// later folders replace earlier commands of the same name.
pub fn discover(root: &Path, user: &Path) -> Vec<Command> {
    let mut commands = std::collections::BTreeMap::new();
    for dir in [user.join("commands"), root.join(".claude/commands"), root.join(".otto/commands")] {
        walk(&dir, "", &mut commands);
    }
    commands.into_values().collect()
}
/// Expand `/name arguments`: `$ARGUMENTS` is the whole argument string and
/// `$1`…`$9` its whitespace-separated words; a template without placeholders
/// gets the arguments appended. Unknown commands stay literal text.
pub(crate) fn expand(root: &Path, user: &Path, text: &str) -> Result<String> {
    let Some(rest) = text.strip_prefix('/') else { return Ok(text.into()) };
    let (name, args) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    let args = args.trim();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ':')) {
        return Ok(text.into());
    }
    let Some(command) = discover(root, user).into_iter().find(|c| c.name == name) else { return Ok(text.into()) };
    let template = crate::agents::frontmatter(&std::fs::read_to_string(&command.path)?).1;
    let words: Vec<&str> = args.split_whitespace().collect();
    let mut expanded = template.replace("$ARGUMENTS", args);
    for index in (1..=9).rev() {
        expanded = expanded.replace(&format!("${index}"), words.get(index - 1).copied().unwrap_or(""));
    }
    if expanded == template && !args.is_empty() {
        expanded.push_str(&format!("\n\nARGUMENTS: {args}"));
    }
    Ok(expanded.trim().to_owned())
}
#[cfg(test)]
mod tests {
    #[test]
    fn templates_and_namespaces() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".otto/commands/git")).unwrap();
        std::fs::write(dir.path().join(".otto/commands/review.md"), "---\ndescription: Review code\nargument-hint: <path>\n---\nReview $ARGUMENTS")
            .unwrap();
        std::fs::write(dir.path().join(".otto/commands/git/commit.md"), "Commit $1 as $2").unwrap();
        std::fs::write(dir.path().join(".otto/commands/plain.md"), "Plain").unwrap();
        let user = dir.path().join("user");
        assert_eq!(super::expand(dir.path(), &user, "/review src").unwrap(), "Review src");
        assert_eq!(super::expand(dir.path(), &user, "/git:commit a b").unwrap(), "Commit a as b");
        assert_eq!(super::expand(dir.path(), &user, "/plain x").unwrap(), "Plain\n\nARGUMENTS: x");
        assert_eq!(super::expand(dir.path(), &user, "/missing x").unwrap(), "/missing x");
        let listed = super::discover(dir.path(), &user);
        assert_eq!(listed.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["git:commit", "plain", "review"]);
        assert_eq!(listed[2].argument_hint.as_deref(), Some("<path>"));
    }
}
