//! Media faces, drawn at the size their caller gives (no layout of their own): image, video (poster, duration,
//! play badge), audio (the compact player), entity (sheet and name). The board (`super::canvas`) draws cards,
//! notes and frames around them at the rects it lays out. Faces use the agent block's tile look
//! (`media::frame`). Clicks are the caller's; the audio player acts on the [`Store`].

use std::time::Instant;

use gpui::{AnyElement, Context, Entity, ObjectFit, Window, div, img, prelude::*, px, rgb};
use silo_proto::studio::{GenKind, Section};

use super::{
    model,
    store::{Asset, AssetKind, Store},
};
use crate::{
    agent::media::{self, PULSE},
    assets::icon,
    theme,
    tooltip::Tooltip,
};

/// The audio player's height.
pub const AUDIO_H: f32 = 56.0;

/// The foreground over the background at `amount`.
pub(crate) fn ink(amount: f32) -> u32 {
    theme::mix(theme::background(), theme::foreground(), amount)
}

/// The waiting pulse's phase now (`None` with Reduce Motion); keeps frames coming while shown.
pub fn pulse(store: &Store, window: &mut Window, cx: &gpui::App) -> Option<f32> {
    if cx.reduce_motion() {
        return None;
    }
    window.request_animation_frame();
    Some(Instant::now().duration_since(store.epoch).as_secs_f32() % PULSE / PULSE)
}

#[must_use]
pub fn kind_glyph(makes: GenKind) -> &'static str {
    match makes {
        GenKind::Image => "image",
        GenKind::Video => "video",
        GenKind::Speech | GenKind::Sound | GenKind::Music => "audio-lines",
        GenKind::Unknown => "sparkles",
    }
}

/// A centered note in a face: muted, small, clipped to `w`.
fn note(text: impl Into<gpui::SharedString>, w: f32) -> gpui::Div {
    div()
        .max_w(px((w - 20.0).max(20.0)))
        .overflow_hidden()
        .whitespace_nowrap()
        .text_ellipsis()
        .text_size(px(11.5))
        .text_color(rgb(theme::muted()))
        .child(text.into())
}

/// Image `path`, covering `w`×`h`; a pulse while it comes.
pub fn image<V: 'static>(store: &Entity<Store>, path: &str, w: f32, h: f32, window: &mut Window, cx: &mut Context<V>) -> AnyElement {
    let asset = store.update(cx, |s, cx| s.asset(path, AssetKind::Image, cx));
    match asset {
        Asset::Image(l) => {
            media::frame(w, h, None).child(img(l.preview.clone()).absolute().size_full().object_fit(ObjectFit::Cover)).into_any_element()
        }
        Asset::Loading => {
            let p = pulse(store.read(cx), window, cx);
            media::frame(w, h, p).child(icon("image", 16.0, theme::muted())).into_any_element()
        }
        _ => media::frame(w, h, None).child(icon("image", 16.0, theme::muted())).child(note("Image unavailable", w)).into_any_element(),
    }
}

/// Video `path`: its poster frame, the duration and a play badge.
pub fn video<V: 'static>(
    store: &Entity<Store>,
    path: &str,
    seconds: Option<f64>,
    w: f32,
    h: f32,
    window: &mut Window,
    cx: &mut Context<V>,
) -> AnyElement {
    let asset = store.update(cx, |s, cx| s.asset(path, AssetKind::Video, cx));
    let badge = div()
        .absolute()
        .size(px(34.0))
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui::rgba(0x0000_0080))
        .child(icon("play", 14.0, 0xffffff).ml(px(1.5)));
    #[allow(clippy::cast_precision_loss)]
    let length = |ms: Option<u64>| seconds.or(ms.map(|ms| ms as f64 / 1000.0)).map(model::seconds);
    let time = |t: String| {
        div()
            .absolute()
            .right(px(6.0))
            .bottom(px(6.0))
            .px(px(5.0))
            .rounded(px(4.0))
            .bg(gpui::rgba(0x0000_0099))
            .text_size(px(11.0))
            .text_color(rgb(0xffffff))
            .child(t)
    };
    match asset {
        Asset::Clip(c) => media::frame(w, h, None)
            .children(c.poster.as_ref().map(|(p, _)| img(p.clone()).absolute().size_full().object_fit(ObjectFit::Cover)))
            .child(badge)
            .children(length(c.ms).map(time))
            .into_any_element(),
        Asset::Loading => {
            let p = pulse(store.read(cx), window, cx);
            media::frame(w, h, p).child(icon("video", 16.0, theme::muted())).into_any_element()
        }
        _ => media::frame(w, h, None).child(icon("video", 16.0, theme::muted())).child(note("Video unavailable", w)).into_any_element(),
    }
}

