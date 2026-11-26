//! Running cards through otto (docs/STUDIO.md §6.2): Run → `studio.run_card`, Stop → `studio.stop_card`; the
//! card's state comes back through the canvas (`studio.changed`), so nothing polls. A run refused with
//! `no_provider` shows the media setup card for that kind over the board (as the agent block's, MEDIA.md
//! §5.2): a connected provider is made the kind's default, else its key is asked for; then the card runs
//! again. Other refusals show on the card. Also otto's model schemas, read for the settings panel (the models
//! themselves are the store's, shared with the Production view's Models menu).

use std::collections::HashMap;

use gpui::{AnyElement, AppContext as _, Context, Entity, FontWeight, Subscription, div, prelude::*, px, rgb};
use otto_client::ClientError;
use otto_proto::{MediaKind, MediaModelSchema, MediaStatus};

use silo_proto::studio::{GenKind, GenStatus, NodeBody};

use super::CanvasView;
use crate::{
    agent::{
        editor::{Editor, EditorEvent},
        media_setup,
    },
    assets::icon,
    studio::{
        board::layout::{self, CardAction},
        store::Load,
    },
    theme,
};

#[derive(Default)]
pub(crate) struct Runs {
    /// Why `studio.run_card` refused, by card (shown on it until the next run).
    pub errors: HashMap<String, String>,
    pub setup: Option<Setup>,
    pub schemas: HashMap<String, Load<MediaModelSchema>>,
}

/// The media setup card: card `node` needs a `kind` provider.
pub(crate) struct Setup {
    node: String,
    kind: MediaKind,
    status: Option<Result<MediaStatus, String>>,
    /// The provider whose key is being typed: id, name, the field.
    key: Option<(String, String, Entity<Editor>, Subscription)>,
    busy: bool,
    error: Option<String>,
}

/// The kind of provider a run refused with `no_provider` needs: the refusal's `kind`, else what the card makes.
pub(super) fn setup_kind(data: Option<&serde_json::Value>, makes: Option<silo_proto::studio::GenKind>) -> MediaKind {
    data.and_then(|d| serde_json::from_value(d["kind"].clone()).ok())
        .or_else(|| makes.map(super::settings_panel::media_kind))
        .unwrap_or(MediaKind::Image)
}

/// What a card's footer button does, its label and icon; `refused`: a run the view saw refused (Retry).
pub(super) fn footer_button(body: &NodeBody, refused: bool) -> (CardAction, &'static str, &'static str) {
    match layout::card_action(body) {
        CardAction::Stop => (CardAction::Stop, "Stop", "stop"),
        CardAction::Connect => (CardAction::Connect, "Connect provider", "plug"),
        CardAction::Run if !refused => (CardAction::Run, "Run", "play"),
        CardAction::Run | CardAction::Retry => (CardAction::Retry, "Retry", "rotate-cw"),
    }
}

/// `No image provider connected` (video, audio) for a card that makes `makes`.
pub(super) fn no_provider_text(makes: GenKind) -> String {
    let kind = match super::settings_panel::media_kind(makes) {
        MediaKind::Image => "image",
        MediaKind::Video => "video",
        MediaKind::Audio => "audio",
    };
    format!("No {kind} provider connected")
}

/// What a card's face says went wrong, if anything: the view's refusal (`refused`, the newest), the missing
/// provider, or the run's error.
pub(super) fn failure_text(body: &NodeBody, refused: Option<&str>) -> Option<String> {
    let NodeBody::Gen { makes, status, error, .. } = body else { return None };
    match layout::card_action(body) {
        CardAction::Stop => None,
        _ if refused.is_some() => refused.map(str::to_owned),
        CardAction::Connect => Some(no_provider_text(*makes)),
        _ if *status == GenStatus::Failed => Some(error.clone().filter(|e| !e.trim().is_empty()).unwrap_or_else(|| "Failed".into())),
        _ => None,
    }
}

pub(super) fn noun(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Image => "an image",
        MediaKind::Video => "a video",
        MediaKind::Audio => "an audio",
    }
}

