//! `~/.local/bin/silo` → the CLI bundled in this Silo.app, when
//! `~/.local/bin` exists. Never edits shell files, never replaces a file or
//! a link to something else.

use std::path::{Path, PathBuf};

/// What is at the link path now.
#[derive(Debug, PartialEq, Eq)]
enum Existing {
    Missing,
    Link(PathBuf),
    Other,
}

/// Create or refresh the link; logs what it did.
pub fn refresh() {
    let Some(target) = bundled_cli() else { return };
    let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from) else { return };
    let dir = home.join(".local/bin");
    if !dir.is_dir() {
        return;
    }
    let link = dir.join("silo");
    let existing = match std::fs::symlink_metadata(&link) {
        Err(_) => Existing::Missing,
        Ok(meta) if meta.file_type().is_symlink() => std::fs::read_link(&link).map_or(Existing::Other, Existing::Link),
        Ok(_) => Existing::Other,
    };
    if !should_link(&existing, &target) {
        return;
    }
    let tmp = dir.join(format!(".silo-link-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    match std::os::unix::fs::symlink(&target, &tmp).and_then(|()| std::fs::rename(&tmp, &link)) {
        Ok(()) => tracing::info!("cli link: {} -> {}", link.display(), target.display()),
        Err(error) => {
            let _ = std::fs::remove_file(&tmp);
            tracing::warn!("cli link {}: {error}", link.display());
        }
    }
}

fn should_link(existing: &Existing, target: &Path) -> bool {
    match existing {
        Existing::Missing => true,
        Existing::Link(to) => to != target && (to.ends_with("Contents/Resources/bin/silo") || to.ends_with("Contents/MacOS/silo-server")),
        Existing::Other => false,
    }
}

/// `Contents/Resources/bin/silo` of the running bundle, else its
/// `Contents/MacOS/silo-server`; `None` outside a bundle.
fn bundled_cli() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?.canonicalize().ok()?;
    let contents = exe.parent().filter(|d| d.ends_with("Contents/MacOS"))?.parent()?;
    let named = contents.join("Resources/bin/silo");
    if named.exists() {
        return Some(named);
    }
    Some(contents.join("MacOS/silo-server")).filter(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_when_missing_or_over_our_own() {
        let target = Path::new("/Applications/Silo.app/Contents/Resources/bin/silo");
        assert!(should_link(&Existing::Missing, target));
        assert!(should_link(&Existing::Link("/Users/bat/Downloads/Silo.app/Contents/MacOS/silo-server".into()), target));
        assert!(should_link(&Existing::Link("/Volumes/Silo/Silo.app/Contents/Resources/bin/silo".into()), target));
        assert!(!should_link(&Existing::Link("/Applications/Other.app/Contents/MacOS/tool".into()), target));
        assert!(!should_link(&Existing::Link(target.into()), target));
        assert!(!should_link(&Existing::Link("/opt/homebrew/bin/silo".into()), target));
        assert!(!should_link(&Existing::Other, target));
    }
}
