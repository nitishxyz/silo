//! Media providers (`docs/MEDIA.md` §5.2, §5.4): the setup card that answers a
//! `generate_*` tool's "needs a provider" question, and the Media section of
//! Otto settings › Providers. Keys go through the sign-in key dialog.

use std::collections::HashMap;

use gpui::{AnyElement, Context, Div, FontWeight, IntoElement, SharedString, Window, div, prelude::*, px, rgb};
use otto_client::ToolState;
use otto_llm::{Item, Part};
use otto_proto::{MediaKeySource, MediaKind, MediaProviderInfo, MediaStatus, PendingQuestion};

use super::{
    AgentPane, rows,
    signin::{Kind, LOGO, ROW_BUTTON, State, short},
};
use crate::{assets::icon, theme};

/// The answer that declines (the question's last option).
pub(super) const NOT_NOW: &str = "Not now";

/// The pane's media status and the Media rows' state.
#[derive(Default)]
pub(super) struct Setup {
    pub(super) status: Option<MediaStatus>,
    /// Why `media.status` failed (shown in the card and the section).
    error: Option<String>,
    loading: bool,
    /// The setup question the status was last fetched for.
    asked: Option<String>,
    /// Provider whose row asks to confirm `Remove`.
    confirm: Option<String>,
    /// Provider whose key is being removed.
    removing: Option<String>,
    /// Provider and a short reason, shown in its row.
    row_error: Option<(String, String)>,
}

impl Setup {
    /// Esc or a click elsewhere drops a pending `Remove` ask (not one in flight).
    pub(super) fn disarm(&mut self) -> bool {
        self.removing.is_none() && self.confirm.take().is_some()
    }

    /// A provider's info changed (`media.set_key`): its row shows it at once.
    fn put(&mut self, info: MediaProviderInfo) {
        if let Some(slot) = self.status.as_mut().and_then(|s| s.providers.iter_mut().find(|p| p.id == info.id)) {
            *slot = info;
        }
    }
}

/// The media kind a tool makes: `generate_image` → image, and so on.
pub(super) fn kind_of(tool: &str) -> Option<MediaKind> {
    match tool.strip_prefix("generate_")? {
        "image" => Some(MediaKind::Image),
        "video" => Some(MediaKind::Video),
        "audio" => Some(MediaKind::Audio),
        _ => None,
    }
}

/// The kind a setup question names: `Video generation needs a provider. …` → video (`generate_media` makes any).
pub(super) fn asked_kind(question: &str) -> Option<MediaKind> {
    match question.split_once(" generation needs a provider")?.0 {
        "Image" => Some(MediaKind::Image),
        "Video" => Some(MediaKind::Video),
        "Audio" => Some(MediaKind::Audio),
        _ => None,
    }
}

/// The tool behind `call_id`: a live call's name, else the transcript's call.
pub(super) fn asking_tool<'a>(call_id: &str, tools: &'a HashMap<String, ToolState>, items: &'a [Item]) -> Option<&'a str> {
    tools.get(call_id).map(|t| t.name.as_str()).or_else(|| {
        items.iter().rev().flat_map(|i| &i.parts).find_map(|p| match p {
            Part::ToolCall { call_id: id, name, .. } if id == call_id => Some(name.as_str()),
            _ => None,
        })
    })
}

/// The provider an option (or typed answer) names: its display name or id, any case.
pub(super) fn option_provider<'a>(option: &str, providers: &'a [MediaProviderInfo]) -> Option<&'a MediaProviderInfo> {
    let option = option.trim();
    providers.iter().find(|p| p.name.eq_ignore_ascii_case(option) || p.id.eq_ignore_ascii_case(option))
}

