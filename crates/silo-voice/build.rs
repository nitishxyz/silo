//! Links Whistle's engine (Cactus Compute's needle, Apache-2.0) on Apple
//! silicon Macs: the prebuilt `libneedle.a` of its macos-arm64 folder, at a
//! pinned revision and checksum, fetched once into `target/needle/`. The
//! engine's C++ source is not published, so there is nothing to build.
//! Other targets build without it (only the Whisper engine).

use std::{env, fs, path::PathBuf, process::Command};

use sha2::{Digest as _, Sha256};

/// `Cactus-Compute/needle3` on Hugging Face.
const REV: &str = "d6c7d6a2e9655505e0f8e23339356dd1705900eb";
const SHA256: &str = "7d1368a4a42a745aba055f310913c3ed6d929dee98a89dcb955b9032b2335700";

fn sha256(path: &std::path::Path) -> Option<String> {
    let bytes = fs::read(path).ok()?;
    Some(Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect())
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=SILO_NEEDLE_LIB");
    println!("cargo:rustc-check-cfg=cfg(whistle)");
    if env::var("TARGET").as_deref() != Ok("aarch64-apple-darwin") {
        return;
    }
    // An explicit copy (offline builds); checked like a download.
    let lib = match env::var_os("SILO_NEEDLE_LIB") {
        Some(path) => PathBuf::from(path),
        None => {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
            let dir = root.join(format!("target/needle/{REV}/macos-arm64"));
            let lib = dir.join("libneedle.a");
            if sha256(&lib).as_deref() != Some(SHA256) {
                fs::create_dir_all(&dir).expect("create target/needle");
                let part = dir.join("libneedle.a.part");
                let url = format!("https://huggingface.co/Cactus-Compute/needle3/resolve/{REV}/macos-arm64/libneedle.a");
                let status = Command::new("curl")
                    .args(["-fsSL", "--retry", "3", "--proto", "=https", "--proto-redir", "=https", "-o"])
                    .arg(&part)
                    .arg(&url)
                    .status()
                    .expect("run curl");
                assert!(status.success(), "downloading {url} failed");
                fs::rename(&part, &lib).expect("move libneedle.a");
            }
            lib
        }
    };
    assert_eq!(sha256(&lib).as_deref(), Some(SHA256), "{} does not match the pinned libneedle.a", lib.display());
    println!("cargo:rerun-if-changed={}", lib.display());
    // A private name: ld64 prefers a dylib over an archive in one directory.
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    fs::copy(&lib, out.join("libsilo_needle.a")).expect("copy libneedle.a");
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=silo_needle");
    println!("cargo:rustc-link-lib=c++");
    println!("cargo:rustc-cfg=whistle");
}
