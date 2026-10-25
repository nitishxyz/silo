//! `silo update` of a standalone binary from a local release feed: the
//! binary is replaced by the release's and its `silo up` takes the running
//! server over live, keeping the session. Everything stays in a temp
//! directory (its own HOME and SILO_HOME, no service manager).

use std::{
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

const VERSION: &str = env!("CARGO_PKG_VERSION");

struct Env {
    root: PathBuf,
    bin: PathBuf,
}

impl Env {
    fn silo(&self, args: &[&str]) -> Output {
        let out = Command::new(&self.bin)
            .args(args)
            .env("HOME", self.root.join("u"))
            // Never the caller's otto home: `silo server` claims it at start.
            .env_remove("OTTO_SILO_HOME")
            .env("SILO_HOME", self.root.join("h"))
            .env("SILO_UPDATE_FEED", format!("file://{}", self.root.join("f").display()))
            .env("SILO_SERVICE_MANAGER", "none")
            .env("SILO_NO_MENUBAR", "1")
            .env_remove("SILO_LOG")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success(), "silo {args:?}: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        out
    }

    fn stdout(&self, args: &[&str]) -> String {
        String::from_utf8(self.silo(args).stdout).unwrap()
    }
}

impl Drop for Env {
    /// Stop the server with SIGTERM (`silo kill-server` needs an approval
    /// this test has nobody to give), wait for it to exit, then clean up.
    fn drop(&mut self) {
        let runtime = std::fs::read(silo_proto::paths::runtime(&self.root.join("h")));
        let pid = runtime.ok().and_then(|bytes| serde_json::from_slice::<silo_proto::paths::Runtime>(&bytes).ok()).map(|r| r.pid as i32);
        if let Some(pid) = pid {
            // SAFETY: signalling the server this test started.
            unsafe { libc::kill(pid, libc::SIGTERM) };
            for _ in 0..250 {
                // SAFETY: signal 0 only checks for existence.
                if unsafe { libc::kill(pid, 0) } != 0 {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// `bytes` with every `from` replaced by `to` (same length).
fn patch(bytes: &mut [u8], from: &str, to: &str) -> usize {
    assert_eq!(from.len(), to.len());
    let mut count = 0;
    let mut at = 0;
    while let Some(i) = bytes[at..].windows(from.len()).position(|w| w == from.as_bytes()) {
        bytes[at + i..at + i + to.len()].copy_from_slice(to.as_bytes());
        at += i + to.len();
        count += 1;
    }
    count
}

/// This build as the next release: the same binary reporting the next
/// patch version and another build id (so its `silo up` takes over from
/// this build's server), re-signed ad hoc on macOS.
fn next_release(this: &Path, build: &str, dest: &Path) -> (String, String) {
    // The next patch, else minor, else major that keeps the version's length (the binary is patched in place):
    // 0.1.8 → 0.1.9, 0.1.9 → 0.2.0.
    let parts: Vec<u32> = VERSION.split('.').map(|p| p.parse().unwrap()).collect();
    let next = (0..parts.len())
        .rev()
        .map(|i| {
            parts
                .iter()
                .enumerate()
                .map(|(j, p)| {
                    match j.cmp(&i) {
                        std::cmp::Ordering::Less => *p,
                        std::cmp::Ordering::Equal => p + 1,
                        std::cmp::Ordering::Greater => 0,
                    }
                    .to_string()
                })
                .collect::<Vec<_>>()
                .join(".")
        })
        .find(|v| v.len() == VERSION.len())
        .expect("a newer version of the same length");
    let other: String = build.chars().map(|c| if c == 'f' { '0' } else { char::from_digit(c.to_digit(16).unwrap() + 1, 16).unwrap() }).collect();
    let mut bytes = std::fs::read(this).unwrap();
    assert!(patch(&mut bytes, &format!("silo {VERSION}"), &format!("silo {next}")) + patch(&mut bytes, VERSION, &next) > 0);
    assert!(patch(&mut bytes, build, &other) > 0);
    std::fs::write(dest, bytes).unwrap();
    std::fs::set_permissions(dest, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    if cfg!(target_os = "macos") {
        let signed = Command::new("codesign").args(["--force", "--sign", "-"]).arg(dest).output().unwrap();
        assert!(signed.status.success(), "{}", String::from_utf8_lossy(&signed.stderr));
    }
    (next, other)
}

fn sha256(path: &Path) -> String {
    silo_proto::release::sha256_file(path).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn updates_a_standalone_binary_and_its_server_live() {
    let root = PathBuf::from(format!("/tmp/silo-cli-update-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for dir in ["b", "f", "h", "u", "new"] {
        std::fs::create_dir_all(root.join(dir)).unwrap();
    }
    let env = Env { bin: root.join("b/silo"), root: root.clone() };
    // The installed binary: this build, outside the cargo target directory.
    std::fs::copy(env!("CARGO_BIN_EXE_silo"), &env.bin).unwrap();
    let (_, build) = silo_proto::upgrade::parse_version(&env.stdout(&["--version"])).unwrap();
    let build = build.unwrap();
    let (next, next_build) = next_release(&env.bin, &build, &root.join("new/silo"));

    // The feed: latest.json, the platform's tarball and its checksum.
    let feed = root.join("f");
    let asset = silo_proto::upgrade::asset(&silo_proto::upgrade::platform());
    let packed = Command::new("tar").arg("-czf").arg(feed.join(&asset)).arg("-C").arg(root.join("new")).arg("silo").status().unwrap();
    assert!(packed.success());
    std::fs::write(feed.join("SHA256SUMS"), format!("{}  {asset}\n", sha256(&feed.join(&asset)))).unwrap();
    std::fs::write(feed.join("latest.json"), format!(r#"{{"tag_name":"v{next}","draft":false,"prerelease":false}}"#)).unwrap();

    // A server of this build with a session.
    env.silo(&["server", "--daemon", "--idle-exit", "0"]);
    let session = env.stdout(&["new", "-d", "-s", "kept", "--", "sleep", "600"]);
    assert!(!session.trim().is_empty());

    assert_eq!(env.stdout(&["update", "--check"]).trim(), format!("{next} available (installed {VERSION})"));
    let out = env.stdout(&["upgrade"]);
    assert!(out.contains(&format!("updated silo to {next}")), "{out}");
    assert!(out.contains("silo server running"), "its `silo up` ran: {out}");
    assert!(!out.contains("remote upgrade"), "no paired hosts, no hint: {out}");

    // The binary is the release's; the server is too, with the session.
    assert_eq!(env.stdout(&["--version"]).trim(), format!("silo {}", silo_proto::upgrade::channel_version_text(&next, &next_build)));
    let (control, _events) = silo_client::Control::connect(&silo_proto::paths::socket(&root.join("h")), "update test").await.unwrap();
    assert_eq!((control.hello().server.version.as_str(), control.hello().build.as_str()), (next.as_str(), next_build.as_str()));
    assert!(env.stdout(&["ls"]).contains("kept"), "the session survived the takeover");
    drop(control);
    let left: Vec<_> = std::fs::read_dir(root.join("b")).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(left, ["silo"], "nothing staged is left behind");

    assert_eq!(env.stdout(&["update"]).trim(), format!("silo {next} is up to date"));
    assert_eq!(env.stdout(&["update", "--check"]).trim(), format!("up to date ({next})"));
}
