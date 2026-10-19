//! Minimal dev `.app` around this example binary, so CEF finds its framework and helpers.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use silo_web::layout::{FRAMEWORK, HELPER_SUFFIXES};

pub const NAME: &str = "SiloWebOffscreen";

fn plist(exe: &str, id: &str, helper: bool) -> String {
    let ui = if helper {
        "<key>LSUIElement</key><string>1</string>"
    } else {
        ""
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>{exe}</string>
<key>CFBundleIdentifier</key><string>{id}</string>
<key>CFBundleName</key><string>{exe}</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>0.1.0</string>
<key>NSHighResolutionCapable</key><true/>
{ui}
</dict></plist>
"#
    )
}

fn app(dir: &Path, exe_name: &str, id: &str, bin: &Path, helper: bool) -> Result<()> {
    let contents = dir.join(format!("{exe_name}.app/Contents"));
    fs::create_dir_all(contents.join("MacOS"))?;
    fs::write(contents.join("Info.plist"), plist(exe_name, id, helper))?;
    let target = contents.join("MacOS").join(exe_name);
    let _ = fs::remove_file(&target);
    fs::copy(bin, &target).with_context(|| format!("copy to {}", target.display()))?;
    Ok(())
}

/// Builds `<dir>/SiloWebOffscreen.app` (exe + helpers are copies of `bin`; the framework is a
/// symlink into `cef_dir`) and returns the main executable path.
pub fn assemble(dir: &Path, bin: &Path, cef_dir: &Path) -> Result<PathBuf> {
    app(dir, NAME, "dev.silo.web-offscreen", bin, false)?;
    let frameworks = dir.join(format!("{NAME}.app/Contents/Frameworks"));
    fs::create_dir_all(&frameworks)?;
    for suffix in HELPER_SUFFIXES {
        let id = format!(
            "dev.silo.web-offscreen.helper{}",
            suffix.to_lowercase().replace([' ', '(', ')'], "")
        );
        app(
            &frameworks,
            &format!("{NAME} Helper{suffix}"),
            &id,
            bin,
            true,
        )?;
    }
    let link = frameworks.join(FRAMEWORK);
    if fs::symlink_metadata(&link).is_err() {
        std::os::unix::fs::symlink(cef_dir.join(FRAMEWORK), &link)?;
    }
    Ok(dir.join(format!("{NAME}.app/Contents/MacOS/{NAME}")))
}
