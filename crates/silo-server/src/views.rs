//! Each app's own view of a session (docs/PLAN.md §3.8): which tab it
//! shows, which block has focus in each tab, and whether a tab is zoomed.
//! The tree (tabs, splits, ratios, floats) is shared; where each person is
//! looking is not, so one person's click never moves another's focus.
//!
//! A [`Session`]'s own `active_window` and its windows' `focused`,
//! `history` and `zoomed` hold one view at a time: the view of whoever
//! acted last ([`Registry::enter`] loads the caller's before a call
//! changes anything). Every other view is kept in [`Session::views`] and
//! brought back in line with the tree after each change. So the registry's
//! operations read and write the session's fields as they always did.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};
use silo_proto::model::{BlockId, ClientId, SessionInfo, WindowId};

use crate::{
    access::Principal,
    app_bridge,
    block::now_ms,
    registry::{Registry, Session, Window},
};

/// Views of apps that are not connected, kept per session so an app comes
/// back to where it was; beyond this the longest unused go.
const KEPT_VIEWS: usize = 8;
/// Longest view id a hello may give.
const MAX_VIEW_ID: usize = 64;

/// One app's view of a session.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct View {
    /// The tab it shows.
    pub active_window: WindowId,
    #[serde(default)]
    pub windows: HashMap<WindowId, WindowView>,
    /// When it last changed (unix ms).
    #[serde(default)]
    pub used_at: u64,
}

/// One app's view of a window (tab).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WindowView {
    pub focused: BlockId,
    /// Blocks by most recent focus, `focused` first.
    #[serde(default)]
    pub history: Vec<BlockId>,
    #[serde(default)]
    pub zoomed: bool,
}

impl WindowView {
    fn of(window: &Window) -> Self {
        Self { focused: window.focused.clone(), history: window.history.clone(), zoomed: window.zoomed }
    }
}

/// The key a client's view is kept under, from the id its hello gave
/// ([`silo_proto::control::ClientHello::view`]): the owner's apps by that
/// id, anyone else's under their grant, so a guest cannot name (and move)
/// the owner's view. `None` for an id no app of ours would send.
#[must_use]
pub fn key(principal: &Principal, id: &str) -> Option<String> {
    if id.is_empty() || id.len() > MAX_VIEW_ID || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return None;
    }
    Some(match principal.grant_id() {
        None => format!("o:{id}"),
        Some(grant) => format!("g:{grant}:{id}"),
    })
}

impl Session {
    /// The view the session's own fields hold now.
    fn held(&self) -> View {
        View {
            active_window: self.active_window.clone(),
            windows: self.windows.iter().map(|w| (w.id.clone(), WindowView::of(w))).collect(),
            used_at: now_ms(),
        }
    }

    /// Keep the view the fields hold as `key`'s.
    fn save_view(&mut self, key: &str) {
        let held = self.held();
        self.views.insert(key.to_owned(), held);
    }

    /// Give `key` a view if it has none: the one the fields hold.
    fn seed_view(&mut self, key: &str) {
        if !self.views.contains_key(key) {
            self.save_view(key);
        }
    }

    /// Put `key`'s view into the fields; without one they stay as they are
    /// (a first look at a session starts where the last person was).
    fn load_view(&mut self, key: &str) {
        let Some(view) = self.views.get(key) else { return };
        if self.windows.iter().any(|w| w.id == view.active_window) {
            self.active_window.clone_from(&view.active_window);
        }
        for window in &mut self.windows {
            let Some(own) = view.windows.get(&window.id).filter(|own| window.contains(&own.focused)) else { continue };
            window.focused.clone_from(&own.focused);
            window.history = own.history.iter().filter(|b| window.contains(b)).cloned().collect();
            window.zoomed = own.zoomed && window.layout.blocks().len() > 1;
        }
    }

    /// Bring every kept view in line with the tree: closed tabs and blocks
    /// leave it, new tabs join as the fields have them, and a view whose
    /// tab or focused block is gone (or hidden in a minimized float) moves
    /// to its own most recent block still shown, else to where the fields
    /// are.
    pub(crate) fn reconcile_views(&mut self) {
        let (windows, active) = (&self.windows, &self.active_window);
        for view in self.views.values_mut() {
            if !windows.iter().any(|w| w.id == view.active_window) {
                view.active_window.clone_from(active);
            }
            view.windows.retain(|id, _| windows.iter().any(|w| &w.id == id));
            for window in windows {
                let own = view.windows.entry(window.id.clone()).or_insert_with(|| WindowView::of(window));
                own.history.retain(|b| window.contains(b));
                if !window.shown(&own.focused) {
                    own.focused = own.history.iter().find(|b| window.shown(b)).unwrap_or(&window.focused).clone();
                }
                own.history.retain(|b| *b != own.focused);
                own.history.insert(0, own.focused.clone());
                own.zoomed &= window.layout.blocks().len() > 1;
            }
        }
    }

