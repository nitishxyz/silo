//! The card shown while a server of a different build holds the local
//! host, and its Upgrade: this build's server takes over the running
//! one live, keeping every session.

use gpui::{AnyElement, Context, FontWeight, IntoElement, SharedString, div, prelude::*, px, rgb};

use crate::{theme, theme::Continuous as _, workspace::Workspace};

impl Workspace {
    /// The active host's incompatible server, if one holds it.
    pub(crate) fn incompatible(&self) -> Option<&silo_client::DifferentBuild> {
        self.hosts.get(self.active_host)?.incompatible.as_ref()
    }

    /// Upgrade the incompatible server on the active (local) host in place.
    pub(crate) fn upgrade_server(&mut self, cx: &mut Context<Self>) {
        let Some(server) = self.incompatible().cloned() else { return };
        let home = self.home.clone();
        self.status = Some("Upgrading the server…".into());
        let exe = crate::conn::server_exe();
        let task = self.rt.spawn(async move { silo_client::mismatch::upgrade(&home, &exe, &server).await });
        cx.spawn(async move |this, cx| {
            let result = task.await.map_err(anyhow::Error::from).and_then(|r| r);
            let _ = this.update(cx, |this, cx| {
                this.status = match result {
                    Ok(()) => Some("Reconnecting…".into()),
                    Err(error) => {
                        tracing::warn!("upgrade server: {error:#}");
                        Some("Could not upgrade the server.".into())
                    }
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(crate) fn render_incompatible(&self, server: &silo_client::DifferentBuild, cx: &mut Context<Self>) -> AnyElement {
        let button = |id: &'static str, label: &'static str, danger: bool| {
            let (bg, fg) = if danger { (theme::danger(), 0x00ff_ffff) } else { (theme::tab_active(), theme::foreground()) };
            div()
                .id(id)
                .h(px(28.0))
                .px(px(14.0))
                .flex()
                .items_center()
                .justify_center()
                .continuous(14.0)
                .whitespace_nowrap()
                .text_size(px(12.5))
                .font_weight(FontWeight::MEDIUM)
                .bg(rgb(bg))
                .text_color(rgb(fg))
                .when(!danger, |b| b.border_1().border_color(rgb(theme::border())))
                .cursor_pointer()
                .hover(|s| s.opacity(0.88))
                .child(label)
        };
        let actions = div().flex().child(
            button("incompatible-upgrade", "Upgrade Server", false)
                .on_click(cx.listener(|this, _, _, cx| this.upgrade_server(cx))),
        );
        let detail: SharedString = match &server.build {
            Some(build) => format!("Server build {build}, this app {}.", silo_proto::BUILD).into(),
            None => format!("This app is build {}.", silo_proto::BUILD).into(),
        };
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p(px(24.0))
            .child(
                div()
                    .max_w(px(460.0))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(12.0))
                    .text_center()
                    .child(div().text_size(px(15.0)).font_weight(FontWeight::SEMIBOLD).child(server.message()))
                    .child(div().text_size(px(12.5)).text_color(rgb(theme::muted())).child(detail))
                    .child(actions)
                    .when_some(self.status.clone(), |el, status| {
                        el.child(div().text_size(px(12.0)).text_color(rgb(theme::muted())).child(status))
                    }),
            )
            .into_any_element()
    }
}
