//! Generated audio rows (`docs/MEDIA.md` §6.4): a compact player per file (play/pause, name, a progress bar to
//! click or drag, elapsed / duration, Save… on hover), played through `AVPlayer` (`av.rs`) without a view. One
//! plays at a time; it stops with the pane, or once its session isn't shown.

use gpui::{AnyElement, ClickEvent, Context, Window, div, prelude::*, px, rgb};

use super::{
    AgentPane, av,
    media::{self, Clip, Entry, Media, Shelf},
    view::ink,
};
use crate::{assets::icon, theme, tooltip::Tooltip};

/// The file playing (or paused where it was), by `session/attachment`.
pub(super) struct Playing {
    pub key: String,
    pub player: av::Player,
    /// The bar is being dragged.
    pub drag: bool,
}

/// The row's widest and its height.
const MAX_W: f32 = 420.0;
const ROW_H: f32 = 56.0;

/// `elapsed / duration` (`0:04 / 0:12`), the duration left out while unknown.
#[must_use]
pub fn times(elapsed: f64, duration: Option<f64>) -> String {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let ms = |s: f64| (s.max(0.0) * 1000.0) as u64;
    match duration {
        Some(d) => format!("{} / {}", media::clock(ms(elapsed.min(d))), media::clock(ms(d))),
        None => media::clock(ms(elapsed)),
    }
}

/// Where a press at `x` falls on a bar from `left`, `width` wide (0–1).
#[must_use]
pub fn fraction(x: f32, left: f32, width: f32) -> f64 {
    if width <= 0.0 { 0.0 } else { f64::from(((x - left) / width).clamp(0.0, 1.0)) }
}

impl AgentPane {
    /// Audio plays (or its bar is dragged): the pane redraws at its ambient rate meanwhile.
    pub(super) fn audio_playing(&self) -> bool {
        self.audio.as_ref().is_some_and(|a| a.drag || a.player.playing())
    }

    /// The player goes once its session isn't the one shown.
    pub(super) fn audio_check(&mut self) {
        let shown = self.session_id.as_deref().map(|s| format!("{s}/"));
        if self.audio.as_ref().is_some_and(|a| shown.as_deref().is_none_or(|s| !a.key.starts_with(s))) {
            self.audio = None;
        }
    }

    /// The player on `clip`, made current (the other one stops).
    fn audio_on(&mut self, key: &str, clip: &Clip) -> Option<&Playing> {
        if self.audio.as_ref().is_none_or(|a| a.key != key) {
            self.audio = None;
            self.audio = Some(Playing { key: key.to_owned(), player: av::Player::new(&clip.file)?, drag: false });
        }
        self.audio.as_ref()
    }

    fn audio_toggle(&mut self, key: &str, clip: &Clip, cx: &mut Context<Self>) {
        if let Some(a) = self.audio_on(key, clip) {
            if a.player.playing() { a.player.pause() } else { a.player.play() }
        }
        cx.notify();
    }

    /// Seek `clip` to `at` (0–1) of `ms` (the line's length when the player doesn't know it yet).
    fn audio_seek(&mut self, key: &str, clip: &Clip, ms: Option<u64>, at: f64, cx: &mut Context<Self>) {
        #[allow(clippy::cast_precision_loss)]
        let known = ms.map(|ms| ms as f64 / 1000.0);
        if let Some(a) = self.audio_on(key, clip)
            && let Some(d) = a.player.duration().or(known)
        {
            a.player.seek(at * d);
        }
        cx.notify();
    }

    /// The save panel (Downloads, the file's name), then the cached file copied there; `None` when cancelled.
    pub(super) fn save_clip(&self, name: &str, file: std::path::PathBuf, cx: &mut Context<Self>) -> gpui::Task<Option<bool>> {
        let chosen = cx.prompt_for_new_path(&silo_web::downloads_dir(), Some(name));
        cx.spawn(async move |_, cx| {
            let path = chosen.await.ok().and_then(Result::ok).flatten()?;
            Some(cx.background_executor().spawn(async move { std::fs::copy(&file, &path).is_ok() }).await)
        })
    }
}

