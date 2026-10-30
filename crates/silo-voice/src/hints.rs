//! Keyword hints for engines that favour words: names from the focused
//! block (a terminal folder's files and git branch, an agent's project,
//! open files), a few at most.

use std::path::Path;

/// At most this many hints.
pub const MAX: usize = 24;
const MAX_LEN: usize = 32;
/// Folder entries read at most.
const MAX_ENTRIES: usize = 200;

/// Where hints come from, most telling first.
#[derive(Debug, Default)]
pub struct Sources {
    /// The project's or folder's name.
    pub project: Option<String>,
    pub branch: Option<String>,
    /// File names: a folder's entries, open files.
    pub files: Vec<String>,
}

/// A name as a hint: its stem (`main.rs` → `main`), unless hidden, too
/// short or long, or only digits.
fn hint(name: &str) -> Option<String> {
    let name = name.trim();
    if name.starts_with('.') {
        return None;
    }
    let stem = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && ext.len() <= 5 => stem,
        _ => name,
    };
    let ok = (3..=MAX_LEN).contains(&stem.chars().count()) && !stem.chars().all(|c| c.is_ascii_digit() || c == '-' || c == '_');
    ok.then(|| stem.to_owned())
}

/// The hints of `sources`: deduplicated (any case), in order, at most [`MAX`].
#[must_use]
pub fn keywords(sources: &Sources) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let named = sources.project.iter().chain(&sources.branch).filter_map(|n| {
        let n = n.trim();
        (!n.is_empty() && n.chars().count() <= MAX_LEN).then(|| n.to_owned())
    });
    for word in named.chain(sources.files.iter().filter_map(|f| hint(f))) {
        if out.len() >= MAX {
            break;
        }
        if !out.iter().any(|w| w.eq_ignore_ascii_case(&word)) {
            out.push(word);
        }
    }
    out
}

/// The git branch checked out at `folder` or a folder above it.
#[must_use]
pub fn git_branch(folder: &Path) -> Option<String> {
    folder.ancestors().take(32).find_map(|dir| {
        let dot = dir.join(".git");
        let git = if dot.is_file() {
            // A worktree: `gitdir: <path>`.
            let text = std::fs::read_to_string(&dot).ok()?;
            dir.join(text.strip_prefix("gitdir:")?.trim())
        } else {
            dot
        };
        let head = std::fs::read_to_string(git.join("HEAD")).ok()?;
        Some(head.trim().strip_prefix("ref: refs/heads/").map(str::to_owned))
    })?
}

/// A local folder's sources: its name, branch and entries.
#[must_use]
pub fn folder(path: &Path) -> Sources {
    let mut files: Vec<String> = std::fs::read_dir(path)
        .map(|entries| entries.take(MAX_ENTRIES).filter_map(|e| e.ok()?.file_name().into_string().ok()).collect())
        .unwrap_or_default();
    files.sort();
    Sources { project: path.file_name().map(|n| n.to_string_lossy().into_owned()), branch: git_branch(path), files }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_become_short_distinct_hints() {
        let sources = Sources {
            project: Some("term".into()),
            branch: Some("otto".into()),
            files: [
                "Cargo.toml",
                "README.md",
                ".git",
                "target",
                "src",
                "2024",
                "settings_panel.rs",
                "TERM.md",
                "a-really-long-file-name-nobody-says.txt",
            ]
            .map(String::from)
            .to_vec(),
        };
        assert_eq!(keywords(&sources), ["term", "otto", "Cargo", "README", "target", "src", "settings_panel"]);
        let many = Sources { files: (0..100).map(|i| format!("file{i}.rs")).collect(), ..Sources::default() };
        assert_eq!(keywords(&many).len(), MAX);
        assert!(keywords(&Sources::default()).is_empty());
    }

    #[test]
    fn a_folder_names_its_branch_and_entries() {
        let root = std::env::temp_dir().join(format!("silo-voice-hints-{}", std::process::id()));
        let sub = root.join("crates");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/voice-typing\n").unwrap();
        std::fs::write(sub.join("whistle.rs"), "").unwrap();
        let found = folder(&sub);
        assert_eq!(found.project.as_deref(), Some("crates"));
        assert_eq!(found.branch.as_deref(), Some("voice-typing"));
        assert_eq!(found.files, ["whistle.rs"]);
        std::fs::write(root.join(".git/HEAD"), "0123abcd\n").unwrap();
        assert_eq!(git_branch(&sub), None, "a detached head has no branch");
        let _ = std::fs::remove_dir_all(root);
    }
}
