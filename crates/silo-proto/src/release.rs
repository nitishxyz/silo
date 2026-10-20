//! Fetching published release files with `curl`, as install.sh does;
//! shared by the server's remote upgrade and the app's updater. Where the
//! files are and how they are checked is in [`crate::upgrade`].

use std::{path::Path, time::Duration};

use sha2::{Digest, Sha256};

/// Longest download of one release file.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// Why a download failed: a refusal code and a reason for the user.
pub type FetchError = (&'static str, String);

/// Download `url` to `dest` (at most `max` bytes; over https only when
/// `https`, redirects included); `None` when the server has no such file.
///
/// # Errors
/// `curl` is missing or failed, the file is larger than `max`, or the
/// download took longer than [`DOWNLOAD_TIMEOUT`].
pub async fn fetch(url: &str, dest: &Path, https: bool, max: u64) -> Result<Option<()>, FetchError> {
    tracing::info!(url, "release: downloading");
    let mut curl = tokio::process::Command::new("curl");
    curl.args(["-fsSL", "--retry", "3", "--max-filesize", &max.to_string()]);
    if https {
        curl.args(["--proto", "=https", "--proto-redir", "=https"]);
    }
    let run = curl.arg("-o").arg(dest).arg(url).stdin(std::process::Stdio::null()).kill_on_drop(true).output();
    let out = match tokio::time::timeout(DOWNLOAD_TIMEOUT, run).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => return Err(("download", "the host needs curl to download releases".into())),
        Ok(Err(e)) => return Err(("download", format!("run curl: {e}"))),
        Err(_) => return Err(("download", format!("downloading {url} timed out"))),
    };
    match out.status.code() {
        Some(0) => Ok(Some(())),
        // HTTP error (404 for a missing asset or release).
        Some(22) => Ok(None),
        Some(63) => Err(("too_large", format!("{url} is larger than {max} bytes"))),
        _ => Err(("download", format!("download {url}: {}", String::from_utf8_lossy(&out.stderr).trim()))),
    }
}

/// `path`'s lowercase hex SHA-256, as in `sha256sum` output.
///
/// # Errors
/// `path` cannot be read.
pub fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut hash = Sha256::new();
    std::io::copy(&mut std::fs::File::open(path)?, &mut hash)?;
    Ok(hash.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_like_sha256sum() {
        let path = std::env::temp_dir().join(format!("silo-release-sha-{}", std::process::id()));
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(sha256_file(&path).unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        std::fs::remove_file(path).unwrap();
    }
}
