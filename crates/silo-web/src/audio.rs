//! Whether a page plays sound, for the app's tab indicator.
//!
//! Chromium knows when a page is audible, but CEF only reports it through
//! `CefAudioHandler`, which captures the stream instead of playing it. So
//! a script in every frame watches its media elements and reports, over the
//! console, whether one plays with sound; the engine ORs the frames.
//! Web Audio alone (no `<audio>`/`<video>`) is not seen.

use std::collections::HashSet;

/// Run in every frame on load (`__KEY__` replaced by the browser's media
/// tag, as a JS string): logs `<tag><frame>:1` while one of its media
/// elements plays audibly (not paused, ended, muted or at volume 0), and
/// `<tag><frame>:0` once none does or the document goes away. Media events
/// don't bubble: they are caught on the way down, and elements played
/// while detached (`new Audio(src).play()`) are watched directly.
const MEDIA_SCRIPT: &str = r"(() => {
if (window.__siloMedia) return;
window.__siloMedia = true;
const key = __KEY__ + Math.random().toString(36).slice(2) + ':';
const log = console.log.bind(console);
const seen = new Set();
const watched = new WeakSet();
let last = false, timer = 0;
const audible = m => !m.paused && !m.ended && !m.muted && m.volume > 0;
const say = on => { if (on !== last) { last = on; log(key + (on ? 1 : 0)); } };
const report = () => {
  let on = false;
  for (const m of seen) {
    if (audible(m)) on = true;
    else if (m.paused || m.ended) seen.delete(m);
  }
  say(on);
  clearTimeout(timer);
  // A muted tab, a removed element: no event says so; look again.
  if (seen.size) timer = setTimeout(report, 1500);
};
const types = ['play', 'playing', 'pause', 'ended', 'volumechange', 'emptied', 'abort', 'error'];
const note = e => {
  const m = e.target;
  if (m instanceof HTMLMediaElement) { seen.add(m); report(); }
};
for (const type of types) addEventListener(type, note, true);
const play = HTMLMediaElement.prototype.play;
HTMLMediaElement.prototype.play = function (...args) {
  if (!watched.has(this)) {
    watched.add(this);
    for (const type of types) this.addEventListener(type, note);
  }
  seen.add(this);
  const result = play.apply(this, args);
  setTimeout(report);
  return result;
};
addEventListener('pagehide', () => { clearTimeout(timer); say(false); });
addEventListener('pageshow', report);
})()";

pub(crate) fn media_script(tag: &str) -> String {
    MEDIA_SCRIPT.replace("__KEY__", &serde_json::to_string(tag).unwrap_or_default())
}

/// The frames playing sound.
#[derive(Debug, Default)]
pub(crate) struct Frames(HashSet<String>);

impl Frames {
    /// A report (`<frame>:0|1`, the tag stripped); `Some(audible)` when the
    /// page as a whole changed.
    pub(crate) fn report(&mut self, report: &str) -> Option<bool> {
        let (frame, state) = report.rsplit_once(':')?;
        let before = !self.0.is_empty();
        match state {
            "1" => _ = self.0.insert(frame.to_owned()),
            "0" => _ = self.0.remove(frame),
            _ => return None,
        }
        let now = !self.0.is_empty();
        (now != before).then_some(now)
    }

    /// The main frame started a new document: every frame's goes. `Some(false)`
    /// if the page was audible.
    pub(crate) fn clear(&mut self) -> Option<bool> {
        let before = !self.0.is_empty();
        self.0.clear();
        before.then_some(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audible_while_any_frame_plays() {
        let mut frames = Frames::default();
        assert_eq!(frames.report("main:1"), Some(true));
        assert_eq!(frames.report("ad:1"), None, "already audible");
        assert_eq!(frames.report("main:0"), None, "the ad still plays");
        assert_eq!(frames.report("ad:0"), Some(false));
        assert_eq!(frames.report("ad:0"), None, "already silent");
        assert_eq!(frames.report("junk"), None);
        assert_eq!(frames.report("main:2"), None);
    }

    #[test]
    fn a_new_document_is_silent() {
        let mut frames = Frames::default();
        assert_eq!(frames.clear(), None);
        frames.report("main:1");
        assert_eq!(frames.clear(), Some(false));
        assert_eq!(frames.report("main:1"), Some(true));
    }

    #[test]
    fn the_script_gets_its_tag_as_a_string() {
        let script = media_script("\u{1}silo-media-1:");
        assert!(script.contains(r#"const key = "\u0001silo-media-1:" + "#));
        assert!(!script.contains("__KEY__"));
    }
}
