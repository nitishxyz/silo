//! Voice typing's one way into the focused block: where the text goes,
//! typing it there through that block's own input path, and the keyword
//! hints its content suggests.

use std::path::Path;

use gpui::{Context, Entity, Keystroke, Window};
use silo_voice::{Target, hints};

use crate::{blocks::BlockPane, browser_pane::BrowserPane, pane::Pane, workspace::Workspace};

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

    /// Type `text` into whatever has focus, then Return if that kind of
    /// target is set to. Done once the current update is over: typing goes
    /// through the window's key dispatch, which reaches this workspace too.
    pub(crate) fn voice_insert(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let dest = self.voice_dest(window, cx);
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