impl CanvasView {
    pub(super) fn run(&mut self, node: String, cx: &mut Context<Self>) {
        self.runs.errors.remove(&node);
        let (root, canvas) = (self.store.read(cx).root.clone(), self.name.clone());
        let n = node.clone();
        let store = self.store.clone();
        store.read(cx).caller().otto(
            cx,
            move |c| async move { c.studio_run_card(&root, &canvas, &n).await },
            move |this: &mut Self, r, cx| match r {
                Ok(_) => {}
                Err(ClientError::Rpc(e)) if e.code == "no_provider" => {
                    let makes = this.board.canvas().node(&node).and_then(|n| match &n.body {
                        silo_proto::studio::NodeBody::Gen { makes, .. } => Some(*makes),
                        _ => None,
                    });
                    this.open_setup(node, setup_kind(e.data.as_ref(), makes), cx);
                }
                Err(e) => {
                    this.runs.errors.insert(node, crate::agent::describe(&e));
                }
            },
        );
        cx.notify();
    }

    /// Run card `node` (Run, Retry), or for one refused for want of a provider (Connect provider), the media
    /// setup card for its kind first, as when a run is refused with `no_provider`: it runs once one is set.
    pub(super) fn run_or_connect(&mut self, node: String, cx: &mut Context<Self>) -> bool {
        let Some(body) = self.board.canvas().node(&node).map(|n| &n.body) else { return false };
        if layout::card_action(body) != CardAction::Connect {
            return false;
        }
        let makes = match body {
            NodeBody::Gen { makes, .. } => Some(*makes),
            _ => None,
        };
        self.open_setup(node, setup_kind(None, makes), cx);
        true
    }

    pub(super) fn stop(&mut self, node: String, cx: &mut Context<Self>) {
        let (root, canvas) = (self.store.read(cx).root.clone(), self.name.clone());
        let n = node.clone();
        let store = self.store.clone();
        store.read(cx).caller().otto(
            cx,
            move |c| async move { c.studio_stop_card(&root, &canvas, &n).await },
            move |this: &mut Self, r, cx| {
                if let Err(e) = r {
                    this.say(format!("Couldn't stop it: {}", crate::agent::describe(&e)), cx);
                }
            },
        );
    }

    fn open_setup(&mut self, node: String, kind: MediaKind, cx: &mut Context<Self>) {
        self.runs.setup = Some(Setup { node, kind, status: None, key: None, busy: false, error: None });
        let store = self.store.clone();
        store.read(cx).caller().otto(
            cx,
            |c| async move { c.media_status().await },
            |this: &mut Self, r, _| {
                if let Some(s) = this.runs.setup.as_mut() {
                    s.status = Some(r.map_err(|e| crate::agent::describe(&e)));
                }
            },
        );
        cx.notify();
    }

    /// The provider is ready: close the card and run again.
    fn setup_done(&mut self, cx: &mut Context<Self>) {
        // Models may have changed their connected state.
        self.store.update(cx, |s, _| s.forget_models());
        if let Some(s) = self.runs.setup.take() {
            // After the card's edits are saved.
            self.pump.run(s.node);
            self.release_runs(cx);
        }
    }

    /// A connected provider: make it the kind's default, then run again.
    fn use_provider(&mut self, provider: String, cx: &mut Context<Self>) {
        let Some(s) = self.runs.setup.as_mut() else { return };
        s.busy = true;
        let kind = s.kind;
        let store = self.store.clone();
        store.read(cx).caller().otto(
            cx,
            move |c| async move { c.call(otto_proto::control::Method::MediaSetDefault { kind, provider: Some(provider), model: None }).await },
            |this: &mut Self, r, cx| match r {
                Ok(_) => this.setup_done(cx),
                Err(e) => {
                    if let Some(s) = this.runs.setup.as_mut() {
                        s.busy = false;
                        s.error = Some(crate::agent::describe(&e));
                    }
                }
            },
        );
    }

    fn ask_key(&mut self, provider: String, name: String, window: &mut gpui::Window, cx: &mut Context<Self>) {
        let editor = cx.new(|cx| {
            let mut e = Editor::new(media_setup::key_placeholder(&provider), cx);
            e.mask = true;
            e.single_line = true;
            e
        });
        let sub = cx.subscribe(&editor, |this, _, e: &EditorEvent, cx| match e {
            EditorEvent::Submit => this.save_key(cx),
            EditorEvent::Escape => {
                if let Some(s) = this.runs.setup.as_mut() {
                    s.key = None;
                }
                cx.notify();
            }
            _ => cx.notify(),
        });
        editor.read(cx).focus.clone().focus(window, cx);
        if let Some(s) = self.runs.setup.as_mut() {
            s.key = Some((provider, name, editor, sub));
            s.error = None;
        }
        cx.notify();
    }

