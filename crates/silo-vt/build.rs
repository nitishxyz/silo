use std::{env, fs, path::PathBuf, process::Command};

fn main() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest.join("../..");
    let rev_file = root.join("GHOSTTY_REV");
    println!("cargo:rerun-if-changed={}", rev_file.display());
    println!("cargo:rerun-if-env-changed=SILO_GHOSTTY_RUNTIME");
    println!("cargo:rerun-if-changed=src/adapter.c");
    println!("cargo:rerun-if-changed=src/adapter.h");
    let revision = fs::read_to_string(&rev_file).expect("GHOSTTY_REV");
    let revision = revision.trim();

    // Cross builds use a per-triple runtime that scripts/build-ghostty.sh
    // cross-compiles with zig and tags with its triple.
    let target = env::var("TARGET").expect("TARGET");
    let cross = (target != env::var("HOST").expect("HOST")).then_some(target.as_str());
    let runtime = env::var_os("SILO_GHOSTTY_RUNTIME").map_or_else(
        || {
            let dir = cross.map_or_else(|| "runtime".to_owned(), |t| format!("runtime-{t}"));
            root.join(format!("target/ghostty/{revision}/{dir}"))
        },
        PathBuf::from,
    );
    let stamp = runtime.join("silo-revision");
    if fs::read_to_string(&stamp).unwrap_or_default().trim() != revision {
        let status = Command::new("bash")
            .arg(root.join("scripts/build-ghostty.sh"))
            .args(cross)
            .status()
            .expect("run scripts/build-ghostty.sh");
        assert!(status.success(), "building libghostty-vt failed");
    }
    assert_eq!(
        fs::read_to_string(&stamp).unwrap_or_default().trim(),
        revision,
        "libghostty-vt runtime missing; run scripts/build-ghostty.sh"
    );
    if let Some(target) = cross {
        let built_for = fs::read_to_string(runtime.join("silo-target")).unwrap_or_default();
        assert_eq!(
            built_for.trim(),
            target,
            "libghostty-vt runtime {} is not built for {target}; unset SILO_GHOSTTY_RUNTIME or run scripts/build-ghostty.sh {target}",
            runtime.display()
        );
    }
    println!("cargo:rerun-if-changed={}", stamp.display());

    cc::Build::new()
        .file("src/adapter.c")
        .include(runtime.join("include"))
        .define("GHOSTTY_STATIC", None)
        .flag_if_supported("-std=c11")
        .warnings(true)
        .compile("silo_vt_adapter");
    // ld64 prefers a dylib over an archive with the same name in one search
    // directory, so link a private copy of the archive.
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    fs::copy(
        runtime.join("lib/libghostty-vt.a"),
        out.join("libsilo_ghostty_vt.a"),
    )
    .expect("copy libghostty-vt.a");
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=silo_ghostty_vt");
    println!("cargo:include={}", runtime.join("include").display());
    println!("cargo:rustc-env=SILO_GHOSTTY_REV={revision}");
}