/// The row's frame: the tiles' look, `w` wide.
fn row(w: f32, pulse: Option<f32>) -> gpui::Div {
    media::frame(w, ROW_H, pulse).flex_row().justify_start().gap(px(10.0)).px(px(12.0))
}

impl AgentPane {
    /// While the call runs: the row's frame pulsing, with the time and the provider's word.
    pub(super) fn render_audio_pending(&self, shelf: &Shelf, avail: f32, pulse: Option<f32>) -> AnyElement {
        let muted = theme::muted();
        let elapsed = self.pending_clock(shelf);
        let line = |el: gpui::Div| el.min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis();
        row(avail.min(MAX_W), pulse)
            .child(icon("audio-lines", 16.0, muted))
            .child(line(div().flex_1()).text_size(px(12.0)).text_color(rgb(muted)).children(shelf.progress.clone()))
            .children(elapsed.map(|t| div().flex_none().text_size(px(12.0)).text_color(rgb(muted)).font_features(super::view::tabular()).child(t)))
            .into_any_element()
    }

    /// A file's player row: a pulse while its bytes come, `Audio unavailable` if they don't.
    pub(super) fn render_audio(&mut self, m: &Media, avail: f32, pulse: Option<f32>, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let (muted, fg) = (theme::muted(), theme::foreground());
        let w = avail.min(MAX_W);
        let clip = match self.media(m, cx) {
            Entry::Clip(c) => c,
            Entry::Loading => {
                if pulse.is_some() {
                    window.request_animation_frame();
                }
                return row(w, pulse).child(icon("audio-lines", 16.0, muted)).into_any_element();
            }
            _ => {
                let note = div().text_size(px(12.0)).text_color(rgb(muted)).child("Audio unavailable");
                return row(w, None).child(icon("audio-lines", 16.0, muted)).child(note).into_any_element();
            }
        };
        let Some(session) = self.session_id.as_deref() else { return div().into_any_element() };
        let key = format!("{session}/{}", m.id);
        let current = self.audio.as_ref().filter(|a| a.key == key);
        let playing = current.is_some_and(|a| a.player.playing());
        #[allow(clippy::cast_precision_loss)]
        let duration = current.and_then(|a| a.player.duration()).or(m.ms.or(clip.ms).map(|ms| ms as f64 / 1000.0));
        let elapsed = current.map_or(0.0, |a| a.player.time());
        #[allow(clippy::cast_possible_truncation)]
        let done = duration.filter(|d| *d > 0.0).map_or(0.0, |d| (elapsed / d).clamp(0.0, 1.0)) as f32;
        let ms = m.ms.or(clip.ms);
        let group: gpui::SharedString = format!("audio-{}", m.id).into();
        let toggle = {
            let (key, clip) = (key.clone(), clip.clone());
            div()
                .id(gpui::ElementId::Name(format!("audio-play-{}", m.id).into()))
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
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.audio_toggle(&key, &clip, cx)))
        };
        let save = {
            let (name, file) = (m.name.clone(), clip.file.clone());
            div()
                .id(gpui::ElementId::Name(format!("audio-save-{}", m.id).into()))
                .flex_none()
                .px(px(6.0))
                .rounded(px(5.0))
                .text_size(px(12.0))
                .text_color(rgb(muted))
                .cursor_pointer()
                .opacity(0.0)
                .group_hover(group.clone(), |s| s.opacity(1.0))
                .hover(|s| s.bg(rgb(ink(0.1))).text_color(rgb(fg)))
                .child("Save…")
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.save_clip(&name, file.clone(), cx).detach()))
        };
        let top = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(13.0))
                    .text_color(rgb(fg))
                    .child(m.name.clone()),
            )
            .child(save)
            .child(
                div().flex_none().text_size(px(12.0)).text_color(rgb(muted)).font_features(super::view::tabular()).child(times(elapsed, duration)),
            );
        row(w, None)
            .group(group)
            .child(toggle)
            .child(div().flex_1().min_w_0().flex().flex_col().gap(px(5.0)).child(top).child(self.audio_bar(key, clip, ms, done, cx)))
            .into_any_element()
    }
}

