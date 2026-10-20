//! `BUILD`: a hash of the workspace sources that shape the protocol and
//! server (rule in `src/build_id.rs`), so binaries built from one tree
//! agree and any other build differs.

#[path = "src/build_id.rs"]
mod build_id;

use std::path::Path;

fn main() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().expect("workspace root");
    let sources = build_id::sources(&root).expect("list workspace sources");
    // Directories catch added and removed files; a stray file there reruns
    // this script but leaves the id unchanged.
    for path in sources.dirs.iter().chain(&sources.files) {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    let id = build_id::hash(&root, &sources.files).expect("hash workspace sources");
    println!("cargo:rustc-env=SILO_BUILD_ID={id}");
    // `VT`: terminal snapshots are libghostty's GHOSTSNP records, which
    // carry no compatibility guarantee across ghostty revisions.
    let rev_file = root.join("GHOSTTY_REV");
    println!("cargo:rerun-if-changed={}", rev_file.display());
    let rev = std::fs::read_to_string(&rev_file).expect("GHOSTTY_REV");
    println!("cargo:rustc-env=SILO_VT_FORMAT=ghostsnp1-{}", rev.trim());
}
