//! `BUILD`: a hash of every workspace source, so binaries built from the
//! same tree (app, CLI, server, menu bar) agree and any other build differs.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let root = root.canonicalize().expect("workspace root");
    let mut files = vec![root.join("Cargo.toml"), root.join("Cargo.lock"), root.join("GHOSTTY_REV")];
    let mut crates: Vec<PathBuf> = std::fs::read_dir(root.join("crates"))
        .expect("read crates")
        .map(|e| e.expect("crate entry").path())
        .filter(|p| p.join("Cargo.toml").is_file())
        .collect();
    crates.sort();
    println!("cargo:rerun-if-changed={}", root.join("crates").display());
    for krate in &crates {
        files.push(krate.join("Cargo.toml"));
        let src = krate.join("src");
        println!("cargo:rerun-if-changed={}", src.display());
        walk(&src, &mut files);
    }
    let mut hash = Sha256::new();
    for file in &files {
        println!("cargo:rerun-if-changed={}", file.display());
        let rel = file.strip_prefix(&root).expect("inside workspace");
        let bytes = std::fs::read(file).unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
        hash.update(rel.to_string_lossy().as_bytes());
        hash.update([0]);
        hash.update((bytes.len() as u64).to_le_bytes());
        hash.update(&bytes);
    }
    let digest = hash.finalize();
    let id: String = digest[..8].iter().map(|b| format!("{b:02x}")).collect();
    println!("cargo:rustc-env=SILO_BUILD_ID={id}");
}

/// Every file under `dir`, sorted, so the hash does not depend on readdir order.
fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut paths: Vec<PathBuf> = entries.map(|e| e.expect("dir entry").path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            walk(&path, out);
        } else {
            out.push(path);
        }
    }
}
