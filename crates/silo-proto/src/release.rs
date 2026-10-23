//! Fetching published release files with `curl`, as install.sh does;
//! shared by the server's remote upgrade and the app's updater. Where the
//! files are and how they are checked is in [`crate::upgrade`].

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use crate::upgrade;

/// Longest download of one release file.
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
/// Longest request for a file's size before a download with progress.
const SIZE_TIMEOUT: Duration = Duration::from_secs(15);
/// How often a download's progress is reported.
pub const PROGRESS_POLL: Duration = Duration::from_millis(250);

/// Longest `<binary> --version`.
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
/// Most `--version` output read.
const VERSION_MAX: u64 = 4096;

/// Why a download failed: a refusal code and a reason for the user.
pub type FetchError = (&'static str, String);

/// Download `url` to `dest` (at most `max` bytes; over https only when
/// `https`, redirects included); `None` when the server has no such file.
///
/// # Errors
/// `curl` is missing or failed, the file is larger than `max`, or the
/// download took longer than [`DOWNLOAD_TIMEOUT`].
pub async fn fetch(url: &str, dest: &Path, https: bool, max: u64) -> Result<Option<()>, FetchError> {
    fetch_with_progress(url, dest, https, max, None::<fn(u64, Option<u64>)>).await
}

/// [`fetch`], calling `on_progress` with the bytes downloaded and the
/// file's size (when the server tells it, asked first) about every
/// [`PROGRESS_POLL`].
///
/// # Errors
/// As [`fetch`].
pub async fn fetch_with_progress(
    url: &str,
    dest: &Path,
    https: bool,
    max: u64,
    on_progress: Option<impl Fn(u64, Option<u64>) + Send + Sync>,
) -> Result<Option<()>, FetchError> {
    let total = match on_progress {
        Some(_) => size_of(url, https).await,
        None => None,
    };
    tracing::info!(url, "release: downloading");
    let mut curl = tokio::process::Command::new("curl");
    curl.args(["-fsSL", "--retry", "3", "--max-filesize", &max.to_string()]);
    if https {
        curl.args(["--proto", "=https", "--proto-redir", "=https"]);
    }
    let run =
        tokio::time::timeout(DOWNLOAD_TIMEOUT, curl.arg("-o").arg(dest).arg(url).stdin(std::process::Stdio::null()).kill_on_drop(true).output());
    let report = |done: u64| {
        if let Some(f) = &on_progress {
            f(done, total);
        }
    };
    let run = async {
        if on_progress.is_none() {
            return run.await;
        }
        poll_size(dest, &report, run).await
    };
    let out = match run.await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => return Err(("download", "the host needs curl to download releases".into())),
        Ok(Err(e)) => return Err(("download", format!("run curl: {e}"))),
        Err(_) => return Err(("download", format!("downloading {url} timed out"))),
    };
    match out.status.code() {
        Some(0) => {
            report(std::fs::metadata(dest).map_or(0, |m| m.len()));
            Ok(Some(()))
        }
        // HTTP error (404 for a missing asset or release).
        Some(22) => Ok(None),
        Some(63) => Err(("too_large", format!("{url} is larger than {max} bytes"))),
        _ => Err(("download", format!("download {url}: {}", String::from_utf8_lossy(&out.stderr).trim()))),
    }
}

/// Run `work`, calling `report` with `path`'s size every [`PROGRESS_POLL`]
/// (0 while it doesn't exist).
pub async fn poll_size<T>(path: &Path, report: impl Fn(u64), work: impl Future<Output = T>) -> T {
    let mut tick = tokio::time::interval(PROGRESS_POLL);
    tokio::pin!(work);
    loop {
        tokio::select! {
            out = &mut work => return out,
            _ = tick.tick() => report(std::fs::metadata(path).map_or(0, |m| m.len())),
        }
    }
}

/// The size of `url`'s file from a HEAD request (redirects followed), if
/// the server tells it.
async fn size_of(url: &str, https: bool) -> Option<u64> {
    let mut curl = tokio::process::Command::new("curl");
    curl.args(["-sIL", "--max-time", &SIZE_TIMEOUT.as_secs().to_string()]);
    if https {
        curl.args(["--proto", "=https", "--proto-redir", "=https"]);
    }
    let run = curl.arg(url).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null()).kill_on_drop(true).output();
    let out = tokio::time::timeout(SIZE_TIMEOUT, run).await.ok()?.ok()?;
    content_length(&String::from_utf8_lossy(&out.stdout))
}

