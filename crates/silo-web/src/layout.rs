//! macOS bundle layout CEF runs from.
//!
//! ```text
//! <Name>.app/Contents/MacOS/<exe>
//! <Name>.app/Contents/Frameworks/Chromium Embedded Framework.framework
//! <Name>.app/Contents/Frameworks/<Name> Helper.app/Contents/MacOS/<Name> Helper
//! <Name>.app/Contents/Frameworks/<Name> Helper (GPU|Renderer|Plugin).app/…
//! ```

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

pub const FRAMEWORK: &str = "Chromium Embedded Framework.framework";
/// Helper bundle suffixes Chromium appends to the base helper name.
pub const HELPER_SUFFIXES: [&str; 4] = ["", " (GPU)", " (Renderer)", " (Plugin)"];

#[derive(Clone, Debug)]
pub struct Layout {
    pub app: PathBuf,
    pub frameworks: PathBuf,
    pub helper: PathBuf,
}

impl Layout {
    /// Layout of the bundle containing the main executable `exe`.
    pub fn for_main(exe: &Path) -> Result<Layout> {
        let macos = exe.parent().context("exe has no parent")?;
        let contents = macos.parent().context("exe not inside Contents/MacOS")?;
        let app = contents.parent().context("exe not inside a .app")?;
        if macos.file_name() != Some("MacOS".as_ref()) || app.extension() != Some("app".as_ref()) {
            bail!("{} is not inside <Name>.app/Contents/MacOS (CEF needs the bundle layout)", exe.display());
        }
        let name = app.file_stem().context("bundle name")?.to_string_lossy().into_owned();
        let frameworks = contents.join("Frameworks");
        let helper_name = format!("{name} Helper");
        let helper = frameworks.join(format!("{helper_name}.app/Contents/MacOS/{helper_name}"));
        Ok(Layout { app: app.to_path_buf(), frameworks, helper })
    }

    pub fn framework(&self) -> PathBuf {
        self.frameworks.join(FRAMEWORK)
    }
}