/// The card's rows: the providers the question offers, in its order, that make `kind`.
pub(super) fn offered<'a>(q: &PendingQuestion, kind: MediaKind, providers: &'a [MediaProviderInfo]) -> Vec<&'a MediaProviderInfo> {
    let mut out: Vec<&MediaProviderInfo> = Vec::new();
    for p in q.options.iter().filter_map(|o| option_provider(o, providers)) {
        if p.kinds.contains(&kind) && !out.iter().any(|o| o.id == p.id) {
            out.push(p);
        }
    }
    out
}

/// The default model's name for `kind` (else its first), e.g. `GPT Image 2.5 Flare`.
pub(super) fn models_line(p: &MediaProviderInfo, kind: MediaKind) -> String {
    let of_kind = || p.models.iter().filter(|m| m.kind == kind);
    of_kind().find(|m| m.default).or_else(|| of_kind().next()).map(|m| m.name.clone()).unwrap_or_default()
}

/// `Images`, `Images · Video`, `Audio`.
pub(super) fn kinds_label(kinds: &[MediaKind]) -> String {
    let label = |k: &MediaKind| match k {
        MediaKind::Image => "Images",
        MediaKind::Video => "Video",
        MediaKind::Audio => "Audio",
    };
    kinds.iter().map(label).collect::<Vec<_>>().join(" · ")
}

/// A Media row's second line.
pub(super) fn status_line(p: &MediaProviderInfo) -> String {
    match p.source {
        Some(MediaKeySource::Media) => "Connected · API key".into(),
        Some(MediaKeySource::Chat) => format!("Uses your {} key", super::view::provider_name(&p.id)),
        Some(MediaKeySource::Env) => "From environment".into(),
        None => p.kinds.first().map(|k| models_line(p, *k)).filter(|l| !l.is_empty()).unwrap_or_else(|| "API key".into()),
    }
}

fn noun(kind: MediaKind) -> (&'static str, &'static str) {
    match kind {
        MediaKind::Image => ("an image", "Images are made with your own API key."),
        MediaKind::Video => ("a video", "Videos are made with your own API key."),
        MediaKind::Audio => ("an audio", "Audio is made with your own API key."),
    }
}

pub(super) fn logo(provider: &str) -> &'static str {
    match provider {
        "openai" => "provider-openai",
        "google" => "provider-google",
        "xai" => "provider-xai",
        "higgsfield" => "provider-higgsfield",
        "fal" => "provider-fal",
        "elevenlabs" => "provider-elevenlabs",
        _ => "image",
    }
}

pub(super) fn key_hint(provider: &str) -> &'static str {
    match provider {
        "openai" => "Create one on the OpenAI platform",
        "google" => "Create one in Google AI Studio",
        "xai" => "Create one in the xAI Console",
        "higgsfield" => "Paste as KEY_ID:KEY_SECRET from the Higgsfield console",
        "fal" => "Create one in the fal dashboard",
        "elevenlabs" => "Create one in ElevenLabs settings",
        _ => "Paste your API key",
    }
}

pub(super) fn key_placeholder(provider: &str) -> &'static str {
    match provider {
        "openai" => "sk-…",
        "google" => "AIza…",
        "xai" => "xai-…",
        "higgsfield" => "KEY_ID:KEY_SECRET",
        _ => "Paste an API key",
    }
}

/// A plausible key: no spaces, a sane length; OpenAI's prefix; Higgsfield's two parts.
pub(super) fn key_valid(provider: &str, key: &str) -> bool {
    let key = key.trim();
    let ok = match provider {
        "openai" => key.starts_with("sk-"),
        "higgsfield" => key.split_once(':').is_some_and(|(id, secret)| !id.is_empty() && !secret.is_empty()),
        _ => true,
    };
    ok && key.len() >= 20 && !key.contains(char::is_whitespace)
}

