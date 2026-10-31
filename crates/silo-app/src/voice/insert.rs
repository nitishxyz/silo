//! Voice typing's one way into the focused block: where the text goes,
//! formatting it for there (and polishing it, when asked), typing it
//! through that block's own input path, and the keyword hints its content
//! suggests.

use std::{
    path::Path,
    time::{Duration, Instant},
};

use gpui::{Context, Entity, Keystroke, Window};
use otto_proto::TextStyle;
use silo_voice::{
    Target, hints,
    prefs::BUILT_IN_VOCABULARY,
    style::{self, Style},
};

use super::Stage;
use crate::{blocks::BlockPane, browser_pane::BrowserPane, pane::Pane, workspace::Workspace};

/// Polished text arrives within this, or the text as it is goes.
const POLISH_WAIT: Duration = Duration::from_secs(2);

/// Where dictated text goes.
enum Dest {
    /// A terminal: pasted (bracketed when the program asks).
    Terminal(Entity<Pane>),
    /// A web page: its focused element, through the page's text input.
    Page(Entity<BrowserPane>),
    /// The focused gpui text input (the agent composer, the Files editor, any
    /// text field): typed as one keystroke's text, as the IME commits it.
    Focused(Target),
}

impl Dest {
    fn target(&self) -> Target {
        match self {
            Self::Terminal(_) => Target::Terminal,
            Self::Page(_) => Target::Browser,
            Self::Focused(target) => *target,
        }
    }
}

/// A keystroke typing `text` and pressing no key.
fn typing(text: &str) -> Keystroke {
    Keystroke { modifiers: gpui::Modifiers::default(), key: String::new(), key_char: Some(text.to_owned()) }
}

impl Workspace {
    fn voice_dest(&self, window: &Window, cx: &gpui::App) -> Dest {
        let pane = self.block_with_focus(window, cx).and_then(|block| self.block_pane(&block));
        match pane {
            Some(BlockPane::Terminal(p)) => Dest::Terminal(p),
            Some(BlockPane::Browser(p)) if p.read(cx).focus.is_focused(window) => Dest::Page(p),
            Some(BlockPane::Agent(_)) => Dest::Focused(Target::Agent),
            Some(BlockPane::Files(p)) if p.read(cx).editing() && !p.read(cx).tree_focus.is_focused(window) => Dest::Focused(Target::Editor),
            _ => Dest::Focused(Target::Field),
        }
    }

    /// A transcript to the focused block: replacements, spoken commands and
    /// lists as that block takes them, polished first when asked (falling
    /// back to that within [`POLISH_WAIT`]).
    pub(crate) fn voice_deliver(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let prefs = &self.state.voice;
        let dest = self.voice_dest(window, cx);
        let target = dest.target();
        let vocabulary = prefs.vocabulary.iter().cloned().chain(BUILT_IN_VOCABULARY.map(str::to_owned)).collect();
        let style = Style { multiline: target != Target::Terminal, lists: prefs.format_lists.get(target), vocabulary };
        let plain = style::format(text, &prefs.replacements, &style);
        let local = self.hosts.iter().position(|h| h.info.local);
        let otto = local.filter(|_| prefs.polish && !plain.is_empty()).and_then(|host| self.agent_target(host));
        let Some(live) = self.voice.live.as_mut() else { return };
        let Some(otto) = otto else {
            live.stage = Stage::Leaving(Instant::now());
            return self.voice_type(dest, plain, window, cx);
        };
        live.polishing = true;
        let kind = match (style.multiline, style.lists) {
            (false, _) => TextStyle::Line,
            (true, false) => TextStyle::Prose,
            (true, true) => TextStyle::Markdown,
        };
        let (words, rules, send) = (prefs.keywords(Vec::new()), prefs.replacements.clone(), plain.clone());
        let task = self.rt.spawn(async move {
            let call = async {
                let c = crate::agent::client::connection(&otto).await.map_err(|e| e.to_string())?;
                c.text_polish(send, words, kind).await.map_err(|e| e.to_string())
            };
            tokio::time::timeout(POLISH_WAIT, call).await.map_err(|_| "timed out".to_owned())?
        });
        cx.spawn_in(window, async move |this, cx| {
            let polished = match task.await {
                Ok(Ok(text)) if !text.trim().is_empty() => Some(text),
                Ok(Ok(_)) => None,
                Ok(Err(error)) => {
                    tracing::debug!("voice: polish: {error}");
                    None
                }
                Err(error) => {
                    tracing::debug!("voice: polish: {error}");
                    None
                }
            };
            let _ = this.update_in(cx, |this, window, cx| {
                // Cancelled meanwhile (Esc).
                let Some(live) = this.voice.live.as_mut().filter(|l| l.polishing && matches!(l.stage, Stage::Finishing(_))) else { return };
                live.polishing = false;
                live.stage = Stage::Leaving(Instant::now());
                let text = polished.map_or(plain, |text| {
                    let text = style::replace(text.trim(), &rules);
                    if kind == TextStyle::Line { silo_voice::engine::tidy(&text) } else { text }
                });
                this.voice_type(dest, text, window, cx);
                cx.notify();
            });
        })
        .detach();
    }

