//! Listing blocks: one row model for every list of the shown session's
//! blocks: ⌘P's Blocks section, Browse Blocks (⇧⌘B, ⌘P on `#`) and the
//! titlebar's background menu. Each row has the block's kind (its glyph),
//! title, state, who started it and where it is; picking one goes to the
//! same place from any list ([`Workspace::pick_block`]).

use gpui::{Context, Window};
use silo_proto::{
    control::Method,
    model::{Busy, Lifecycle},
};

use crate::{blocks::Kind, finder::Target, workspace::Workspace};

/// One block of the shown session, as every list shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BlockRow {
    pub block: String,
    pub kind: Kind,
    pub title: String,
    /// See [`state_text`].
    pub state: String,
    /// The block that started it (an agent), by its title, while that
    /// block is still in the session.
    pub by: Option<String>,
    /// The running command line, when the shell reported one.
    pub command: Option<String>,
    /// A program runs: closing asks first.
    pub busy: bool,
    pub place: Place,
}

/// Where a block is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Place {
    /// In tab `index` (named `name`), tiled or floating.
    Tab { index: usize, name: String, float: bool },
    /// Parked: in no tab.
    Background,
}

impl BlockRow {
    /// The kind's glyph.
    pub(crate) fn icon(&self) -> &'static str {
        self.kind.icon()
    }

    /// Its state and who started it: `running cargo · started by otto`.
    pub(crate) fn status(&self) -> String {
        let by = self.by.as_ref().map(|by| format!(" · started by {by}")).unwrap_or_default();
        format!("{}{by}", self.state)
    }

    /// Where it is, after its title in ⌘P: its tab (`Tab 2 · review`,
    /// `· floating`), or `background` and its [`Self::status`].
    pub(crate) fn detail(&self) -> String {
        match &self.place {
            Place::Tab { index, name, float } => {
                let mut detail = if *name == self.title { format!("Tab {}", index + 1) } else { format!("Tab {} · {name}", index + 1) };
                if *float {
                    detail.push_str(" · floating");
                }
                detail
            }
            Place::Background => format!("background · {}", self.status()),
        }
    }

    /// Where picking it goes: the block in its tab, or a background block
    /// floated into the shown tab.
    pub(crate) fn target(&self) -> Target {
        match &self.place {
            Place::Tab { index, .. } => Target::Block { index: *index, block: self.block.clone() },
            Place::Background => Target::Background(self.block.clone()),
        }
    }

    pub(crate) fn in_background(&self) -> bool {
        self.place == Place::Background
    }
}

/// A block's state in a word or three: `running cargo`, `idle`,
/// `exited · code 1`, `exited`, `restored`.
#[must_use]
pub(crate) fn state_text(lifecycle: Lifecycle, exited: Option<i32>, busy: Option<&Busy>) -> String {
    match (lifecycle, exited, busy) {
        (Lifecycle::Running, _, Some(busy)) => format!("running {}", busy.process),
        (Lifecycle::Running, _, None) => "idle".into(),
        (Lifecycle::Exited, Some(code), _) => format!("exited · code {code}"),
        (Lifecycle::Exited, None, _) => "exited".into(),
        (Lifecycle::Restored, ..) => "restored".into(),
        (Lifecycle::Unknown, ..) => "stopped".into(),
    }
}

impl Workspace {
    /// Every block of the shown session: each tab's tiled and floating
    /// ones in order, then the background ones, oldest first.
    pub(crate) fn block_rows(&self, cx: &gpui::App) -> Vec<BlockRow> {
        let Some(view) = &self.view else { return Vec::new() };
        let mut out = Vec::new();
        for (index, w) in view.session.windows.iter().enumerate() {
            let name = self.window_title(w, cx);
            for block in crate::floats::window_blocks(w) {
                let float = crate::floats::is_float(w, &block);
                out.push(self.block_row(block, None, Place::Tab { index, name: name.clone(), float }, cx));
            }
        }
        for parked in &view.session.parked {
            out.push(self.block_row(parked.id.clone(), parked.owner.as_deref(), Place::Background, cx));
        }
        out
    }