impl AgentPane {
    /// The kind a question's asking tool makes: such a question gets the setup card.
    pub(super) fn setup_kind(&self, q: &PendingQuestion) -> Option<MediaKind> {
        let own = self.mirror().and_then(|m| asking_tool(&q.call_id, &m.tools, &m.items));
        let child = || self.subs.waits.iter().find_map(|w| w.tools.get(&q.call_id)).map(String::as_str);
        let tool = own.or_else(child)?;
        kind_of(tool).or_else(|| (tool == super::media::ANY_TOOL).then(|| asked_kind(&q.question)).flatten())
    }

    /// Esc in an empty composer declines the shown session's own setup card.
    pub(super) fn escape_setup(&mut self, cx: &mut Context<Self>) -> bool {
        let Some((_, None, super::Card::Question(q))) = self.live_card() else { return false };
        if self.setup_kind(&q).is_none() || !self.editor.read(cx).text().trim().is_empty() {
            return false;
        }
        self.answer_question(q.question_id, NOT_NOW.into(), cx);
        true
    }

    /// Fetch `media.status` once, and again for each new setup `question`.
    pub(super) fn want_media(&mut self, question: Option<&str>, cx: &mut Context<Self>) {
        let m = &self.media_keys;
        let stale = match question {
            Some(q) => m.asked.as_deref() != Some(q),
            None => m.status.is_none() && m.error.is_none(),
        };
        if stale && !m.loading {
            if let Some(q) = question {
                self.media_keys.asked = Some(q.to_owned());
            }
            self.refresh_media(cx);
        }
    }

    /// Reload the media status (also after a chat sign-in changes, once loaded).
    pub(super) fn refresh_media(&mut self, cx: &mut Context<Self>) {
        self.media_keys.loading = true;
        self.run(
            cx,
            |c| async move { c.media_status().await },
            |this, result, cx| {
                let m = &mut this.media_keys;
                m.loading = false;
                match result {
                    Ok(status) => {
                        cx.set_global(super::mention::ModelProviders::of(&status));
                        (m.status, m.error) = (Some(status), None);
                    }
                    Err(error) => m.error = Some(short(&error)),
                }
            },
        );
    }

    pub(super) fn media_loaded(&self) -> bool {
        self.media_keys.status.is_some()
    }

    /// The media status last loaded (the `@` popup's models).
    pub(super) fn media_status(&self) -> Option<&MediaStatus> {
        self.media_keys.status.as_ref()
    }

    /// The key dialog's Save for a media provider: closes it, then answers the setup question.
    pub(super) fn save_media_key(&mut self, provider: String, name: String, key: String, question: Option<String>, cx: &mut Context<Self>) {
        let attempt = self.signin.attempt;
        let p = provider.clone();
        self.run(
            cx,
            move |c| async move { c.media_set_key(&p, Some(key)).await },
            move |this, result, cx| {
                if this.signin.attempt != attempt {
                    // Cancelled meanwhile: the key may still have been saved.
                    return this.refresh_media(cx);
                }
                match result {
                    Ok(info) => {
                        this.signin.attempt += 1;
                        this.close_dialog(cx);
                        this.key_field.update(cx, |e, cx| e.clear(cx));
                        this.media_keys.put(info);
                        this.refresh_media(cx);
                        if let Some(q) = question {
                            this.answer_setup(q, name, cx);
                        }
                    }
                    Err(error) => this.dialog_error(short(&error), false, cx),
                }
            },
        );
    }

    /// Answer setup question `question` if it still waits (a typed answer may have beaten it).
    fn answer_setup(&mut self, question: String, answer: String, cx: &mut Context<Self>) {
        if matches!(self.live_card(), Some((_, _, super::Card::Question(q))) if q.question_id == question) {
            self.answer_question(question, answer, cx);
        }
    }