    /// Drop the longest-unused views beyond [`KEPT_VIEWS`], never one of `keep`.
    fn prune_views(&mut self, keep: &[&str]) {
        let mut idle: Vec<(u64, String)> =
            self.views.iter().filter(|(k, _)| !keep.contains(&k.as_str())).map(|(k, v)| (v.used_at, k.clone())).collect();
        if idle.len() <= KEPT_VIEWS {
            return;
        }
        idle.sort();
        for (_, key) in idle.drain(..idle.len() - KEPT_VIEWS) {
            self.views.remove(&key);
        }
    }

    /// The session as `key`'s view shows it; as the fields have it for no
    /// key, or a key without a view.
    #[must_use]
    pub fn info_for(&self, key: Option<&str>) -> SessionInfo {
        let mut info = self.info();
        let Some(view) = key.and_then(|k| self.views.get(k)) else { return info };
        if info.windows.iter().any(|w| w.id == view.active_window) {
            info.active_window.clone_from(&view.active_window);
        }
        for window in &mut info.windows {
            if let Some(own) = view.windows.get(&window.id) {
                window.focused.clone_from(&own.focused);
                window.zoomed = own.zoomed;
            }
        }
        info
    }
}

/// The kept views of a session, by [`key`].
pub type Views = BTreeMap<String, View>;

impl Registry {
    /// The view `client` sees and acts on: its own if its hello named one,
    /// else (the CLI, scripts, agents, apps from before views) the view of
    /// the user's most recently active app, so `silo split` typed in a pane
    /// focuses the new block in the app it was typed in. `None`: no app
    /// with a view is connected; the session's fields are all there is.
    #[must_use]
    pub fn view_key(&self, client: ClientId) -> Option<String> {
        match self.clients.get(&client).and_then(|c| c.view.clone()) {
            Some(key) => Some(key),
            None => self.proxy_view(),
        }
    }

    /// The view of the user's most recently active app.
    pub(crate) fn proxy_view(&self) -> Option<String> {
        self.clients
            .iter()
            .filter(|(_, c)| c.view.is_some() && (app_bridge::owner_app(c) || app_bridge::trusted_remote_app(c)))
            .max_by_key(|(id, c)| (c.active_at, **id))
            .and_then(|(_, c)| c.view.clone())
    }

    /// `client` connected naming view `key`: it starts, in every session it
    /// has no view of yet, where the last person to act is.
    pub fn adopt_view(&mut self, client: ClientId, key: String) {
        for session in &mut self.sessions {
            session.seed_view(&key);
        }
        if let Some(entry) = self.clients.get_mut(&client) {
            entry.view = Some(key);
        }
    }

    /// Act as `client`: put its view into every session's fields, so what
    /// the call that follows (under the same lock) reads as "the active
    /// tab" and "the focused block" is this client's, and what it changes
    /// is too. The view the fields held is kept first.
    pub fn enter(&mut self, client: ClientId) {
        let Some(key) = self.view_key(client) else { return };
        if self.held_view.as_deref() == Some(key.as_str()) {
            return;
        }
        for session in &mut self.sessions {
            if let Some(held) = &self.held_view {
                session.save_view(held);
            }
            session.load_view(&key);
        }
        self.held_view = Some(key);
    }

    /// Session `si` changed: its fields are the view of whoever acted
    /// last; keep them as that view, give connected apps new to the session
    /// one, and bring the others in line with the tree.
    pub(crate) fn sync_views(&mut self, si: usize) {
        let connected: Vec<&str> = self.clients.values().filter_map(|c| c.view.as_deref()).collect();
        let session = &mut self.sessions[si];
        if let Some(held) = &self.held_view {
            session.save_view(held);
        }
        for key in &connected {
            session.seed_view(key);
        }
        session.reconcile_views();
        let mut keep = connected;
        keep.extend(self.held_view.as_deref());
        session.prune_views(&keep);
    }
}

#[cfg(test)]
mod tests {
    use silo_proto::model::{Axis, Float, Layout};

    use super::*;

