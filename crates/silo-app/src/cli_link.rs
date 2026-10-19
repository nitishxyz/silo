//! `<SILO_HOME>/bin/silo` → this build's runtime `silo`, and
//! `~/.local/bin/silo` → that stable path when `~/.local/bin` exists. Never
//! edits shell files, never replaces a file or a link to something else.

use std::path::{Path, PathBuf};

/// What is at the link path now.
#[derive(Debug, PartialEq, Eq)]
enum Existing {
    Missing,
    Link(PathBuf),
    Other,
}

/// Create or refresh both links; logs what it did.
pub fn refresh(silo_home: &Path, silo: &Path) {
    let stable = silo_home.join("bin/silo");
    if std::fs::read_link(&stable).ok().as_deref() != Some(silo) {
        let _ = std::fs::create_dir_all(silo_home.join("bin"));
        link(&stable, silo);
    }
    let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from) else { return };
    let dir = home.join(".local/bin");
    if !dir.is_dir() {
        return;
    }
    let user = dir.join("silo");
    let existing = match std::fs::symlink_metadata(&user) {
        Err(_) => Existing::Missing,
        Ok(meta) if meta.file_type().is_symlink() => std::fs::read_link(&user).map_or(Existing::Other, Existing::Link),
        Ok(_) => Existing::Other,
    };
    if should_link(&existing, &stable) {
        link(&user, &stable);
    }
}

/// Point `path` at `target` atomically.
fn link(path: &Path, target: &Path) {
    let tmp = path.with_file_name(format!(".silo-link-{}", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    match std::os::unix::fs::symlink(target, &tmp).and_then(|()| std::fs::rename(&tmp, path)) {
        Ok(()) => tracing::info!("cli link: {} -> {}", path.display(), target.display()),
        Err(error) => {
            let _ = std::fs::remove_file(&tmp);
            tracing::warn!("cli link {}: {error}", path.display());
        }
    }
}

/// Missing, or a link into a Silo.app (which an update would break).
fn should_link(existing: &Existing, target: &Path) -> bool {
    match existing {
        Existing::Missing => true,
        Existing::Link(to) => to != target && (to.ends_with("Contents/Resources/bin/silo") || to.ends_with("Contents/MacOS/silo-server")),
        Existing::Other => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_when_missing_or_into_a_bundle() {
        let target = Path::new("/Users/bat/Library/Application Support/silo/bin/silo");
        assert!(should_link(&Existing::Missing, target));
        assert!(should_link(&Existing::Link("/Applications/Silo.app/Contents/Resources/bin/silo".into()), target));
        assert!(should_link(&Existing::Link("/Users/bat/Downloads/Silo.app/Contents/MacOS/silo-server".into()), target));
        assert!(!should_link(&Existing::Link(target.into()), target));
        assert!(!should_link(&Existing::Link("/Applications/Other.app/Contents/MacOS/tool".into()), target));
        assert!(!should_link(&Existing::Link("/opt/homebrew/bin/silo".into()), target));
        assert!(!should_link(&Existing::Other, target));
    }

    #[test]
    fn stable_link_follows_the_runtime() {
        let home = std::env::temp_dir().join(format!("silo-cli-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let stable = home.join("bin/silo");
        std::fs::create_dir_all(home.join("bin")).unwrap();
        link(&stable, Path::new("/x/runtime/a/silo"));
        link(&stable, Path::new("/x/runtime/b/silo"));
        assert_eq!(std::fs::read_link(&stable).unwrap(), Path::new("/x/runtime/b/silo"));
        assert_eq!(std::fs::read_dir(home.join("bin")).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&home);
    }
}