    /// The confirmed `Remove`: drops the media key; a chat or environment key may still connect it.
    fn remove_media_key(&mut self, provider: String, cx: &mut Context<Self>) {
        if self.media_keys.removing.is_some() || self.media_keys.confirm.as_deref() != Some(provider.as_str()) {
            return;
        }
        self.media_keys.removing = Some(provider.clone());
        self.media_keys.row_error = None;
        let p = provider.clone();
        self.run(
            cx,
            move |c| async move { c.media_set_key(&p, None).await },
            move |this, result, _| {
                let m = &mut this.media_keys;
                (m.removing, m.confirm) = (None, None);
                match result {
                    Ok(info) => m.put(info),
                    Err(error) => m.row_error = Some((provider, short(&error))),
                }
            },
        );
        cx.notify();
    }
}

impl AgentPane {
    /// The setup card's body (`card` already carries the sub-agent label): header,
    /// one row per offered provider, then the typed-answer hint and `Not now`.
    pub(super) fn render_setup(&mut self, card: Div, q: &PendingQuestion, kind: MediaKind, active: bool, cx: &mut Context<Self>) -> Div {
        self.want_media(Some(&q.question_id), cx);
        let muted = theme::muted();
        let (noun, line) = noun(kind);
        let header = div()
            .flex()
            .items_start()
            .gap(px(8.0))
            .child(div().flex_none().h(px(20.0)).flex().items_center().child(icon(
                if kind == MediaKind::Audio { "mic" } else { "image" },
                16.0,
                muted,
            )))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(div().h(px(20.0)).flex().items_center().font_weight(FontWeight::MEDIUM).child(format!("Connect {noun} provider")))
                    .child(div().text_size(px(12.0)).text_color(rgb(muted)).child(line)),
            );
        let enabled = active && !self.dialog_open();
        let providers = self.media_keys.status.as_ref().map(|s| offered(q, kind, &s.providers).into_iter().cloned().collect::<Vec<_>>());
        let list: AnyElement = match (providers, &self.media_keys.error) {
            (Some(list), _) => div()
                .flex()
                .flex_col()
                .children(list.into_iter().enumerate().map(|(i, p)| {
                    let (id, name, question) = (p.id.clone(), p.name.clone(), q.question_id.clone());
                    let button = if p.connected {
                        self.button(
                            format!("media-use-{i}"),
                            Kind::Primary,
                            "Use",
                            ROW_BUTTON,
                            State { enabled, loading: false },
                            cx,
                            move |this, _, cx| {
                                this.answer_setup(question.clone(), name.clone(), cx);
                            },
                        )
                    } else {
                        self.button(
                            format!("media-add-{i}"),
                            Kind::Outline,
                            "Add key",
                            ROW_BUTTON,
                            State { enabled, loading: false },
                            cx,
                            move |this, _, cx| {
                                this.open_media_key(id.clone(), name.clone(), Some(question.clone()), cx);
                            },
                        )
                    };
                    let models = models_line(&p, kind);
                    logo_row(
                        ("media-setup-row", i),
                        &p.id,
                        div().font_weight(FontWeight::MEDIUM).child(p.name.clone()),
                        div().child(models),
                        button.into_any_element(),
                    )
                }))
                .into_any_element(),
            (None, Some(error)) => div().text_size(px(12.0)).text_color(rgb(theme::danger())).child(error.clone()).into_any_element(),
            (None, None) => div().text_size(px(12.0)).text_color(rgb(muted)).child("Loading providers…").into_any_element(),
        };
        let question = q.question_id.clone();
        let not_now =
            self.button("media-not-now", Kind::Quiet, NOT_NOW, ROW_BUTTON, State { enabled: active, loading: false }, cx, move |this, _, cx| {
                this.answer_question(question.clone(), NOT_NOW.into(), cx);
            });
        let footer = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(div().flex_1().min_w_0().text_size(px(11.5)).text_color(rgb(muted)).child("Or type an answer below"))
            .child(not_now);
        card.child(header).child(list).child(footer)
    }
}