    fn save_key(&mut self, cx: &mut Context<Self>) {
        let Some(s) = self.runs.setup.as_mut().filter(|s| !s.busy) else { return };
        let Some((provider, _, editor, _)) = &s.key else { return };
        let key = editor.read(cx).text().trim().to_owned();
        if !media_setup::key_valid(provider, &key) {
            s.error = Some("That doesn't look like a key".into());
            return cx.notify();
        }
        s.busy = true;
        let p = provider.clone();
        let store = self.store.clone();
        store.read(cx).caller().otto(
            cx,
            move |c| async move { c.media_set_key(&p, Some(key)).await },
            |this: &mut Self, r, cx| match r {
                Ok(_) => this.setup_done(cx),
                Err(e) => {
                    if let Some(s) = this.runs.setup.as_mut() {
                        s.busy = false;
                        s.error = Some(crate::agent::describe(&e));
                    }
                }
            },
        );
        cx.notify();
    }
}

/// The media setup card, centred over the board.
pub(super) fn render_setup(view: &CanvasView, cx: &mut Context<CanvasView>) -> Option<AnyElement> {
    let s = view.runs.setup.as_ref()?;
    let muted = theme::muted();
    let kind = s.kind;
    let body: AnyElement = match &s.status {
        None => div().text_size(px(12.0)).text_color(rgb(muted)).child("Loading providers\u{2026}").into_any_element(),
        Some(Err(e)) => div().text_size(px(12.0)).text_color(rgb(theme::danger())).child(e.clone()).into_any_element(),
        Some(Ok(status)) => {
            let rows = status.providers.iter().filter(|p| p.kinds.contains(&kind)).enumerate().map(|(i, p)| {
                let (id, name) = (p.id.clone(), p.name.clone());
                let typing = s.key.as_ref().filter(|(k, ..)| *k == p.id);
                let button = if p.connected {
                    crate::confirm_card::button("studio-setup-use", "Use", true, false)
                        .id(("studio-setup-use", i))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, _, cx| this.use_provider(id.clone(), cx)))
                } else {
                    crate::confirm_card::button("studio-setup-key", "Add key", false, false)
                        .id(("studio-setup-key", i))
                        .cursor_pointer()
                        .on_click(cx.listener(move |this, _, window, cx| this.ask_key(id.clone(), name.clone(), window, cx)))
                };
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .child(icon(media_setup::logo(&p.id), 18.0, theme::foreground()))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_col()
                                    .child(div().font_weight(FontWeight::MEDIUM).child(p.name.clone()))
                                    .child(div().text_size(px(11.5)).text_color(rgb(muted)).child(media_setup::models_line(p, kind))),
                            )
                            .when(typing.is_none(), |el| el.child(button)),
                    )
                    .children(typing.map(|(provider, _, editor, _)| {
                        div()
                            .flex()
                            .flex_col()
                            .gap(px(4.0))
                            .child(div().px(px(8.0)).py(px(6.0)).rounded(px(6.0)).bg(theme::well_fill()).child(Editor::element(
                                editor,
                                1,
                                super::nodes::colors(),
                                cx,
                            )))
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(rgb(muted))
                                    .child(format!("{} \u{b7} \u{21a9} saves", media_setup::key_hint(provider))),
                            )
                    }))
            });
            div().flex().flex_col().gap(px(10.0)).children(rows).into_any_element()
        }
    };
    Some(
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(
                crate::confirm_card::card("studio-media-setup")
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(icon(if kind == MediaKind::Audio { "mic" } else { "image" }, 16.0, muted))
                            .child(div().font_weight(FontWeight::MEDIUM).child(format!("Connect {} provider", noun(kind)))),
                    )
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(rgb(muted))
                            .child("Cards run with your own API key; the card runs again once one is set."),
                    )
                    .child(body)
                    .children(s.error.clone().map(|e| div().text_size(px(12.0)).text_color(rgb(theme::danger())).child(e)))
                    .child(div().flex().justify_end().child(
                        crate::confirm_card::button("studio-setup-close", "Not now", false, false).cursor_pointer().on_click(cx.listener(
                            |this, _, _, cx| {
                                this.runs.setup = None;
                                cx.notify();
                            },
                        )),
                    )),
            )
            .into_any_element(),
    )
}
