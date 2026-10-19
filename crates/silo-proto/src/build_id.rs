//! The build id rule, shared by `build.rs` (which computes `BUILD`) and
//! this crate's tests.
//!
//! Hashed, relative to the workspace root:
//! - the root `Cargo.toml`, `Cargo.lock` and `GHOSTTY_REV`;
//! - for every workspace member (`crates/*` with a `Cargo.toml`) except
//!   [`EXCLUDED`]: its `Cargo.toml`, its `build.rs`, and every `*.rs`,
//!   `*.toml`, `*.c` and `*.h` under its `src/`.
//!
//! [`EXCLUDED`] are front ends and dev tools nothing on the wire depends
//! on. Everything the server, CLI and client library are built from is
//! included, so the app (which embeds silo-client and silo-proto) and the
//! server agree whenever they could disagree on the wire. A new crate is
//! included by default.
//!
//! Cross-platform: the file set depends only on names and extensions,
//! never on readdir order, untracked junk (`.DS_Store`, notes) or the
//! host; paths are hashed relative and `/`-joined. A Linux and a macOS
//! checkout of one commit therefore hash the same bytes (assuming no
//! line-ending conversion on checkout).

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Members that do not affect the protocol or the server.
pub const EXCLUDED: &[&str] = &["silo-app", "silo-menubar", "silo-render", "silo-latency", "silo-web"];

const SOURCE_EXTENSIONS: &[&str] = &["rs", "toml", "c", "h"];

/// Hashed files and the directories whose new files would join them.
pub struct Sources {
    pub files: Vec<PathBuf>,
    pub dirs: Vec<PathBuf>,
}

/// The files and watched directories under workspace `root`.
pub fn sources(root: &Path) -> std::io::Result<Sources> {
    let mut files = vec![root.join("Cargo.toml"), root.join("Cargo.lock"), root.join("GHOSTTY_REV")];
    let mut dirs = vec![root.join("crates")];
    for krate in sorted(&root.join("crates"))? {
        let name = krate.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        if !krate.join("Cargo.toml").is_file() || EXCLUDED.contains(&name) {
            continue;
        }
        files.push(krate.join("Cargo.toml"));
        if krate.join("build.rs").is_file() {
            files.push(krate.join("build.rs"));
        }
        walk(&krate.join("src"), &mut files, &mut dirs)?;
    }
    files.sort_by_key(|f| relative(root, f));
    Ok(Sources { files, dirs })
}

/// 16 hex chars of SHA-256 over each file's relative path and contents.
pub fn hash(root: &Path, files: &[PathBuf]) -> std::io::Result<String> {
    let mut hash = Sha256::new();
    for file in files {
        let bytes = std::fs::read(file)?;
        hash.update(relative(root, file).as_bytes());
        hash.update([0]);
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(&bytes);
    }
    Ok(hash.finalize()[..8].iter().map(|b| format!("{b:02x}")).collect())
}

/// `file` relative to `root`, `/`-separated.
fn relative(root: &Path, file: &Path) -> String {
    let rel = file.strip_prefix(root).unwrap_or(file);
    rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")
}

fn sorted(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut paths = std::fs::read_dir(dir)?.map(|e| e.map(|e| e.path())).collect::<Result<Vec<_>, _>>()?;
    paths.sort();
    Ok(paths)
}

fn walk(dir: &Path, files: &mut Vec<PathBuf>, dirs: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    dirs.push(dir.to_owned());
    for path in sorted(dir)? {
        if path.is_dir() {
            walk(&path, files, dirs)?;
        } else if path.extension().and_then(|e| e.to_str()).is_some_and(|e| SOURCE_EXTENSIONS.contains(&e)) {
            files.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn id(root: &Path) -> String {
        hash(root, &sources(root).unwrap().files).unwrap()
    }

    #[test]
    fn only_sources_of_included_members_count() {
        let root = std::env::temp_dir().join(format!("silo-build-id-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for (rel, text) in [
            ("Cargo.toml", "[workspace]"),
            ("Cargo.lock", "lock"),
            ("GHOSTTY_REV", "rev"),
            ("crates/x/Cargo.toml", "[package]"),
            ("crates/x/build.rs", "fn main() {}"),
            ("crates/x/src/lib.rs", "pub fn f() {}"),
            ("crates/x/src/a/adapter.c", "int x;"),
            ("crates/silo-app/Cargo.toml", "[package]"),
            ("crates/silo-app/src/main.rs", "fn main() {}"),
        ] {
            write(&root, rel, text);
        }
        let base = id(&root);
        assert_eq!(base.len(), 16);
        assert_eq!(id(&root), base, "deterministic");
        let watched = sources(&root).unwrap().dirs;
        assert!(watched.contains(&root.join("crates/x/src/a")) && !watched.contains(&root.join("crates/silo-app/src")));

        // Junk, excluded members and non-members leave it alone.
        write(&root, "crates/x/src/.DS_Store", "junk");
        write(&root, "crates/x/src/notes.md", "notes");
        write(&root, "crates/x/src/a/.DS_Store", "junk");
        write(&root, "crates/x/tests/t.rs", "#[test] fn t() {}");
        write(&root, "crates/silo-app/src/more.rs", "pub fn g() {}");
        write(&root, "crates/wip/src/lib.rs", "no manifest, not a member");
        assert_eq!(id(&root), base);

        // Real sources change it.
        for (rel, text) in [("crates/x/src/lib.rs", "pub fn f2() {}"), ("crates/x/src/a/adapter.h", "int y;"), ("GHOSTTY_REV", "rev2")]
        {
            write(&root, rel, text);
            let next = id(&root);
            assert_ne!(next, base, "{rel}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn paths_are_slash_joined() {
        let root = Path::new("/w");
        assert_eq!(relative(root, &root.join("crates").join("x").join("src").join("lib.rs")), "crates/x/src/lib.rs");
    }
}