    /// Row of `block`; `owner`: who parked it, if the session says.
    fn block_row(&self, block: String, owner: Option<&str>, place: Place, cx: &gpui::App) -> BlockRow {
        let info = self.block_info(&block);
        let busy = self.block_status(&block).and_then(|s| s.busy.as_ref());
        let state = info.map_or_else(|| "starting".to_owned(), |i| state_text(i.lifecycle, i.exited, busy));
        let owner = owner.or_else(|| info.and_then(|i| i.owner_block.as_deref()));
        // Only a block still in the session has a name to show.
        let by = owner.filter(|o| self.block_info(o).is_some()).map(|o| self.block_label(o, cx));
        BlockRow {
            kind: self.block_kind(&block),
            title: self.block_label(&block, cx),
            state,
            by,
            command: busy.and_then(|b| b.command.clone()),
            busy: busy.is_some(),
            place,
            block,
        }
    }

    /// The shown session's background blocks, oldest first.
    pub(crate) fn background_rows(&self, cx: &gpui::App) -> Vec<BlockRow> {
        self.block_rows(cx).into_iter().filter(BlockRow::in_background).collect()
    }

    /// Go to a listed block ([`BlockRow::target`]), from any list.
    pub(crate) fn pick_block(&mut self, target: Target, window: &mut Window, cx: &mut Context<Self>) {
        match target {
            Target::Block { index, block } => {
                let active = self.active_window().map(|w| w.id.clone());
                let target = self.view.as_ref().and_then(|v| v.session.windows.get(index)).map(|w| w.id.clone());
                if target.is_some() && target == active {
                    self.focus_block(&block, window, cx);
                } else {
                    // The server's focus is followed once the tab shows.
                    self.call(Method::BlockFocus { block_id: block });
                    self.select_window(index);
                }
            }
            Target::Background(block) => self.reveal_agent_target(block, window, cx),
            other => tracing::warn!(?other, "not a block"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn busy(process: &str) -> Busy {
        Busy { process: process.into(), command: Some(format!("{process} build")), since: 0 }
    }

    #[test]
    fn rows_say_what_runs_or_how_it_ended() {
        assert_eq!(state_text(Lifecycle::Running, None, Some(&busy("cargo"))), "running cargo");
        assert_eq!(state_text(Lifecycle::Running, None, None), "idle", "a shell at its prompt");
        assert_eq!(state_text(Lifecycle::Exited, Some(0), None), "exited · code 0");
        assert_eq!(state_text(Lifecycle::Exited, Some(101), None), "exited · code 101");
        assert_eq!(state_text(Lifecycle::Exited, None, None), "exited", "killed by a signal");
        // An exited block's stale busy record never reads as running.
        assert_eq!(state_text(Lifecycle::Exited, Some(1), Some(&busy("sleep"))), "exited · code 1");
        assert_eq!(state_text(Lifecycle::Restored, None, None), "restored");
        assert_eq!(state_text(Lifecycle::Unknown, None, None), "stopped");
    }

    fn row(kind: Kind, place: Place) -> BlockRow {
        let title = format!("{kind:?}").to_lowercase();
        BlockRow { block: format!("b-{title}"), kind, title, state: "idle".into(), by: None, command: None, busy: false, place }
    }

    #[test]
    fn every_kind_lists_alike_in_a_tab_or_the_background() {
        for (i, kind) in Kind::ALL.into_iter().enumerate() {
            let tab = row(kind, Place::Tab { index: i, name: "work".into(), float: false });
            assert_eq!(tab.icon(), kind.icon(), "{kind:?}");
            assert_eq!(tab.detail(), format!("Tab {} · work", i + 1), "{kind:?}");
            assert_eq!(tab.target(), Target::Block { index: i, block: tab.block.clone() }, "{kind:?}");
            assert!(!tab.in_background());
            let parked = BlockRow { by: Some("otto".into()), ..row(kind, Place::Background) };
            assert_eq!(parked.icon(), kind.icon(), "{kind:?}");
            assert_eq!(parked.status(), "idle · started by otto", "{kind:?}");
            assert_eq!(parked.detail(), "background · idle · started by otto", "{kind:?}");
            assert_eq!(parked.target(), Target::Background(parked.block.clone()), "{kind:?}");
            assert!(parked.in_background());
        }
    }

    #[test]
    fn a_tab_named_like_its_block_is_not_repeated() {
        let alone = row(Kind::Terminal, Place::Tab { index: 0, name: "terminal".into(), float: false });
        assert_eq!(alone.detail(), "Tab 1");
        let float = row(Kind::Browser, Place::Tab { index: 0, name: "api".into(), float: true });
        assert_eq!(float.detail(), "Tab 1 · api · floating");
        assert_eq!(row(Kind::Files, Place::Background).detail(), "background · idle");
    }
}