/// The final response's `Content-Length` in `curl -I` output (one header
/// block per redirect; a `file://` URL's has no status line), if that
/// response succeeded and is not empty.
#[must_use]
pub fn content_length(headers: &str) -> Option<u64> {
    let (mut ok, mut length) = (true, None);
    for line in headers.lines() {
        if line.starts_with("HTTP/") {
            ok = line.split_whitespace().nth(1).is_some_and(|code| code.starts_with('2'));
            length = None;
        } else if let Some((name, value)) = line.split_once(':')
            && ok
            && name.trim().eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().ok();
        }
    }
    length.filter(|&n| n > 0)
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

/// Fetch release `version` for `platform` into `path` (mode 0755): its
/// asset, checked against the release's checksums, unpacked. Uses `curl`
/// and `tar` like install.sh; `base` (`SILO_DOWNLOAD_BASE`) overrides the
/// location.
///
/// # Errors
/// Not published (`upgrade::NOT_PUBLISHED`), not downloadable, not
/// matching the checksums, or not holding a plain `silo`.
pub async fn download(path: &Path, base: Option<&str>, version: &str, platform: &str) -> Result<(), FetchError> {
    let url = |file: &str| upgrade::release_url(base, version, file).ok_or_else(|| ("bad_request", format!("{version:?} is not a release version")));
    let (sums_url, asset) = (url(upgrade::CHECKSUMS)?, upgrade::asset(platform));
    let asset_url = url(&asset)?;
    // Only the default (GitHub) location must be https; an override may be
    // a mirror or a test server.
    let https = base.is_none_or(str::is_empty);
    let name = path.file_name().and_then(|n| n.to_str()).ok_or_else(|| ("internal", "bad staging path".to_owned()))?;
    let unpack = path.with_file_name(format!("{name}.d"));
    std::fs::create_dir(&unpack).map_err(|e| ("internal", e.to_string()))?;
    let _unpack = TempDir(unpack.clone());
    let not_published = || (upgrade::NOT_PUBLISHED, upgrade::not_published(version, platform));
    let sums = unpack.join(upgrade::CHECKSUMS);
    fetch(&sums_url, &sums, https, upgrade::MAX_BINARY).await?.ok_or_else(not_published)?;
    let sums = std::fs::read_to_string(&sums).map_err(|e| ("internal", e.to_string()))?;
    let expected = upgrade::checksum(&sums, &asset).ok_or_else(not_published)?.to_ascii_lowercase();
    let archive = unpack.join(&asset);
    fetch(&asset_url, &archive, https, upgrade::MAX_BINARY).await?.ok_or_else(not_published)?;
    let actual = sha256_file(&archive).map_err(|e| ("internal", e.to_string()))?;
    if actual != expected {
        return Err(("checksum", format!("{asset} does not match the release's {}", upgrade::CHECKSUMS)));
    }
    let tar = tokio::process::Command::new("tar")
        .arg("-xzf")
        .arg(&archive)
        .arg("-C")
        .arg(&unpack)
        .arg("silo")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| ("download", format!("run tar: {e}")))?;
    if !tar.status.success() {
        return Err(("download", format!("unpack {asset}: {}", String::from_utf8_lossy(&tar.stderr).trim())));
    }
    let member = unpack.join("silo");
    // A plain file of its own: a symlink or hard link member could point
    // anywhere (set_permissions and rename would act on its target).
    let plain = std::fs::symlink_metadata(&member).is_ok_and(|m| m.file_type().is_file() && std::os::unix::fs::MetadataExt::nlink(&m) == 1);
    if !plain {
        return Err(("invalid", format!("{asset} does not hold a plain `silo` file")));
    }
    std::fs::rename(&member, path).map_err(|e| ("internal", e.to_string()))?;
    std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).map_err(|e| ("internal", e.to_string()))
}