impl AgentPane {
    /// The progress bar: a press seeks there and a drag follows the pointer (anywhere, until released).
    fn audio_bar(&self, key: String, clip: std::sync::Arc<Clip>, ms: Option<u64>, done: f32, cx: &mut Context<Self>) -> AnyElement {
        let entity = cx.entity().downgrade();
        let dragging = self.audio.as_ref().is_some_and(|a| a.key == key && a.drag);
        let track = div()
            .absolute()
            .left_0()
            .right_0()
            .top(px(5.0))
            .h(px(4.0))
            .rounded_full()
            .bg(rgb(ink(0.14)))
            .child(div().h_full().w(gpui::relative(done)).rounded_full().bg(rgb(theme::foreground())));
        let input = gpui::canvas(
            |bounds, window, _| window.insert_hitbox(bounds, gpui::HitboxBehavior::Normal),
            move |bounds, hitbox, window, _| {
                window.set_cursor_style(gpui::CursorStyle::PointingHand, &hitbox);
                let at = move |x: gpui::Pixels| fraction(f32::from(x), f32::from(bounds.origin.x), f32::from(bounds.size.width));
                let (down, moved, up) = ((entity.clone(), key.clone(), clip.clone()), (entity.clone(), key.clone(), clip), entity);
                window.on_mouse_event(move |e: &gpui::MouseDownEvent, phase, window, cx| {
                    if phase == gpui::DispatchPhase::Bubble && e.button == gpui::MouseButton::Left && hitbox.is_hovered(window) {
                        let (entity, key, clip) = &down;
                        let _ = entity.update(cx, |this, cx| {
                            this.audio_seek(key, clip, ms, at(e.position.x), cx);
                            if let Some(a) = this.audio.as_mut() {
                                a.drag = true;
                            }
                        });
                        cx.stop_propagation();
                    }
                });
                if !dragging {
                    return;
                }
                window.on_mouse_event(move |e: &gpui::MouseMoveEvent, phase, _, cx| {
                    if phase == gpui::DispatchPhase::Bubble && e.pressed_button == Some(gpui::MouseButton::Left) {
                        let (entity, key, clip) = &moved;
                        let _ = entity.update(cx, |this, cx| this.audio_seek(key, clip, ms, at(e.position.x), cx));
                    }
                });
                window.on_mouse_event(move |_: &gpui::MouseUpEvent, phase, _, cx| {
                    if phase == gpui::DispatchPhase::Bubble {
                        let _ = up.update(cx, |this, cx| {
                            if let Some(a) = this.audio.as_mut() {
                                a.drag = false;
                            }
                            cx.notify();
                        });
                    }
                });
            },
        )
        .absolute()
        .size_full();
        div().relative().h(px(14.0)).child(track).child(input).into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_read_elapsed_over_duration() {
        assert_eq!(times(4.2, Some(12.0)), "0:04 / 0:12");
        assert_eq!(times(30.0, Some(12.0)), "0:12 / 0:12", "never past the end");
        assert_eq!(times(75.9, None), "1:15");
        assert_eq!(times(-1.0, Some(3725.0)), "0:00 / 1:02:05");
    }

    #[test]
    fn presses_map_onto_the_bar() {
        assert!((fraction(150.0, 100.0, 200.0) - 0.25).abs() < 1e-9);
        assert!((fraction(50.0, 100.0, 200.0)).abs() < 1e-9 && (fraction(900.0, 100.0, 200.0) - 1.0).abs() < 1e-9, "clamped");
        assert!(fraction(10.0, 0.0, 0.0).abs() < 1e-9);
    }
}