    /// Session `s`: window `w` with `a | b`, window `x` with `c`.
    fn session() -> Session {
        let mut layout = Layout::block("a");
        layout.split("a", "b".into(), Axis::Horizontal);
        Session {
            id: "s".into(),
            name: "work".into(),
            active_window: "w".into(),
            windows: vec![Window { layout, ..Window::new("w".into(), "a") }, Window::new("x".into(), "c")],
            revision: 1,
            created_at: 0,
            parked: Vec::new(),
            views: Views::new(),
        }
    }

    fn window<'a>(s: &'a mut Session, id: &str) -> &'a mut Window {
        s.windows.iter_mut().find(|w| w.id == id).unwrap()
    }

    fn shown(s: &Session, key: &str) -> (String, String) {
        let info = s.info_for(Some(key));
        let focused = info.windows.iter().find(|w| w.id == info.active_window).unwrap().focused.clone();
        (info.active_window, focused)
    }

    #[test]
    fn keys_keep_guests_out_of_the_owner_s_views() {
        assert_eq!(key(&Principal::Owner, "mac-1").as_deref(), Some("o:mac-1"));
        assert_eq!(key(&Principal::Owner, ""), None);
        assert_eq!(key(&Principal::Owner, "o:mac"), None, "no separators");
        assert_eq!(key(&Principal::Owner, &"x".repeat(65)), None);
    }

    #[test]
    fn a_view_is_kept_apart_from_the_fields() {
        let mut s = session();
        s.save_view("one");
        // Someone else moves the fields: another tab, another block.
        s.active_window = "x".into();
        window(&mut s, "w").focus("b");
        s.save_view("two");
        assert_eq!(shown(&s, "one"), ("w".into(), "a".into()));
        assert_eq!(shown(&s, "two"), ("x".into(), "c".into()));
        // No view of its own: what the fields have.
        assert_eq!(shown(&s, "three"), ("x".into(), "c".into()));
        // Loading brings a view back into the fields, history and all.
        s.load_view("one");
        assert_eq!(
            (s.active_window.as_str(), s.windows[0].focused.as_str(), s.windows[0].history.as_slice()),
            ("w", "a", ["a".to_owned()].as_slice())
        );
    }

    #[test]
    fn views_follow_the_tree() {
        let mut s = session();
        window(&mut s, "w").focus("b");
        window(&mut s, "w").focus("a");
        window(&mut s, "w").zoomed = true;
        s.save_view("one");
        // Its focused block closes: back to the one it focused before, and
        // a tab of one block is not zoomed.
        let w = window(&mut s, "w");
        w.layout = Layout::block("b");
        w.focus("b");
        w.history.retain(|b| b != "a");
        w.zoomed = false;
        s.reconcile_views();
        assert_eq!(shown(&s, "one"), ("w".into(), "b".into()));
        assert!(!s.views["one"].windows["w"].zoomed);
        assert_eq!(s.views["one"].windows["w"].history, ["b"]);
        // Its tab closes: where the fields are. A new tab joins every view.
        s.windows.remove(0);
        s.windows.push(Window::new("y".into(), "d"));
        s.active_window = "y".into();
        s.reconcile_views();
        assert_eq!(shown(&s, "one"), ("y".into(), "d".into()));
        assert_eq!(s.views["one"].windows.len(), 2);
    }

    #[test]
    fn a_minimized_float_does_not_keep_focus_in_any_view() {
        let mut s = session();
        let w = window(&mut s, "w");
        w.floats.push(Float::new("f", Default::default()));
        w.focus("b");
        w.focus("f");
        s.save_view("one");
        let w = window(&mut s, "w");
        w.floats[0].minimized = true;
        w.focus("a");
        s.reconcile_views();
        assert_eq!(shown(&s, "one"), ("w".into(), "b".into()), "its own previous block, not the fields'");
    }

    #[test]
    fn idle_views_beyond_the_limit_are_dropped_oldest_first() {
        let mut s = session();
        for i in 0..KEPT_VIEWS + 3 {
            s.save_view(&format!("v{i:02}"));
            s.views.get_mut(&format!("v{i:02}")).unwrap().used_at = i as u64;
        }
        s.prune_views(&["v00"]);
        assert_eq!(s.views.len(), KEPT_VIEWS + 1);
        assert!(s.views.contains_key("v00"), "a connected app's view stays");
        assert!(!s.views.contains_key("v01") && !s.views.contains_key("v02") && s.views.contains_key("v03"));
    }
}