/// Audio `path`: the compact player (play/pause, its name, progress, elapsed / duration), as the agent block's.
pub fn audio<V: 'static>(store: &Entity<Store>, path: &str, w: f32, window: &mut Window, cx: &mut Context<V>) -> AnyElement {
    let asset = store.update(cx, |s, cx| s.asset(path, AssetKind::Audio, cx));
    let (muted, fg) = (theme::muted(), theme::foreground());
    let row = |p| media::frame(w, AUDIO_H, p).flex_row().justify_start().gap(px(10.0)).px(px(12.0));
    let clip = match asset {
        Asset::Clip(c) => c,
        Asset::Loading => {
            let p = pulse(store.read(cx), window, cx);
            return row(p).child(icon("audio-lines", 16.0, muted)).into_any_element();
        }
        _ => return row(None).child(icon("audio-lines", 16.0, muted)).child(note("Audio unavailable", w)).into_any_element(),
    };
    let s = store.read(cx);
    let current = s.audio.as_ref().filter(|a| a.path == path);
    let playing = current.is_some_and(|a| a.player.playing());
    if playing {
        window.request_animation_frame();
    }
    #[allow(clippy::cast_precision_loss)]
    let duration = current.and_then(|a| a.player.duration()).or(clip.ms.map(|ms| ms as f64 / 1000.0));
    let elapsed = current.map_or(0.0, |a| a.player.time());
    #[allow(clippy::cast_possible_truncation)]
    let done = duration.filter(|d| *d > 0.0).map_or(0.0, |d| (elapsed / d).clamp(0.0, 1.0)) as f32;
    let name = path.rsplit('/').next().unwrap_or(path).to_owned();
    let toggle = {
        let (store, path, clip) = (store.clone(), path.to_owned(), clip.clone());
        div()
            .id(gpui::ElementId::Name(format!("studio-audio-{path}").into()))
            .flex_none()
            .size(px(32.0))
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(ink(0.1)))
            .hover(|s| s.bg(rgb(ink(0.16))))
            .cursor_pointer()
            .child(icon(if playing { "pause" } else { "play" }, 13.0, fg).when(!playing, |i| i.ml(px(1.5))))
            .tooltip(Tooltip::text(if playing { "Pause" } else { "Play" }))
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                store.update(cx, |s, cx| s.audio_toggle(&path, &clip, cx));
            })
    };
    let bar = {
        let (store, path, clip) = (store.clone(), path.to_owned(), clip.clone());
        let bounds = std::rc::Rc::new(std::cell::Cell::new(gpui::Bounds::<gpui::Pixels>::default()));
        let probe = bounds.clone();
        div()
            .id(gpui::ElementId::Name(format!("studio-audio-bar-{path}").into()))
            .relative()
            .h(px(14.0))
            .cursor_pointer()
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .top(px(5.0))
                    .h(px(4.0))
                    .rounded_full()
                    .bg(rgb(ink(0.14)))
                    .child(div().h_full().w(gpui::relative(done)).rounded_full().bg(rgb(fg))),
            )
            .child(gpui::canvas(move |b, _, _| probe.set(b), |_, _, _, _| {}).absolute().size_full())
            .on_click(move |e, _, cx| {
                cx.stop_propagation();
                let b = bounds.get();
                let at = crate::agent::audio::fraction(f32::from(e.position().x), f32::from(b.origin.x), f32::from(b.size.width));
                store.update(cx, |s, cx| s.audio_seek(&path, &clip, at, cx));
            })
    };
    let top = div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(13.0)).text_color(rgb(fg)).child(name))
        .child(div().flex_none().text_size(px(12.0)).text_color(rgb(muted)).child(crate::agent::audio::times(elapsed, duration)));
    row(None).child(toggle).child(div().flex_1().min_w_0().flex().flex_col().gap(px(5.0)).child(top).child(bar)).into_any_element()
}

/// An entity: its sheet (a placeholder without one) and its name under it.
pub fn entity<V: 'static>(
    store: &Entity<Store>,
    sections: &[Section],
    id: &str,
    w: f32,
    h: f32,
    window: &mut Window,
    cx: &mut Context<V>,
) -> AnyElement {
    let info = sections.iter().flat_map(|s| &s.entities).find(|e| e.id == id).cloned();
    let name = info.as_ref().map_or_else(|| id.to_owned(), |e| e.name.clone());
    let label_h = 24.0;
    let face = match info.as_ref().and_then(|e| e.sheet.clone()) {
        Some(sheet) => image(store, &sheet, w, h - label_h, window, cx),
        None => media::frame(w, h - label_h, None).child(icon("user", 18.0, theme::muted())).into_any_element(),
    };
    div()
        .flex()
        .flex_col()
        .w(px(w))
        .h(px(h))
        .child(face)
        .child(div().h(px(label_h)).flex().items_center().overflow_hidden().whitespace_nowrap().text_ellipsis().text_size(px(12.5)).child(name))
        .into_any_element()
}
