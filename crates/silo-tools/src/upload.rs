//! `browser_upload`: the agent's files, read here (beside the agent) and
//! sent with the request, since the app running the page may be on
//! another machine.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use serde_json::Value;
use silo_proto::control::{BrowserUploadFile, BrowserUploadParams};

/// `raw` as a path: absolute, `~/…`, or relative to `cwd`.
fn resolve(raw: &str, cwd: Option<&Path>, home: Option<&Path>) -> anyhow::Result<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("paths must be non-empty strings");
    }
    Ok(if raw == "~" || raw.starts_with("~/") {
        home.context("no home directory for ~")?.join(raw.trim_start_matches('~').trim_start_matches('/'))
    } else if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        cwd.context("path is relative and there is no working directory; give an absolute path")?.join(raw)
    })
}

/// The files `input.paths` names.
///
/// # Errors
/// No paths, a folder or missing file, or more than
/// [`BrowserUploadParams::MAX_BYTES`] in all.
pub async fn read(input: &Value, cwd: Option<&Path>, home: Option<&Path>) -> anyhow::Result<Vec<BrowserUploadFile>> {
    let raw = input["paths"].as_array().filter(|p| !p.is_empty()).context("paths must be a non-empty array of file paths")?;
    let mut files = Vec::with_capacity(raw.len());
    let mut total = 0usize;
    for item in raw {
        let path = resolve(item.as_str().context("paths must be strings")?, cwd, home)?;
        let meta = tokio::fs::metadata(&path).await.with_context(|| format!("{} does not exist", path.display()))?;
        if !meta.is_file() {
            bail!("{} is not a file", path.display());
        }
        total = total.saturating_add(usize::try_from(meta.len()).unwrap_or(usize::MAX));
        if total > BrowserUploadParams::MAX_BYTES {
            bail!("the files come to more than {} MiB, the most one upload takes", BrowserUploadParams::MAX_BYTES / (1024 * 1024));
        }
        let bytes = tokio::fs::read(&path).await.with_context(|| format!("read {}", path.display()))?;
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).context("the path has no file name")?;
        files.push(BrowserUploadFile { name, data_base64: data_encoding::BASE64.encode(&bytes) });
    }
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn files_are_read_from_where_the_agent_works() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), b"hi").unwrap();
        let files = read(&json!({"paths": ["a.txt"]}), Some(dir.path()), None).await.unwrap();
        assert_eq!((files[0].name.as_str(), files[0].data_base64.as_str()), ("a.txt", "aGk="));
        let home = read(&json!({"paths": ["~/a.txt"]}), None, Some(dir.path())).await.unwrap();
        assert_eq!(home[0].name, "a.txt");
        for (input, error) in [
            (json!({}), "non-empty array"),
            (json!({"paths": []}), "non-empty array"),
            (json!({"paths": ["missing.txt"]}), "does not exist"),
            (json!({"paths": ["."]}), "is not a file"),
            (json!({"paths": [1]}), "must be strings"),
        ] {
            let message = read(&input, Some(dir.path()), None).await.unwrap_err().to_string();
            assert!(message.contains(error), "{input}: {message}");
        }
        assert!(read(&json!({"paths": ["a.txt"]}), None, None).await.unwrap_err().to_string().contains("relative"));
        let big = dir.path().join("big.bin");
        std::fs::File::create(&big).unwrap().set_len(BrowserUploadParams::MAX_BYTES as u64 + 1).unwrap();
        assert!(read(&json!({"paths": ["big.bin"]}), Some(dir.path()), None).await.unwrap_err().to_string().contains("MiB"));
    }
}