/// A list row with the provider's logo glyph on the column's left edge and
/// its trailing buttons on the right edge (like the chat providers' rows).
fn logo_row(id: impl Into<gpui::ElementId>, provider: &str, title: impl IntoElement, second: Div, trailing: AnyElement) -> AnyElement {
    let inset = (rows::ICON_COL - LOGO) / 2.0;
    let row = rows::row(
        id,
        false,
        Some(icon(logo(provider), LOGO, theme::foreground()).into_any_element()),
        title,
        Some(second.into_any_element()),
        Some(div().ml(px(12.0)).child(trailing).into_any_element()),
    )
    .pl_0()
    .pr(px(inset));
    div().mx(px(-inset)).child(row).into_any_element()
}

impl AgentPane {
    /// Otto settings › Providers' Media section, below the chat providers.
    pub(super) fn media_section(&mut self, cx: &mut Context<Self>) -> AnyElement {
        self.want_media(None, cx);
        let muted = theme::muted();
        let providers = self.media_keys.status.as_ref().map(|s| s.providers.clone()).unwrap_or_default();
        let count = providers.len();
        let mut list = div().w_full().flex().flex_col();
        for (i, p) in providers.iter().enumerate() {
            list = list.child(self.media_row(i, p, cx));
            if i + 1 < count {
                list = list.child(div().w_full().h(px(1.0)).bg(theme::divider()));
            }
        }
        let note = match (&self.media_keys.error, count) {
            (Some(error), _) => Some(div().text_color(rgb(theme::danger())).child(error.clone())),
            (None, 0) => Some(div().text_color(rgb(muted)).child("Loading…")),
            _ => None,
        };
        div()
            .w_full()
            .mt(px(28.0))
            .flex()
            .flex_col()
            .child(div().text_size(px(13.0)).font_weight(FontWeight::MEDIUM).child("Media"))
            .child(div().mt(px(2.0)).mb(px(8.0)).text_size(px(12.5)).text_color(rgb(muted)).child("Images, video and audio, with your own API keys"))
            .child(list)
            .children(note.map(|n| n.mt(px(6.0)).text_size(px(12.0))))
            .into_any_element()
    }

    fn media_row(&mut self, i: usize, p: &MediaProviderInfo, cx: &mut Context<Self>) -> AnyElement {
        let m = &self.media_keys;
        let asking = m.confirm.as_deref() == Some(p.id.as_str());
        let removing = m.removing.as_deref() == Some(p.id.as_str());
        let locked = m.removing.is_some() || self.dialog_open();
        let error = m.row_error.as_ref().filter(|(id, _)| *id == p.id).map(|(_, e)| e.clone());
        let state = |mine: bool| State { enabled: !locked || mine, loading: mine };
        let second = match (&error, p.connected) {
            _ if asking => div().text_color(rgb(theme::foreground())).child(format!("Remove your {} key?", p.name)),
            (Some(e), _) => div().text_color(rgb(theme::danger())).child(e.clone()),
            (None, true) => {
                div().flex().items_center().gap(px(6.0)).child(div().size(px(6.0)).rounded_full().bg(rgb(theme::success()))).child(status_line(p))
            }
            (None, false) => div().child(status_line(p)),
        };
        let (id, name) = (p.id.clone(), p.name.clone());
        let add = {
            let (id, name) = (id.clone(), name.clone());
            move |this: &mut Self, _: &mut Window, cx: &mut Context<Self>| {
                this.media_keys.row_error = None;
                this.media_keys.confirm = None;
                this.open_media_key(id.clone(), name.clone(), None, cx);
            }
        };
        let actions = if asking {
            let id = id.clone();
            div()
                .flex()
                .gap(px(6.0))
                .child(self.button(
                    format!("media-remove-cancel-{i}"),
                    Kind::Outline,
                    "Cancel",
                    ROW_BUTTON,
                    State { enabled: !removing, loading: false },
                    cx,
                    |this, _, cx| {
                        this.media_keys.disarm();
                        cx.notify();
                    },
                ))
                .child(self.button(
                    format!("media-remove-confirm-{i}"),
                    Kind::Danger,
                    "Remove",
                    ROW_BUTTON,
                    state(removing),
                    cx,
                    move |this, _, cx| {
                        this.remove_media_key(id.clone(), cx);
                    },
                ))
                .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                    if this.media_keys.disarm() {
                        cx.notify();
                    }
                }))
        } else if p.source == Some(MediaKeySource::Media) {
            div()
                .flex()
                .gap(px(6.0))
                .child(self.button(format!("media-change-{i}"), Kind::Quiet, "Change key", ROW_BUTTON, state(false), cx, add))
                .child(self.button(format!("media-remove-{i}"), Kind::Quiet, "Remove", ROW_BUTTON, state(false), cx, move |this, _, cx| {
                    if this.media_keys.removing.is_none() {
                        this.media_keys.confirm = Some(id.clone());
                        this.media_keys.row_error = None;
                    }
                    cx.notify();
                }))
        } else {
            div().child(self.button(format!("media-key-{i}"), Kind::Outline, "Add key", ROW_BUTTON, state(false), cx, add))
        };
        let title = div()
            .flex()
            .items_baseline()
            .gap(px(6.0))
            .child(div().font_weight(FontWeight::MEDIUM).child(name))
            .child(div().text_size(px(11.5)).text_color(rgb(theme::muted())).child(SharedString::from(kinds_label(&p.kinds))));
        logo_row(("media-row", i), &p.id, title, second, actions.into_any_element())
    }
}