/// Removes a directory tree on drop.
struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Version and build reported by `path --version`: proves it runs here.
/// Runs in its own process group, killed whole if it overstays; at most
/// [`VERSION_MAX`] bytes of output are read.
///
/// # Errors
/// It does not run, does not finish, or reports no silo version and build.
pub async fn version_of(path: &Path) -> Result<(String, String), FetchError> {
    let not_silo = || ("invalid", "the binary is not a silo for this host's platform".to_owned());
    let mut child = tokio::process::Command::new(path)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| not_silo())?;
    let group = child.id().and_then(|id| rustix::process::Pid::from_raw(id.cast_signed()));
    let stdout = child.stdout.take().expect("piped");
    let run = async {
        let mut out = Vec::new();
        stdout.take(VERSION_MAX).read_to_end(&mut out).await?;
        std::io::Result::Ok((child.wait().await?, out))
    };
    let (status, out) = match tokio::time::timeout(VERSION_TIMEOUT, run).await {
        Ok(Ok(done)) => done,
        Ok(Err(_)) => return Err(not_silo()),
        Err(_) => {
            if let Some(group) = group {
                let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
            }
            return Err(("invalid", "the binary's `--version` did not finish".into()));
        }
    };
    match upgrade::parse_version(&String::from_utf8_lossy(&out)) {
        Some((version, Some(build))) if status.success() => Ok((version, build)),
        Some((_, None)) => Err(("invalid", "the binary predates remote upgrade (it reports no build)".into())),
        _ => Err(not_silo()),
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt;

    use super::*;

    #[tokio::test]
    async fn version_probe() {
        let dir = std::env::temp_dir().join(format!("silo-upgrade-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let script = |name: &str, body: &str| {
            let path = dir.join(name);
            std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
            path
        };
        let ok = script("ok", "echo 'silo 1.2.3 (abc)'");
        assert_eq!(version_of(&ok).await.unwrap(), ("1.2.3".into(), "abc".into()));
        let old = script("old", "echo 'silo 1.2.3'");
        assert!(version_of(&old).await.unwrap_err().1.contains("predates"));
        let failing = script("failing", "echo 'silo 1.2.3 (abc)'; exit 1");
        assert_eq!(version_of(&failing).await.unwrap_err().0, "invalid");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A minimal HTTP/1.0 server for `files`; anything else is a 404.
    async fn serve_files(files: std::collections::HashMap<String, Vec<u8>>) -> String {
        use tokio::io::AsyncReadExt as _;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let files = files.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match socket.read(&mut buf).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => request.extend_from_slice(&buf[..n]),
                        }
                    }
                    let path = String::from_utf8_lossy(&request).split_whitespace().nth(1).unwrap_or("").to_owned();
                    let reply = match files.get(path.trim_start_matches("/r/")) {
                        Some(body) => [format!("HTTP/1.0 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes(), body.clone()].concat(),
                        None => b"HTTP/1.0 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
                    };
                    let _ = socket.write_all(&reply).await;
                });
            }
        });
        format!("http://{addr}/r")
    }

    #[tokio::test]
    async fn downloads_a_checked_release() {
        let dir = std::env::temp_dir().join(format!("silo-upgrade-download-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("pack")).unwrap();
        std::fs::write(dir.join("pack/silo"), "#!/bin/sh\necho 'silo 9.9.9 (rel)'\n").unwrap();
        let archive = dir.join("a.tar.gz");
        let packed = std::process::Command::new("tar").arg("-czf").arg(&archive).arg("-C").arg(dir.join("pack")).arg("silo").status().unwrap();
        assert!(packed.success());
        let tarball = std::fs::read(&archive).unwrap();
        let sha = |bytes: &[u8]| -> String { Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect() };
        // `silo` as a symlink to a file outside the unpack directory.
        std::fs::create_dir_all(dir.join("evil")).unwrap();
        std::os::unix::fs::symlink(dir.join("pack/silo"), dir.join("evil/silo")).unwrap();
        let evil_archive = dir.join("evil.tar.gz");
        let packed = std::process::Command::new("tar").arg("-czf").arg(&evil_archive).arg("-C").arg(dir.join("evil")).arg("silo").status().unwrap();
        assert!(packed.success());
        let evil = std::fs::read(&evil_archive).unwrap();
        let asset = upgrade::asset("linux-x86_64");
        let sums = format!("{}  {asset}\n{}  silo-macos-aarch64.tar.gz\n{}  silo-linux-riscv64.tar.gz\n", sha(&tarball), "0".repeat(64), sha(&evil));
        let files = std::collections::HashMap::from([
            (upgrade::CHECKSUMS.to_owned(), sums.into_bytes()),
            (asset.clone(), tarball),
            ("silo-macos-aarch64.tar.gz".to_owned(), b"corrupt".to_vec()),
            ("silo-linux-riscv64.tar.gz".to_owned(), evil),
        ]);
        let base = serve_files(files).await;
        let path = dir.join(".silo.upgrade.1.aa");
        download(&path, Some(&base), "9.9.9", "linux-x86_64").await.unwrap();
        assert_eq!(version_of(&path).await.unwrap(), ("9.9.9".into(), "rel".into()));
        assert_eq!(std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&path).unwrap().permissions()) & 0o777, 0o755);
        std::fs::remove_file(&path).unwrap();
        // Not in the checksums: not published.
        let error = download(&path, Some(&base), "9.9.9", "linux-aarch64").await.unwrap_err();
        assert_eq!(error, (upgrade::NOT_PUBLISHED, "silo 9.9.9 isn't published for linux-aarch64; use --local or --binary".into()));
        // A missing release (404 for SHA256SUMS): not published either.
        let error = download(&path, Some(&format!("{base}/nope")), "9.9.9", "linux-x86_64").await.unwrap_err();
        assert_eq!(error.0, upgrade::NOT_PUBLISHED);
        let error = download(&path, Some(&base), "9.9.9", "macos-aarch64").await.unwrap_err();
        assert_eq!(error.0, "checksum");
        // A symlink member is refused and its target left alone.
        let mode = |p: &Path| std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(p).unwrap().permissions()) & 0o777;
        let before = mode(&dir.join("pack/silo"));
        let error = download(&path, Some(&base), "9.9.9", "linux-riscv64").await.unwrap_err();
        assert_eq!(error.0, "invalid", "{error:?}");
        assert_eq!(mode(&dir.join("pack/silo")), before);
        // A version that is no release version never reaches a URL.
        let error = download(&path, None, "0.1.0/../../../../x/y/releases/download/v1", "linux-x86_64").await.unwrap_err();
        assert_eq!(error.0, "bad_request");
        assert!(!path.exists());
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left.len(), 4, "only the fixtures remain: {left:?}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn hashes_like_sha256sum() {
        let path = std::env::temp_dir().join(format!("silo-release-sha-{}", std::process::id()));
        std::fs::write(&path, b"abc").unwrap();
        assert_eq!(sha256_file(&path).unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn sizes_from_headers() {
        let github = "HTTP/2 302 \r\nlocation: https://x/y\r\ncontent-length: 0\r\n\r\nHTTP/2 200 \r\nContent-Length: 209715200\r\naccept-ranges: bytes\r\n\r\n";
        assert_eq!(content_length(github), Some(209_715_200), "the final response's");
        assert_eq!(content_length("Content-Length: 5\r\nAccept-ranges: bytes\r\n"), Some(5), "file://");
        assert_eq!(content_length("HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\n"), None, "an error page's");
        assert_eq!(content_length("HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nHTTP/1.1 403 Forbidden\r\n\r\n"), None);
        assert_eq!(content_length("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n"), None);
        assert_eq!(content_length("HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n"), None);
        assert_eq!(content_length(""), None);
    }

    #[tokio::test]
    async fn polls_a_growing_file() {
        let path = std::env::temp_dir().join(format!("silo-release-poll-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let seen = std::sync::Mutex::new(Vec::new());
        // Written between the polls (at 0, 250, 500 and 750 ms).
        let grow = async {
            tokio::time::sleep(PROGRESS_POLL / 2).await;
            for _ in 0..3 {
                let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path).unwrap();
                std::io::Write::write_all(&mut file, &[0; 100]).unwrap();
                tokio::time::sleep(PROGRESS_POLL).await;
            }
            "done"
        };
        assert_eq!(poll_size(&path, |n| seen.lock().unwrap().push(n), grow).await, "done");
        let seen = seen.into_inner().unwrap();
        assert_eq!(seen.first(), Some(&0), "not there yet");
        assert!(seen.is_sorted() && seen.iter().any(|&n| n > 0) && *seen.last().unwrap() <= 300, "{seen:?}");
        std::fs::remove_file(path).unwrap();
    }
}
