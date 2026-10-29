//! The card shown while a server of a different build holds the local
//! host, and its one Upgrade: this build's server takes over the running
//! one live, keeping every session; a server from before live upgrade is
//! restarted instead, ending its sessions.

use gpui::{AnyElement, Context, FontWeight, IntoElement, SharedString, div, prelude::*, px, rgb};
use silo_client::mismatch::Upgraded;

use crate::surface::Glassy as _;
use crate::{hosts::HostState, status::Status, theme, theme::Continuous as _, workspace::Workspace};

/// How long the note after an upgrade stays in the status bar.
const NOTICE_FOR: std::time::Duration = std::time::Duration::from_secs(6);

/// The opaque card these problems are shown on, over or instead of content.
fn card() -> gpui::Div {
    div()
        .max_w(px(460.0))
        .flex()
        .flex_col()
        .items_center()
        .gap(px(12.0))
        .px(px(28.0))
        .py(px(22.0))
        .glass_sheet(crate::settings_panel::CARD_RADIUS)
        .text_center()
}

impl Workspace {
    /// The active host's incompatible server, if one holds it.
    pub(crate) fn incompatible(&self) -> Option<&silo_client::IncompatibleServer> {
        self.hosts.get(self.active_host)?.incompatible.as_ref()
    }

    /// The service running the active host's server, if it is this Mac.
    pub(crate) fn local_service(&self) -> Option<silo_client::service::Service> {
        match &self.hosts.get(self.active_host)?.info.transport {
            crate::hosts::Transport::Local { service: Ok(service), .. } => Some(service.clone()),
            _ => None,
        }
    }

    /// Upgrade the incompatible server on the active (local) host; does
    /// nothing while an upgrade runs. Until this build connects the card
    /// shows "Upgrading…"; then a short note says how it went.
    pub(crate) fn upgrade_server(&mut self, cx: &mut Context<Self>) {
        let (Some(server), Some(service)) = (self.incompatible().cloned(), self.local_service()) else { return };
        if std::mem::replace(&mut self.upgrading, true) {
            return;
        }
        self.status = None;
        let task = self.rt.spawn(async move { crate::conn::upgrade(&service, &server).await });
        cx.spawn(async move |this, cx| {
            let result = task.await.map_err(anyhow::Error::from).and_then(|r| r);
            let _ = this.update(cx, |this, cx| {
                let notice = match result {
                    Ok(Upgraded::Live) => "Server upgraded",
                    Ok(Upgraded::Stopped) => "Server upgraded · old sessions ended",
                    Err(error) => {
                        tracing::warn!("upgrade server: {error:#}");
                        this.upgrading = false;
                        this.status = Some(Status::error("Couldn't upgrade the server"));
                        return cx.notify();
                    }
                };
                this.notice = Some(notice.into());
                // This build may have connected before the upgrade returned.
                if this.incompatible().is_none() && this.view.is_some() {
                    this.upgrade_connected(cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    /// A session is shown after an upgrade: show its note (again, if the
    /// session changed) until it has been up for a moment.
    pub(crate) fn upgrade_connected(&mut self, cx: &mut Context<Self>) {
        let Some(notice) = self.notice.clone() else { return };
        let notice = Status::success(notice);
        self.status = Some(notice.clone());
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NOTICE_FOR).await;
            let _ = this.update(cx, |this, cx| {
                this.notice = None;
                if this.status.as_ref() == Some(&notice) {
                    this.status = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Card for a remote host that can't be used as is: not paired (with
    /// Pair again) or another build (nothing to do from here).
    pub(crate) fn render_remote_problem(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let _glass = crate::surface::enter();
        let host = self.active_host;
        let (title, detail, pair) = match self.hosts.get(host)?.state {
            HostState::NotPaired => ("Not paired", "This Mac is no longer paired with that machine.", true),
            HostState::Incompatible => ("Incompatible server", "Upgrade Silo on that machine.", false),
            _ => return None,
        };
        let card = card()
            .child(div().text_size(px(15.0)).font_weight(FontWeight::SEMIBOLD).child(title))
            .child(div().text_size(px(12.5)).text_color(rgb(theme::muted())).child(detail))
            .when(pair, |el| {
                el.child(
                    div()
                        .id("remote-pair-again")
                        .h(px(28.0))
                        .px(px(14.0))
                        .flex()
                        .items_center()
                        .continuous(14.0)
                        .text_size(px(12.5))
                        .font_weight(FontWeight::MEDIUM)
                        .bg(theme::control_fill())
                        .border_1()
                        .border_color(rgb(theme::border()))
                        .cursor_pointer()
                        .hover(|s| s.opacity(0.88))
                        .child("Pair Again…")
                        .on_click(cx.listener(move |this, _, window, cx| this.pair_again(host, window, cx))),
                )
            });
        Some(div().size_full().flex().items_center().justify_center().p(px(24.0)).child(card).into_any_element())
    }

    pub(crate) fn render_incompatible(&self, server: &silo_client::IncompatibleServer, cx: &mut Context<Self>) -> AnyElement {
        let _glass = crate::surface::enter();
        let upgrading = self.upgrading;
        let button = div()
            .id("incompatible-upgrade")
            .h(px(28.0))
            .px(px(14.0))
            .flex()
            .items_center()
            .justify_center()
            .continuous(14.0)
            .whitespace_nowrap()
            .text_size(px(12.5))
            .font_weight(FontWeight::MEDIUM)
            .bg(theme::control_fill())
            .text_color(rgb(theme::foreground()))
            .border_1()
            .border_color(rgb(theme::border()))
            .child(if upgrading { "Upgrading…" } else { "Upgrade Server" })
            .when(upgrading, |b| b.opacity(0.6))
            .when(!upgrading, |b| b.cursor_pointer().hover(|s| s.opacity(0.88)).on_click(cx.listener(|this, _, _, cx| this.upgrade_server(cx))));
        let detail: SharedString = match &server.build {
            Some(build) => format!("Server build {build}, this app {}.", silo_proto::BUILD).into(),
            None => format!("This app is build {}.", silo_proto::BUILD).into(),
        };
        let muted = |text: SharedString| div().text_size(px(12.5)).text_color(rgb(theme::muted())).child(text);
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p(px(24.0))
            .child(
                card()
                    .child(div().text_size(px(15.0)).font_weight(FontWeight::SEMIBOLD).child(server.message()))
                    .child(muted(silo_client::IncompatibleServer::UPGRADE_NOTE.into()))
                    .child(muted(detail))
                    .child(button)
                    .children(self.status.as_ref().map(|status| crate::status::inline(status, theme::overlay(), cx))),
            )
            .into_any_element()
    }
}