#[cfg(test)]
mod tests {
    use otto_llm::{CallKind, Role};
    use otto_proto::MediaModel;

    use super::*;

    fn provider(id: &str, name: &str, kinds: Vec<MediaKind>, source: Option<MediaKeySource>) -> MediaProviderInfo {
        let models = vec![
            MediaModel { id: format!("{id}-a"), kind: MediaKind::Image, name: format!("{name} A"), handle: String::new(), default: false },
            MediaModel { id: format!("{id}-b"), kind: MediaKind::Image, name: format!("{name} B"), handle: String::new(), default: true },
        ];
        MediaProviderInfo { id: id.into(), name: name.into(), kinds, connected: source.is_some(), source, models }
    }

    fn all() -> Vec<MediaProviderInfo> {
        let image = || vec![MediaKind::Image];
        vec![
            provider("openai", "OpenAI", image(), None),
            provider("google", "Gemini", vec![MediaKind::Image, MediaKind::Video], None),
            provider("xai", "xAI", image(), None),
            provider("higgsfield", "Higgsfield", image(), None),
            provider("fal", "fal", image(), None),
            provider("elevenlabs", "ElevenLabs", vec![MediaKind::Audio], None),
        ]
    }

    fn question(options: &[&str]) -> PendingQuestion {
        PendingQuestion {
            question_id: "q1".into(),
            call_id: "c1".into(),
            question: "Image generation needs a provider. Connect one to continue.".into(),
            options: options.iter().map(|o| (*o).to_owned()).collect(),
        }
    }

    #[test]
    fn setup_questions_come_from_generate_tools() {
        let mut tools = HashMap::new();
        tools.insert("c1".to_owned(), ToolState { name: "generate_image".into(), ..ToolState::default() });
        assert_eq!(asking_tool("c1", &tools, &[]), Some("generate_image"));
        // Reloaded: only the transcript's call names it.
        let call = Part::ToolCall {
            call_id: "c2".into(),
            name: "generate_video".into(),
            input: serde_json::json!({}),
            kind: CallKind::Function,
            opaque: None,
        };
        let items = vec![Item::new("i1", Role::Assistant, vec![call])];
        assert_eq!(asking_tool("c2", &tools, &items), Some("generate_video"));
        assert_eq!(asking_tool("c3", &tools, &items), None);
        assert_eq!(kind_of("generate_image"), Some(MediaKind::Image));
        assert_eq!(kind_of("generate_video"), Some(MediaKind::Video));
        assert_eq!(kind_of("generate_audio"), Some(MediaKind::Audio));
        for other in ["ask_user", "generate_", "generate_text", "image"] {
            assert_eq!(kind_of(other), None, "{other}");
        }
        // generate_media makes any kind: its question says which.
        assert_eq!(asked_kind("Video generation needs a provider. Connect one to continue."), Some(MediaKind::Video));
        assert_eq!(asked_kind("Audio generation needs a provider. Connect one to continue."), Some(MediaKind::Audio));
        assert_eq!(asked_kind("Which one?"), None);
    }

