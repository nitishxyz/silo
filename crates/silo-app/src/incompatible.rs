//! The card shown while a server of another version holds the local host,
//! and its Restart: stop that server through a version-independent path;
//! the connection loop then starts this version's server.

use gpui::{AnyElement, Context, FontWeight, IntoElement, SharedString, div, prelude::*, px, rgb};

use crate::{theme, theme::Continuous as _, workspace::Workspace};

impl Workspace {
    /// The active host's incompatible server, if one holds it.
    pub(crate) fn incompatible(&self) -> Option<&silo_client::Incompatible> {
        self.hosts.get(self.active_host)?.incompatible.as_ref()
    }

    /// Stop the incompatible server on the active (local) host. The
    /// connection loop notices it is gone and starts ours.
    pub(crate) fn restart_incompatible(&mut self, cx: &mut Context<Self>) {
        self.confirm_restart = false;
        let Some(server) = self.incompatible().cloned() else { return };
        let home = self.home.clone();
        self.status = Some("Restarting the server…".into());
        let task = self.rt.spawn(async move { silo_client::compat::stop(&home, &server).await });
        cx.spawn(async move |this, cx| {
            let result = task.await.map_err(anyhow::Error::from).and_then(|r| r);
            let _ = this.update(cx, |this, cx| {
                this.status = match result {
                    Ok(()) => Some("Starting the server…".into()),
                    Err(error) => {
                        tracing::warn!("restart incompatible server: {error:#}");
                        Some(format!("Could not restart the server: {error}"))
                    }
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(crate) fn render_incompatible(&self, server: &silo_client::Incompatible, cx: &mut Context<Self>) -> AnyElement {
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
        let actions = if self.confirm_restart {
            div()
                .flex()
                .gap(px(8.0))
                .child(button("incompatible-cancel", "Cancel", false).on_click(cx.listener(|this, _, _, cx| {
                    this.confirm_restart = false;
                    cx.notify();
                })))
                .child(
                    button("incompatible-confirm", "End Sessions & Restart", true)
                        .on_click(cx.listener(|this, _, _, cx| this.restart_incompatible(cx))),
                )
        } else {
            div().flex().child(button("incompatible-restart", "Restart Server…", false).on_click(cx.listener(|this, _, _, cx| {
                // Nothing to lose: restart at once.
                if this.incompatible().is_some_and(|s| s.sessions == Some(0)) {
                    this.restart_incompatible(cx);
                } else {
                    this.confirm_restart = true;
                    cx.notify();
                }
            })))
        };
        let detail: SharedString = match (&server.version, self.confirm_restart) {
            (_, true) => "All of its sessions will end. Unsaved work will be lost.".into(),
            (Some(version), false) => format!("Server version {version}, this app {}.", env!("CARGO_PKG_VERSION")).into(),
            (None, false) => format!("This app is version {}.", env!("CARGO_PKG_VERSION")).into(),
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
