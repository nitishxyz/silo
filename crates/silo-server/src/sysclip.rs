//! The host's system clipboard, which remote clients' pastes are pushed
//! into (so programs reading the clipboard directly see them) and, where
//! changes can be watched, copied back from.

use std::{
    io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use silo_proto::data::Clip;

pub trait SystemClipboard: Send + Sync {
    /// Put text or an image on the clipboard.
    ///
    /// # Errors
    /// Unsupported content or the clipboard cannot be written.
    fn write(&self, clip: &Clip) -> io::Result<()>;

    /// A counter that changes whenever the clipboard does; `None` when
    /// changes cannot be watched.
    fn change_count(&self) -> Option<i64> {
        None
    }

    /// The clipboard's text, if any.
    fn read_text(&self) -> Option<String> {
        None
    }
}

/// This host's system clipboard: the general pasteboard on macOS, the
/// display's clipboard through `wl-copy`/`xclip` elsewhere, `None` without
/// a display.
#[must_use]
pub fn native() -> Option<Arc<dyn SystemClipboard>> {
    #[cfg(target_os = "macos")]
    return Some(Arc::new(Pasteboard));
    #[cfg(not(target_os = "macos"))]
    return DisplayTools::from_env().map(|tools| Arc::new(tools) as Arc<dyn SystemClipboard>);
}

fn unsupported(clip: &Clip) -> io::Error {
    let what = match clip {
        Clip::Text(_) => "text".to_owned(),
        Clip::Image { mime, .. } => mime.clone(),
        Clip::Types(_) => "a type list".to_owned(),
    };
    io::Error::new(io::ErrorKind::Unsupported, format!("cannot put {what} on the clipboard"))
}

#[cfg(target_os = "macos")]
pub struct Pasteboard;

#[cfg(target_os = "macos")]
impl SystemClipboard for Pasteboard {
    fn write(&self, clip: &Clip) -> io::Result<()> {
        use objc2_app_kit::{NSPasteboard, NSPasteboardTypePNG, NSPasteboardTypeString, NSPasteboardTypeTIFF};
        use objc2_foundation::{NSData, NSString};
        let board = NSPasteboard::generalPasteboard();
        // SAFETY: the pasteboard type constants are immutable AppKit statics.
        let (png, tiff, string) = unsafe { (NSPasteboardTypePNG, NSPasteboardTypeTIFF, NSPasteboardTypeString) };
        let written = match clip {
            Clip::Text(text) => {
                board.clearContents();
                board.setString_forType(&NSString::from_str(text), string)
            }
            Clip::Image { mime, data } if mime == "image/png" || mime == "image/tiff" => {
                board.clearContents();
                board.setData_forType(Some(&NSData::with_bytes(data)), if mime == "image/png" { png } else { tiff })
            }
            other => return Err(unsupported(other)),
        };
        if written { Ok(()) } else { Err(io::Error::other("the pasteboard refused the content")) }
    }

    fn change_count(&self) -> Option<i64> {
        Some(objc2_app_kit::NSPasteboard::generalPasteboard().changeCount() as i64)
    }

    fn read_text(&self) -> Option<String> {
        let board = objc2_app_kit::NSPasteboard::generalPasteboard();
        // SAFETY: an immutable AppKit static.
        board.stringForType(unsafe { objc2_app_kit::NSPasteboardTypeString }).map(|s| s.to_string())
    }
}

/// The display's clipboard through the real `wl-copy` (Wayland) or `xclip`
/// (X11), which keep serving the selection in the background.
pub struct DisplayTools {
    wayland: bool,
}

impl DisplayTools {
    /// Tools for the display named in this process's environment, if any.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        let set = |name: &str| std::env::var_os(name).is_some_and(|v| !v.is_empty());
        if set("WAYLAND_DISPLAY") {
            Some(Self { wayland: true })
        } else if set("DISPLAY") {
            Some(Self { wayland: false })
        } else {
            None
        }
    }
}

/// `tool` on `PATH`, skipping Silo's shims.
fn real_tool(tool: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let shims = silo_proto::paths::shims(&silo_proto::paths::home());
    let shims = shims.canonicalize().unwrap_or(shims);
    std::env::split_paths(&std::env::var_os("PATH")?)
        .filter(|dir| dir.canonicalize().unwrap_or_else(|_| dir.clone()) != shims)
        .map(|dir| dir.join(tool))
        .find(|path| path.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

/// Run `program args` with `input` on stdin; waits up to two seconds for
/// it to hand the selection to its background server.
fn feed(program: &Path, args: &[&str], input: &[u8]) -> io::Result<()> {
    use std::io::Write;
    let mut child = Command::new(program).args(args).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
    child.stdin.take().expect("piped stdin").write_all(input)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait()? {
            return if status.success() { Ok(()) } else { Err(io::Error::other(format!("{} failed: {status}", program.display()))) };
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

impl SystemClipboard for DisplayTools {
    fn write(&self, clip: &Clip) -> io::Result<()> {
        let (mime, bytes) = match clip {
            Clip::Text(text) => ("text/plain;charset=utf-8", text.as_bytes()),
            Clip::Image { mime, data } => (mime.as_str(), data.as_slice()),
            other @ Clip::Types(_) => return Err(unsupported(other)),
        };
        let tool = if self.wayland { "wl-copy" } else { "xclip" };
        let program = real_tool(tool).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, format!("{tool} is not installed")))?;
        if self.wayland {
            feed(&program, &["--type", mime], bytes)
        } else {
            let target = if matches!(clip, Clip::Text(_)) { "UTF8_STRING" } else { mime };
            feed(&program, &["-selection", "clipboard", "-t", target, "-i"], bytes)
        }
    }
}

/// A clipboard in memory, for tests.
#[derive(Default)]
pub struct Fake {
    state: Mutex<(i64, Option<Clip>)>,
    /// How long each write takes.
    delay: Duration,
}

impl Fake {
    /// A clipboard whose writes take `delay`.
    #[must_use]
    pub fn with_delay(delay: Duration) -> Self {
        Self { delay, ..Self::default() }
    }

    /// What was last put on the clipboard.
    #[must_use]
    pub fn content(&self) -> Option<Clip> {
        self.state.lock().unwrap().1.clone()
    }

    /// Change the clipboard as another program on the host would.
    pub fn set_by_host(&self, text: &str) {
        let mut state = self.state.lock().unwrap();
        state.0 += 1;
        state.1 = Some(Clip::Text(text.to_owned()));
    }
}

impl SystemClipboard for Fake {
    fn write(&self, clip: &Clip) -> io::Result<()> {
        if matches!(clip, Clip::Types(_)) {
            return Err(unsupported(clip));
        }
        std::thread::sleep(self.delay);
        let mut state = self.state.lock().unwrap();
        state.0 += 1;
        state.1 = Some(clip.clone());
        Ok(())
    }

    fn change_count(&self) -> Option<i64> {
        Some(self.state.lock().unwrap().0)
    }

    fn read_text(&self) -> Option<String> {
        match &self.state.lock().unwrap().1 {
            Some(Clip::Text(text)) => Some(text.clone()),
            _ => None,
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    /// Writes the real pasteboard: run by hand only.
    #[test]
    #[ignore = "writes the user's real pasteboard"]
    fn pasteboard_round_trips_text() {
        let board = Pasteboard;
        let before = board.change_count().unwrap();
        board.write(&Clip::Text("silo pasteboard test".into())).unwrap();
        assert_ne!(board.change_count().unwrap(), before);
        assert_eq!(board.read_text().as_deref(), Some("silo pasteboard test"));
    }
}