    /// Type `text` into `dest`, then Return if that kind of target is set
    /// to. Done once the current update is over: typing goes through the
    /// window's key dispatch, which reaches this workspace too.
    fn voice_type(&mut self, dest: Dest, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let enter = self.state.voice.enter_after.get(dest.target());
        let this = cx.entity().downgrade();
        window.defer(cx, move |window, cx| {
            let typed = match dest {
                Dest::Terminal(pane) => pane.update(cx, |pane, cx| pane.type_text(text, cx)),
                Dest::Page(page) => {
                    page.update(cx, |page, cx| gpui::EntityInputHandler::replace_text_in_range(page, None, &text, window, cx));
                    true
                }
                Dest::Focused(_) => window.dispatch_keystroke(typing(&text), cx),
            };
            if typed && enter {
                let keystroke = Keystroke::parse("enter").expect("a key");
                window.dispatch_keystroke(keystroke.clone(), cx);
                window.dispatch_event(gpui::PlatformInput::KeyUp(gpui::KeyUpEvent { keystroke }), cx);
            }
            if !typed {
                let _ = this.update(cx, |this, cx| this.flash(crate::status::Status::info("Nothing to type into"), cx));
            }
        });
    }

    /// Words the focused block suggests: its folder's name, branch and
    /// entries (this Mac's folders only), a files block's open tabs and top
    /// folder.
    pub(crate) fn voice_hints(&self, window: &Window, cx: &gpui::App) -> Vec<String> {
        let Some(block) = self.focused_block(window, cx) else { return Vec::new() };
        let local = self.hosts.get(self.active_host).is_some_and(|h| h.info.local);
        let mut sources = match self.cwd_of(&block, cx) {
            Some(folder) if local => hints::folder(Path::new(&folder)),
            Some(folder) => {
                hints::Sources { project: Path::new(&folder).file_name().map(|n| n.to_string_lossy().into_owned()), ..Default::default() }
            }
            None => hints::Sources::default(),
        };
        if let Some(BlockPane::Files(p)) = self.block_pane(&block) {
            let p = p.read(cx);
            let open = p.state.open.iter().map(|path| crate::files::model::name(path).to_owned());
            let top = p.listings.get("").into_iter().flatten().map(|e| e.name.clone());
            sources.files = open.chain(top).chain(std::mem::take(&mut sources.files)).collect();
        }
        hints::keywords(&sources)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn typing_is_text_without_a_key() {
        let k = super::typing("hello world");
        assert_eq!((k.key.as_str(), k.key_char.as_deref()), ("", Some("hello world")));
        assert!(!k.modifiers.modified(), "no chord: bindings never match it");
    }
}