    #[test]
    fn options_map_to_providers_by_name_or_id() {
        let providers = all();
        assert_eq!(option_provider("Gemini", &providers).map(|p| p.id.as_str()), Some("google"));
        assert_eq!(option_provider(" google ", &providers).map(|p| p.id.as_str()), Some("google"));
        assert_eq!(option_provider("XAI", &providers).map(|p| p.id.as_str()), Some("xai"));
        assert!(option_provider(NOT_NOW, &providers).is_none());
        let q = question(&["OpenAI", "Gemini", "xAI", "Higgsfield", "fal", "ElevenLabs", "Not now"]);
        let shown: Vec<&str> = offered(&q, MediaKind::Image, &providers).iter().map(|p| p.id.as_str()).collect();
        assert_eq!(shown, ["openai", "google", "xai", "higgsfield", "fal"], "the question's order, image makers only");
        let q = question(&["fal", "OpenAI", "openai", "Not now"]);
        let shown: Vec<&str> = offered(&q, MediaKind::Image, &providers).iter().map(|p| p.id.as_str()).collect();
        assert_eq!(shown, ["fal", "openai"]);
        assert_eq!(models_line(&providers[0], MediaKind::Image), "OpenAI B", "the default model");
        assert_eq!(models_line(&providers[5], MediaKind::Audio), "");
    }

    #[test]
    fn status_lines_follow_the_key_source() {
        let line = |id, name, source| status_line(&provider(id, name, vec![MediaKind::Image], source));
        assert_eq!(line("fal", "fal", Some(MediaKeySource::Media)), "Connected · API key");
        assert_eq!(line("openai", "OpenAI", Some(MediaKeySource::Chat)), "Uses your OpenAI key");
        assert_eq!(line("xai", "xAI", Some(MediaKeySource::Env)), "From environment");
        assert_eq!(line("fal", "fal", None), "fal B");
        let mut eleven = provider("elevenlabs", "ElevenLabs", vec![MediaKind::Audio], None);
        eleven.models.clear();
        assert_eq!(status_line(&eleven), "API key");
        assert_eq!(kinds_label(&[MediaKind::Image]), "Images");
        assert_eq!(kinds_label(&[MediaKind::Image, MediaKind::Video]), "Images · Video");
        assert_eq!(kinds_label(&[MediaKind::Audio]), "Audio");
    }

    #[test]
    fn keys_logos_and_hints() {
        assert!(key_valid("higgsfield", "0123456789ab:secretsecretsecret"));
        assert!(!key_valid("higgsfield", "0123456789abcdefsecretsecret"), "needs both parts");
        assert!(!key_valid("higgsfield", ":secretsecretsecretsecret"));
        assert!(key_valid("openai", "sk-proj-abcdefghijklmnopqrstu"));
        assert!(!key_valid("openai", "abcdefghijklmnopqrstuvwxyz"));
        assert!(!key_valid("fal", "short"));
        for p in all() {
            assert!(crate::assets::has_icon(logo(&p.id)), "{}", p.id);
            assert_ne!(logo(&p.id), "image", "{}", p.id);
            assert!(!key_hint(&p.id).contains('/'), "{}", p.id);
        }
        assert!(key_hint("higgsfield").contains("KEY_ID:KEY_SECRET"));
    }
}
