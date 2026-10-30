//! Settings › Voice, owner side: what the page shows and its changes.

use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

use gpui::Context;
use silo_voice::{EngineKind, VoicePrefs};

use super::fetch::{self, Fetch};
use crate::{settings::Outcome, workspace::Workspace};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MicAccess {
    #[default]
    NotAsked,
    Granted,
    Denied,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EngineRow {
    pub kind: EngineKind,
    /// This build has it.
    pub available: bool,
    pub installed: bool,
    pub fetch: Option<Fetch>,
}

#[derive(Clone, Debug, Default)]
pub struct VoiceSettings {
    pub prefs: VoicePrefs,
    pub engines: Vec<EngineRow>,
    pub microphones: Vec<String>,
    /// macOS's "Press 🌐 key to" does something else.
    pub fn_conflict: bool,
    pub mic: MicAccess,
}

#[derive(Clone, Debug)]
pub enum VoiceEvent {
    Set(VoicePrefs),
    /// Voice typing on or off (the ⌘P finder's switch).
    SetOn(bool),
    Download(EngineKind),
    Delete(EngineKind),
    /// System Settings › Keyboard (the 🌐 key's use).
    OpenKeyboardSettings,
    /// Ask for the microphone, or show its privacy page once refused.
    GrantMicrophone,
}

/// A value read from the system at most every few seconds.
struct Cached<T>(Mutex<Option<(Instant, T)>>);

impl<T: Clone> Cached<T> {
    const fn new() -> Self {
        Self(Mutex::new(None))
    }

    fn get(&self, read: impl FnOnce() -> T) -> T {
        let Ok(mut slot) = self.0.lock() else { return read() };
        match &*slot {
            Some((at, value)) if at.elapsed() < Duration::from_secs(4) => value.clone(),
            _ => {
                let value = read();
                *slot = Some((Instant::now(), value.clone()));
                value
            }
        }
    }
}

/// `defaults read com.apple.HIToolbox AppleFnUsageType`: 0 is "Do
/// Nothing"; unset is the system's default (emoji or input source).
#[must_use]
pub fn fn_conflict(value: Option<&str>) -> bool {
    value.map(str::trim) != Some("0")
}

fn read_fn_usage() -> Option<String> {
    let out = std::process::Command::new("/usr/bin/defaults").args(["read", "com.apple.HIToolbox", "AppleFnUsageType"]).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

fn open_url(url: &str) {
    if let Err(error) = std::process::Command::new("/usr/bin/open").arg(url).spawn() {
        tracing::warn!("open {url}: {error}");
    }
}

fn mic_access() -> MicAccess {
    #[cfg(target_os = "macos")]
    {
        use silo_voice::permission::Access;
        match silo_voice::permission::access() {
            Access::NotAsked => MicAccess::NotAsked,
            Access::Granted => MicAccess::Granted,
            Access::Denied => MicAccess::Denied,
        }
    }
    #[cfg(not(target_os = "macos"))]
    MicAccess::Denied
}

impl Workspace {
    pub(crate) fn voice_settings(&self) -> VoiceSettings {
        static MICS: Cached<Vec<String>> = Cached::new();
        static FN: Cached<bool> = Cached::new();
        // Listing input devices makes macOS ask for the microphone, so only once it's allowed:
        // the first ask comes from using voice typing (or Settings' Allow), never from a listing.
        let mic = mic_access();
        #[cfg(target_os = "macos")]
        let microphones = if mic == MicAccess::Granted { MICS.get(silo_voice::capture::microphones) } else { Vec::new() };
        #[cfg(not(target_os = "macos"))]
        let microphones = MICS.get(Vec::new);
        let engines = EngineKind::ALL
            .into_iter()
            .map(|kind| EngineRow {
                kind,
                available: silo_voice::engine::available(kind),
                installed: kind.model().installed(&self.home),
                fetch: fetch::fetch(kind),
            })
            .collect();
        VoiceSettings { prefs: self.state.voice.clone(), engines, microphones, fn_conflict: FN.get(|| fn_conflict(read_fn_usage().as_deref())), mic }
    }

    pub(crate) fn apply_voice_event(&mut self, event: &VoiceEvent, cx: &mut Context<Self>) -> Outcome {
        match event {
            VoiceEvent::Set(prefs) => {
                let prefs = prefs.clone();
                self.shared.update(cx, |shared, cx| shared.update_state(|s| s.voice = prefs, cx));
            }
            VoiceEvent::SetOn(on) => {
                let on = *on;
                self.shared.update(cx, |shared, cx| shared.update_state(|s| s.voice.on = on, cx));
            }
            VoiceEvent::Download(kind) => {
                fetch::start(self.home.clone(), *kind, &self.rt);
                self.follow_voice_downloads(cx);
            }
            VoiceEvent::Delete(kind) => {
                if let Err(error) = kind.model().delete(&self.home) {
                    tracing::warn!("delete voice model: {error}");
                    return Outcome::failed("Couldn't delete the voice model.");
                }
            }
            VoiceEvent::OpenKeyboardSettings => open_url("x-apple.systempreferences:com.apple.Keyboard-Settings.extension"),
            VoiceEvent::GrantMicrophone => match mic_access() {
                #[cfg(target_os = "macos")]
                MicAccess::NotAsked => {
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    let tx = std::sync::Mutex::new(Some(tx));
                    silo_voice::permission::request(move |_| {
                        if let Some(tx) = tx.lock().ok().and_then(|mut tx| tx.take()) {
                            let _ = tx.send(());
                        }
                    });
                    cx.spawn(async move |this, cx| {
                        let _ = rx.await;
                        let _ = this.update(cx, |this, cx| this.refresh_settings(None, cx));
                    })
                    .detach();
                }
                _ => open_url("x-apple.systempreferences:com.apple.preference.security?Privacy_Microphone"),
            },
        }
        Outcome::default()
    }

    /// Refresh the open settings while a model downloads, and once after.
    fn follow_voice_downloads(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(250)).await;
                let busy = fetch::busy();
                if this.update(cx, |this, cx| this.refresh_settings(None, cx)).is_err() || !busy {
                    return;
                }
            }
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_do_nothing_leaves_fn_to_silo() {
        assert!(!super::fn_conflict(Some("0\n")));
        assert!(super::fn_conflict(Some("2")), "emoji");
        assert!(super::fn_conflict(Some("1")), "input source");
        assert!(super::fn_conflict(None), "the system's default");
    }
}
