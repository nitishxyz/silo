//! Speech models: downloaded on first use into `<home>/voice/`, checked
//! against a pinned SHA-256, never bundled.

use std::path::{Path, PathBuf};

use crate::prefs::EngineKind;

#[derive(Debug)]
pub struct Model {
    pub engine: EngineKind,
    pub file: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub bytes: u64,
}

/// Whistle (Cactus Compute, Apache-2.0), at a pinned revision.
pub const WHISTLE: Model = Model {
    engine: EngineKind::Fast,
    file: "whistle.cact",
    url: "https://huggingface.co/Cactus-Compute/whistle/resolve/741118687e1f008f06d1351a402a731274789790/whistle.cact",
    sha256: "b6e02f048568ac5d01a2042556c658061e699acbc0aa2a1439f52f3d461dffeb",
    bytes: 16_919_407,
};

/// Whisper large-v3 turbo (OpenAI, MIT), 5-bit, in whisper.cpp's format.
pub const WHISPER_TURBO: Model = Model {
    engine: EngineKind::Multilingual,
    file: "ggml-large-v3-turbo-q5_0.bin",
    url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-large-v3-turbo-q5_0.bin",
    sha256: "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2",
    bytes: 574_041_195,
};

/// Where models live under the app's home.
#[must_use]
pub fn dir(home: &Path) -> PathBuf {
    home.join("voice")
}

impl Model {
    #[must_use]
    pub fn path(&self, home: &Path) -> PathBuf {
        dir(home).join(self.file)
    }

    /// Downloaded and complete (checked when it was downloaded).
    #[must_use]
    pub fn installed(&self, home: &Path) -> bool {
        std::fs::metadata(self.path(home)).is_ok_and(|m| m.len() == self.bytes)
    }

    /// "17 MB", "574 MB".
    #[must_use]
    pub fn size(&self) -> String {
        format!("{} MB", (self.bytes + 500_000) / 1_000_000)
    }

    /// Download it into `home`, reporting bytes done and the total.
    ///
    /// # Errors
    /// Offline, refused, or not matching its checksum (said for the user).
    pub async fn download(&self, home: &Path, progress: impl Fn(u64, Option<u64>) + Send + Sync) -> Result<(), String> {
        let dest = self.path(home);
        std::fs::create_dir_all(dir(home)).map_err(|e| format!("Couldn't create the voice folder: {e}"))?;
        let part = dest.with_extension("part");
        let fetched = silo_proto::release::fetch_with_progress(self.url, &part, true, self.bytes + 1024, Some(progress)).await;
        let checked = match fetched {
            Ok(Some(())) => match silo_proto::release::sha256_file(&part) {
                Ok(sum) if sum == self.sha256 => std::fs::rename(&part, &dest).map_err(|e| format!("Couldn't save the model: {e}")),
                Ok(_) => Err("The download was damaged; try again.".to_owned()),
                Err(e) => Err(format!("Couldn't check the model: {e}")),
            },
            Ok(None) => Err("The model is no longer published.".to_owned()),
            Err((_, error)) => {
                tracing::warn!(model = self.file, "voice model download: {error}");
                Err("Couldn't download the model; check your connection.".to_owned())
            }
        };
        let _ = std::fs::remove_file(&part);
        checked
    }

    /// Remove it.
    ///
    /// # Errors
    /// The file exists but can't be removed.
    pub fn delete(&self, home: &Path) -> std::io::Result<()> {
        match std::fs::remove_file(self.path(home)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_are_pinned_and_sized() {
        for model in [&WHISTLE, &WHISPER_TURBO] {
            assert_eq!(model.sha256.len(), 64);
            assert!(model.url.starts_with("https://huggingface.co/") && model.url.ends_with(model.file));
            assert!(!model.url.contains("/main/"), "a pinned revision");
            assert_eq!(model.engine.model().file, model.file);
        }
        assert_eq!((WHISTLE.size(), WHISPER_TURBO.size()), ("17 MB".to_owned(), "574 MB".to_owned()));
    }

    #[test]
    fn installed_means_complete() {
        let home = std::env::temp_dir().join(format!("silo-voice-models-{}", std::process::id()));
        assert!(!WHISTLE.installed(&home));
        std::fs::create_dir_all(dir(&home)).unwrap();
        std::fs::write(WHISTLE.path(&home), b"partial").unwrap();
        assert!(!WHISTLE.installed(&home), "a short file is not installed");
        WHISTLE.delete(&home).unwrap();
        WHISTLE.delete(&home).unwrap();
        assert!(!WHISTLE.path(&home).exists());
        let _ = std::fs::remove_dir_all(home);
    }
}
