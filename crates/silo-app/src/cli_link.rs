//! `~/.local/bin/silo` → the CLI bundled in this Silo.app, when that
//! directory is already on the login shell's `PATH` or the link already
//! points into a Silo.app. Never edits shell files.

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
    let link = dir.join("silo");
    let existing = match std::fs::symlink_metadata(&link) {
        Err(_) => Existing::Missing,
        Ok(meta) if meta.file_type().is_symlink() => std::fs::read_link(&link).map_or(Existing::Other, Existing::Link),
        Ok(_) => Existing::Other,
    };
    let on_path = existing == Existing::Missing && dir.is_dir() && login_path().is_some_and(|path| on_path(&path, &dir));
    if !should_link(&existing, on_path, &target) {
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

fn should_link(existing: &Existing, on_path: bool, target: &Path) -> bool {
    match existing {
        Existing::Missing => on_path,
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

/// `PATH` as the user's login shell sets it; `None` if that takes over 2 s.
fn login_path() -> Option<String> {
    let shell = std::env::var_os("SHELL").filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/zsh".into());
    let mut child = std::process::Command::new(shell)
        .args(["-l", "-c", "printf '\\n%s' \"$PATH\""])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while child.try_wait().ok()?.is_none() {
        if std::time::Instant::now() >= deadline {
            let _ = std::process::Command::new("/bin/kill").arg(child.id().to_string()).status();
            let _ = child.wait();
            tracing::info!("cli link: the login shell is too slow; not linking");
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let out = child.wait_with_output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).lines().last().unwrap_or_default().to_owned())
}

fn on_path(path: &str, dir: &Path) -> bool {
    std::env::split_paths(path).any(|entry| entry == dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_only_onto_path_or_over_our_own() {
        let target = Path::new("/Applications/Silo.app/Contents/Resources/bin/silo");
        assert!(should_link(&Existing::Missing, true, target));
        assert!(!should_link(&Existing::Missing, false, target));
        assert!(should_link(&Existing::Link("/Users/bat/Downloads/Silo.app/Contents/MacOS/silo-server".into()), false, target));
        assert!(should_link(&Existing::Link("/Volumes/Silo/Silo.app/Contents/Resources/bin/silo".into()), false, target));
        assert!(!should_link(&Existing::Link("/Applications/Other.app/Contents/MacOS/tool".into()), true, target));
        assert!(!should_link(&Existing::Link(target.into()), true, target));
        assert!(!should_link(&Existing::Link("/opt/homebrew/bin/silo".into()), true, target));
        assert!(!should_link(&Existing::Other, true, target));
    }

    #[test]
    fn finds_dir_on_path() {
        let dir = Path::new("/Users/bat/.local/bin");
        assert!(on_path("/usr/bin:/Users/bat/.local/bin:/bin", dir));
        assert!(on_path("/Users/bat/.local/bin/", dir));
        assert!(!on_path("/usr/bin:/Users/bat/.local/bin2", dir));
        assert!(!on_path("", dir));
    }
}
