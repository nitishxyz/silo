//! `BUILD`: a hash of the workspace sources that shape the protocol and
//! server (rule in `src/build_id.rs`), so binaries built from one tree
//! agree and any other build differs.

#[path = "src/build_id.rs"]
mod build_id;

use std::path::Path;

fn main() {
    // Read at run time, not with env!(): a compiled-in path would tie the
    // build script to one checkout (and corgi refuses such artifacts).
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let root = Path::new(&manifest).join("../..").canonicalize().expect("workspace root");
    let sources = build_id::sources(&root).expect("list workspace sources");
    // Directories catch added and removed files; a stray file there reruns
    // this script but leaves the id unchanged.
    for path in sources.dirs.iter().chain(&sources.files) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    // corgi's sandbox hides Cargo.lock from build scripts; corgi.toml hands
    // over its digest instead. Unset under cargo, which reads the file.
    println!("cargo:rerun-if-env-changed=SILO_CARGO_LOCK_SHA256");
    let lock = std::env::var("SILO_CARGO_LOCK_SHA256").ok();
    let id = build_id::hash(&root, &sources.files, lock.as_deref()).expect("hash workspace sources");
    println!("cargo:rustc-env=SILO_BUILD_ID={id}");
    // `VT`: terminal snapshots are libghostty's GHOSTSNP records, which
    // carry no compatibility guarantee across ghostty revisions.
    let rev_file = root.join("GHOSTTY_REV");
    println!("cargo:rerun-if-changed={}", rev_file.display());
    let rev = std::fs::read_to_string(&rev_file).expect("GHOSTTY_REV");
    println!("cargo:rustc-env=SILO_VT_FORMAT=ghostsnp1-{}", rev.trim());
}
