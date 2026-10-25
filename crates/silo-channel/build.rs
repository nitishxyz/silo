//! `SILO_CHANNEL` picks the build's channel: `release` only when set so
//! (scripts/bundle-macos.sh and .github/workflows/release.yml do); unset,
//! empty or `dev` is a dev build. Anything else fails the build, so a typo
//! can never quietly produce either channel.

fn main() {
    println!("cargo:rerun-if-env-changed=SILO_CHANNEL");
    let value = match std::env::var("SILO_CHANNEL") {
        Ok(value) => value,
        Err(std::env::VarError::NotPresent) => String::new(),
        Err(std::env::VarError::NotUnicode(value)) => panic!("SILO_CHANNEL is not UTF-8: {value:?}"),
    };
    let channel = match value.as_str() {
        "" | "dev" => "dev",
        "release" => "release",
        other => panic!("SILO_CHANNEL must be `release` or `dev` (or unset for dev), not {other:?}"),
    };
    println!("cargo:rustc-env=SILO_BUILD_CHANNEL={channel}");
}
