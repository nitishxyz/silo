//! New tabs a page opens (a ⌘-clicked link, `window.open`, `target=_blank`,
//! the context menu's Open in New Tab): one path for all. A tab of the page's
//! session is created (`window.create`, behind unless `foreground`); a popup's
//! engine page, created by CEF with its `window.opener` link, is adopted by a
//! new pane rather than reloaded. A window showing the new block takes that
//! pane; until one does, the off-screen host runs it (`browser_host.rs`).

use std::{
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use gpui::{App, AppContext as _};
use silo_proto::{
    control::{BlockView, Created, Method, OwnMode, SpawnSpec},
    model::{BlockKind, BrowserState},
};

use super::{BrowserPane, Route};

/// Everything a new tab's pane needs from the page that opened it.
struct Opener {
    rt: tokio::runtime::Handle,
    control: silo_client::Control,
    route: Route,
    home: std::path::PathBuf,
}

impl BrowserPane {
    /// Open `url` in a new tab of this block's session, adopting `popup`
    /// (the engine page CEF made for it) when there is one.
    pub(crate) fn open_tab(&mut self, url: String, foreground: bool, popup: Option<silo_web::Popup>, cx: &mut gpui::Context<Self>) {
        let session = self.info.as_ref().map(|i| i.session_id.clone());
        let start = if popup.is_some() { None } else { silo_proto::model::browser_url(&url) };
        let (Some(control), Some(session_id)) = (self.control.clone(), session) else { return discard(popup) };
        if popup.is_none() && start.is_none() {
            return;
        }
        // A popup's page is already loading: the block starts blank, so a
        // pane made for it before the adoption only shows the empty state.
        let method = Method::WindowCreate { session_id, spawn: SpawnSpec::default(), kind: BlockKind::Browser, url: start, background: !foreground };
        let opener = Opener { rt: self.rt.clone(), control: control.clone(), route: self.route.clone(), home: self.home.clone() };
        let task = self.rt.spawn(async move { control.call::<Created>(method).await });
        cx.spawn(async move |_, cx| {
            let created = task.await.map_err(anyhow::Error::from).and_then(|r| r);
            cx.update(|cx| match created {
                Ok(created) => opened(created.block_id, url, foreground, popup, opener, cx),
                Err(error) => {
                    tracing::warn!("new tab: {error:#}");
                    discard(popup);
                }
            });
        })
        .detach();
    }

    /// The pane of a new tab whose page is `popup`, at `url` (empty: blank).
    fn adopted(block_id: String, opener: &Opener, popup: silo_web::Popup, url: &str, cx: &mut gpui::Context<Self>) -> Self {
        let mut pane = Self::new(block_id, opener.rt.clone(), Some(opener.control.clone()), opener.route.clone(), opener.home.clone(), cx);
        // Set: the events since CEF made the page are drained on the next pump.
        let dirty = Arc::new(AtomicBool::new(true));
        let flag = dirty.clone();
        let page = Rc::new(popup.open(Arc::new(move || flag.store(true, Ordering::Release))));
        crate::web::register(page.clone(), dirty, Some(cx.weak_entity()));
        let url = if url.is_empty() { "about:blank" } else { url };
        pane.state = Some(BrowserState::new(url.to_owned()));
        url.clone_into(&mut pane.engine_url);
        pane.page = Some(page);
        pane.adopting = true;
        pane
    }
}

/// The new tab `block` exists: make its pane if it needs one now.
fn opened(block: String, url: String, foreground: bool, popup: Option<silo_web::Popup>, opener: Opener, cx: &mut App) {
    let pane = match popup {
        Some(popup) => cx.new(|cx| BrowserPane::adopted(block.clone(), &opener, popup, &url, cx)),
        // A tab in front: the window showing it makes its pane.
        None if foreground => return,
        // Behind: its page loads now, as in a browser.
        None => cx.new(|cx| {
            BrowserPane::new(block.clone(), opener.rt.clone(), Some(opener.control.clone()), opener.route.clone(), opener.home.clone(), cx)
        }),
    };
    let adopting = pane.read(cx).adopting;
    if !crate::browser_host::BrowserHost::adopt(pane.clone(), adopting, cx) {
        return;
    }
    // Own it first: the page's state is this app's to report, not the server's blank one.
    let (rt, control) = (opener.rt, opener.control);
    let task = rt.spawn(async move {
        control.call_value(Method::OwnBlock { block_ids: vec![block.clone()], mode: OwnMode::Request }).await?;
        control.call::<BlockView>(Method::BlockView { block_id: block }).await
    });
    let pane = pane.downgrade();
    cx.spawn(async move |cx| match task.await.map_err(anyhow::Error::from).and_then(|r| r) {
        Ok(view) => _ = pane.update(cx, |pane, cx| pane.set_info(view.block, cx)),
        Err(error) => tracing::warn!("new tab: {error:#}"),
    })
    .detach();
}

/// A popup nobody will show: close it (before or after CEF made it).
fn discard(popup: Option<silo_web::Popup>) {
    if let Some(popup) = popup {
        popup.open(Arc::new(|| {})).close();
    }
}
