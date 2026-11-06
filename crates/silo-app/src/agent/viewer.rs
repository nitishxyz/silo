//! The media viewer (`docs/MEDIA.md` §5.3, §6.4): a generated image or video over the agent pane on a dimmed
//! backdrop, fit to it; images zoom around the pointer (pinch, scroll, ⌘-scroll, `+` `-`, 10 %–800 %; `0` fits,
//! `1` is 100 %, double-click toggles) and drag to pan; a video plays in gpui (`player/`); ←/→ (`[` `]` for a
//! video, whose ←/→ seek) the session's other images and videos; copy, save, close. The backdrop is opaque
//! once open: nothing of the transcript or the composer shows through.

use std::{cell::Cell, rc::Rc, time::Instant};

use gpui::{Bounds, FocusHandle, Pixels, Point, Window};
use otto_proto::MediaKind;

use super::{
    AgentPane, av,
    media::{Entry, Loaded, Media, Tiles},
    player, svg,
    transcript::Row,
};

/// Zoom bounds (1 is 100 %).
pub const MIN: f32 = 0.1;
pub const MAX: f32 = 8.0;
/// Room for the toolbar above the fitted image, the footer below it, and the sides.
const TOP: f32 = 56.0;
const BOTTOM: f32 = 84.0;
const SIDE: f32 = 32.0;
/// A video's room above it: its title and Close.
const VIDEO_TOP: f32 = 64.0;
/// Opening and closing (the sign-in dialog's), and a zoom by key or button gliding.
pub(super) const ENTER: std::time::Duration = std::time::Duration::from_millis(200);
pub(super) const EXIT: std::time::Duration = std::time::Duration::from_millis(160);
pub(super) const GLIDE: std::time::Duration = std::time::Duration::from_millis(180);

pub(super) struct Viewer {
    /// The attachment shown.
    pub id: String,
    pub focus: FocusHandle,
    /// Focus before it opened, given back when it closes.
    back: Option<FocusHandle>,
    /// The scale (1 is 100 %); `None` fits.
    pub zoom: Option<f32>,
    /// The image centre's offset from the view's.
    pub pan: (f32, f32),
    /// The scale and pan a glide starts from, and when.
    pub from: Option<(Instant, f32, (f32, f32))>,
    /// A drag's start: pointer and pan.
    pub drag: Option<(Point<Pixels>, (f32, f32))>,
    pub shown: Instant,
    pub closing: Option<Instant>,
    /// The backdrop's bounds as last painted (window coordinates).
    pub bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// A passing word in the toolbar (`Copied`, `Saved`, `Couldn't save`), and since when.
    pub note: Option<(&'static str, Instant)>,
    /// The shown video's player, and what carries to the next video (volume, loop, fill).
    pub video: Option<player::Playback>,
    pub prefs: player::Prefs,
    /// A closing video's last frame, fading out (a surface has no opacity).
    still: Option<std::sync::Arc<gpui::RenderImage>>,
    /// An SVG's sharp raster of what shows, the one being made, and old ones to drop from the atlas.
    detail: Option<Detail>,
    detail_pending: bool,
    retired: Vec<std::sync::Arc<gpui::RenderImage>>,
}

/// A raster of the part of an SVG that shows at the zoomed scale (`svg.rs`).
struct Detail {
    /// The attachment, the scale (px a unit, ×1000) and the region in pixels it was made for.
    key: (String, [i64; 5]),
    region: svg::Rect,
    image: std::sync::Arc<gpui::RenderImage>,
}

/// One image of the session, with what made it.
pub(super) struct Shown {
    pub media: Media,
    pub prompt: String,
    pub credit: Option<String>,
}

/// Where the image sits: the view's size, the fitted area's centre (both local), and the fit scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Geo {
    pub view: (f32, f32),
    pub center: (f32, f32),
    pub fit: f32,
    pub dims: (f32, f32),
}

impl Geo {
    #[must_use]
    pub fn new(view: (f32, f32), dims: (f32, f32)) -> Self {
        let area = ((view.0 - 2.0 * SIDE).max(40.0), (view.1 - TOP - BOTTOM).max(40.0));
        let fit = if dims.0 > 0.0 && dims.1 > 0.0 { (area.0 / dims.0).min(area.1 / dims.1).min(1.0) } else { 1.0 };
        Self { view, center: (view.0 / 2.0, TOP + area.1 / 2.0), fit, dims }
    }

    /// A video: fit to the area above the player's bar (`fill`: the whole view), larger than its pixels too.
    #[must_use]
    pub fn video(view: (f32, f32), dims: (f32, f32), fill: bool) -> Self {
        let (top, bottom, side) = if fill { (0.0, 0.0, 0.0) } else { (VIDEO_TOP, player::BAR_ROOM, SIDE) };
        let area = ((view.0 - 2.0 * side).max(40.0), (view.1 - top - bottom).max(40.0));
        let fit = if dims.0 > 0.0 && dims.1 > 0.0 { (area.0 / dims.0).min(area.1 / dims.1) } else { 1.0 };
        Self { view, center: (view.0 / 2.0, top + area.1 / 2.0), fit, dims }
    }

    /// `pan` kept so an image larger than the view covers it (else centred on that axis).
    #[must_use]
    pub fn clamp(&self, scale: f32, pan: (f32, f32)) -> (f32, f32) {
        let axis = |pan: f32, len: f32, view: f32, center: f32| {
            if len <= view { 0.0 } else { pan.clamp(view - len / 2.0 - center, len / 2.0 - center) }
        };
        (axis(pan.0, self.dims.0 * scale, self.view.0, self.center.0), axis(pan.1, self.dims.1 * scale, self.view.1, self.center.1))
    }

    /// The pan after scaling `from` → `to` with the point `at` (local) staying put.
    #[must_use]
    pub fn anchored(&self, from: f32, to: f32, pan: (f32, f32), at: (f32, f32)) -> (f32, f32) {
        let q = (at.0 - self.center.0, at.1 - self.center.1);
        let k = to / from.max(0.0001);
        self.clamp(to, (q.0 - (q.0 - pan.0) * k, q.1 - (q.1 - pan.1) * k))
    }

    /// The image is larger than the view on some axis (scrolling pans it).
    #[must_use]
    pub fn overflows(&self, scale: f32) -> bool {
        self.dims.0 * scale > self.view.0 + 0.5 || self.dims.1 * scale > self.view.1 + 0.5
    }
}

impl AgentPane {
    /// Every generated image and video of the session, in transcript order.
    pub(super) fn generated(&self) -> Vec<Shown> {
        let mut out = Vec::new();
        for row in &self.rows {
            if let Row::Media(s) = row
                && let Tiles::Ready(media) = &s.tiles
            {
                let shown = media.iter().filter(|m| m.kind() != MediaKind::Audio);
                out.extend(shown.map(|m| Shown { media: m.clone(), prompt: s.prompt.clone(), credit: s.credit.clone() }));
            }
        }
        out
    }

    /// Open the viewer on attachment `id`; it takes key focus.
    pub(super) fn open_viewer(&mut self, id: &str, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let focus = cx.focus_handle();
        let back = window.focused(cx);
        window.focus(&focus, cx);
        self.viewer = Some(Viewer {
            id: id.to_owned(),
            focus,
            back,
            zoom: None,
            pan: (0.0, 0.0),
            from: None,
            drag: None,
            shown: Instant::now(),
            closing: None,
            bounds: Rc::default(),
            note: None,
            video: None,
            prefs: player::Prefs::default(),
            still: None,
            detail: None,
            detail_pending: false,
            retired: Vec::new(),
        });
        cx.notify();
    }

    /// Fade out and give focus back.
    pub(super) fn close_viewer(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let Some(v) = self.viewer.as_mut().filter(|v| v.closing.is_none()) else { return };
        v.closing = Some(Instant::now());
        v.drag = None;
        if let Some(p) = &mut v.video {
            p.video.player.pause();
            p.drag = None;
            v.still = p.video.frame().and_then(|f| av::still(&f)).map(std::sync::Arc::new);
        }
        match v.back.take() {
            Some(back) => window.focus(&back, cx),
            None => self.focus_composer(window, cx),
        }
        cx.notify();
    }

    /// The shown image's or video's geometry, once the viewer was painted and its size is known.
    pub(super) fn viewer_geo(&self) -> Option<Geo> {
        let v = self.viewer.as_ref()?;
        let b = v.bounds.get()?;
        let key = format!("{}/{}", self.session_id.as_deref()?, v.id);
        let shown = || self.generated().into_iter().find(|s| s.media.id == v.id);
        let (dims, video) = match self.media.get(&key) {
            Some(Entry::Ready(l)) => (l.dims, false),
            Some(Entry::Clip(c)) => (shown()?.media.dims.or(c.poster.as_ref().map(|p| p.1)).unwrap_or((1280, 720)), true),
            _ => {
                let m = shown()?.media;
                let video = m.kind() == MediaKind::Video;
                (m.dims.or(video.then_some((1280, 720)))?, video)
            }
        };
        #[allow(clippy::cast_precision_loss)]
        let (view, dims) = ((f32::from(b.size.width), f32::from(b.size.height)), (dims.0 as f32, dims.1 as f32));
        Some(if video { Geo::video(view, dims, v.prefs.fill) } else { Geo::new(view, dims) })
    }

    /// The shown item is a video (no zoom).
    pub(super) fn viewing_video(&self) -> bool {
        self.viewer.as_ref().is_some_and(|v| self.generated().iter().any(|s| s.media.id == v.id && s.media.kind() == MediaKind::Video))
    }

    /// The scale and pan drawn now: the target, or on the way to it.
    pub(super) fn viewer_shown(&self, geo: &Geo, now: Instant, cx: &gpui::App) -> (f32, (f32, f32)) {
        let Some(v) = self.viewer.as_ref() else { return (geo.fit, (0.0, 0.0)) };
        let scale = v.zoom.unwrap_or(geo.fit);
        match v.from.and_then(|(at, s, p)| Some((super::view::eased(at, GLIDE, now, cx)?, s, p))) {
            Some((t, s, p)) => (s + (scale - s) * t, (p.0 + (v.pan.0 - p.0) * t, p.1 + (v.pan.1 - p.1) * t)),
            None => (scale, v.pan),
        }
    }

    /// Zoom to `to` (`None` fits) around `at` (window coordinates; the centre when `None`), gliding there for
    /// keys and buttons.
    pub(super) fn zoom_viewer(&mut self, to: Option<f32>, at: Option<Point<Pixels>>, glide: bool, cx: &mut gpui::Context<Self>) {
        let Some(geo) = self.viewer_geo() else { return };
        let now = Instant::now();
        let shown = self.viewer_shown(&geo, now, cx);
        let Some(v) = self.viewer.as_mut() else { return };
        let from = v.zoom.unwrap_or(geo.fit);
        let origin = v.bounds.get().map_or(Point::default(), |b| b.origin);
        let at = at.map_or(geo.center, |p| (f32::from(p.x - origin.x), f32::from(p.y - origin.y)));
        v.from = (glide && !cx.reduce_motion()).then_some((now, shown.0, shown.1));
        match to {
            None => (v.zoom, v.pan) = (None, (0.0, 0.0)),
            Some(to) => {
                let to = to.clamp(MIN, MAX);
                v.pan = geo.anchored(from, to, v.pan, at);
                v.zoom = Some(to);
            }
        }
        cx.notify();
    }

    /// The current scale (fit resolved).
    pub(super) fn viewer_scale(&self) -> Option<f32> {
        let geo = self.viewer_geo()?;
        Some(self.viewer.as_ref()?.zoom.unwrap_or(geo.fit))
    }

    /// The previous (`-1`) or next (`1`) image of the session, fitted.
    pub(super) fn step_viewer(&mut self, dir: i32, cx: &mut gpui::Context<Self>) {
        let all = self.generated();
        let Some(v) = self.viewer.as_mut() else { return };
        let Some(ix) = all.iter().position(|s| s.media.id == v.id) else { return };
        let Some(next) = ix.checked_add_signed(dir as isize).and_then(|i| all.get(i)) else { return };
        v.id.clone_from(&next.media.id);
        (v.zoom, v.pan, v.from, v.drag, v.video) = (None, (0.0, 0.0), None, None, None);
        v.retired.extend(v.detail.take().map(|d| d.image));
        cx.notify();
    }
}

impl AgentPane {
    /// Keys while the viewer has focus; none reach the pane.
    pub(super) fn viewer_key(&mut self, event: &gpui::KeyDownEvent, window: &mut Window, cx: &mut gpui::Context<Self>) {
        let k = &event.keystroke;
        let m = &k.modifiers;
        let scale = self.viewer_scale();
        let video = self.viewing_video();
        if video && let Some(key) = player::map(&k.key, m.platform, m.control) {
            self.player_key(key, window, cx);
            return cx.stop_propagation();
        }
        match (k.key.as_str(), m.platform) {
            ("escape", _) => self.close_viewer(window, cx),
            ("left" | "[", false) => self.step_viewer(-1, cx),
            ("right" | "]", false) => self.step_viewer(1, cx),
            ("=" | "+" | "-" | "0" | "1", _) if video => {}
            ("=" | "+", _) => self.zoom_viewer(scale.map(|s| crate::files::zoom::stepped(s, 1, MIN, MAX)), None, true, cx),
            ("-", _) => self.zoom_viewer(scale.map(|s| crate::files::zoom::stepped(s, -1, MIN, MAX)), None, true, cx),
            ("0", _) => self.zoom_viewer(None, None, true, cx),
            ("1", false) => self.zoom_viewer(Some(1.0), None, true, cx),
            ("c", true) => self.copy_viewed(cx),
            ("s", true) => self.save_viewed(cx),
            // Window and app chords (⌘W, ⌘Q, …) go on.
            _ if m.platform || m.control => return,
            _ => {}
        }
        cx.stop_propagation();
    }

    /// Scroll: ⌘-scroll zooms, and so does plain scroll while the image fits; once it overflows, scroll pans.
    pub(super) fn viewer_wheel(&mut self, event: &gpui::ScrollWheelEvent, cx: &mut gpui::Context<Self>) {
        if self.viewing_video() {
            return;
        }
        let (Some(geo), Some(scale)) = (self.viewer_geo(), self.viewer_scale()) else { return };
        let d = event.delta.pixel_delta(gpui::px(16.0));
        let (dx, dy) = (f32::from(d.x), f32::from(d.y));
        if event.modifiers.platform || !geo.overflows(scale) {
            if dy.abs() > 0.0 {
                self.zoom_viewer(Some(scale * (dy * 0.006).exp()), Some(event.position), false, cx);
            }
        } else if let Some(v) = self.viewer.as_mut() {
            v.from = None;
            v.pan = geo.clamp(scale, (v.pan.0 + dx, v.pan.1 + dy));
            cx.notify();
        }
    }

    pub(super) fn viewer_pinch(&mut self, event: &gpui::PinchEvent, cx: &mut gpui::Context<Self>) {
        if let Some(scale) = self.viewer_scale().filter(|_| !self.viewing_video()) {
            self.zoom_viewer(Some(crate::files::zoom::pinched(scale, event.delta, MIN, MAX)), Some(event.position), false, cx);
        }
    }

    /// A press on the image: a double-click toggles fit / 100 % at the pointer, else a drag starts.
    pub(super) fn viewer_press(&mut self, event: &gpui::MouseDownEvent, cx: &mut gpui::Context<Self>) {
        cx.stop_propagation();
        // A video: a click plays or pauses; a double-click (its first click undone) fills or fits.
        if self.viewing_video() {
            self.player_activity(cx);
            self.player_toggle(cx);
            if event.click_count == 2 {
                self.player_fill(cx);
            }
            return;
        }
        let (Some(geo), Some(scale)) = (self.viewer_geo(), self.viewer_scale()) else { return };
        if event.click_count == 2 {
            let to = if (scale - 1.0).abs() < 0.01 && geo.fit < 0.99 { None } else { Some(1.0) };
            return self.zoom_viewer(to, Some(event.position), true, cx);
        }
        if let Some(v) = self.viewer.as_mut() {
            v.drag = Some((event.position, v.pan));
            v.from = None;
        }
    }

    pub(super) fn viewer_drag(&mut self, event: &gpui::MouseMoveEvent, cx: &mut gpui::Context<Self>) {
        if let Some(v) = self.viewer.as_mut()
            && let Some(p) = v.video.as_mut()
        {
            p.pointer_in = v.bounds.get().is_some_and(|b| b.contains(&event.position));
            let drag = p.drag.filter(|_| event.pressed_button == Some(gpui::MouseButton::Left));
            if p.pointer_in || drag.is_some() {
                self.player_activity(cx);
            }
            match drag {
                Some(player::Drag::Scrub { .. }) => return self.scrub_to(event.position.x, cx),
                Some(player::Drag::Volume) => return self.volume_to(event.position.x, cx),
                None => {}
            }
        }
        let (Some(geo), Some(scale)) = (self.viewer_geo(), self.viewer_scale()) else { return };
        let Some(v) = self.viewer.as_mut() else { return };
        let Some((start, pan)) = v.drag else { return };
        if event.pressed_button != Some(gpui::MouseButton::Left) {
            v.drag = None;
            return cx.notify();
        }
        let d = event.position - start;
        v.pan = geo.clamp(scale, (pan.0 + f32::from(d.x), pan.1 + f32::from(d.y)));
        cx.notify();
    }

    /// The shown image, decoded.
    fn viewed(&self) -> Option<(Media, std::sync::Arc<super::media::Loaded>)> {
        let v = self.viewer.as_ref()?;
        let Some(Entry::Ready(l)) = self.media.get(&format!("{}/{}", self.session_id.as_deref()?, v.id)) else { return None };
        Some((self.generated().into_iter().find(|s| s.media.id == v.id)?.media, l.clone()))
    }

    /// The shown video, written to the cache.
    fn viewed_clip(&self) -> Option<(Media, std::sync::Arc<super::media::Clip>)> {
        let v = self.viewer.as_ref()?;
        let Some(Entry::Clip(c)) = self.media.get(&format!("{}/{}", self.session_id.as_deref()?, v.id)) else { return None };
        Some((self.generated().into_iter().find(|s| s.media.id == v.id)?.media, c.clone()))
    }

    /// A word in the toolbar for a moment.
    fn viewer_note(&mut self, note: &'static str, cx: &mut gpui::Context<Self>) {
        if let Some(v) = self.viewer.as_mut() {
            v.note = Some((note, Instant::now()));
        }
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(NOTE).await;
            let _ = this.update(cx, |_, cx| cx.notify());
        })
        .detach();
    }

    /// An image to the clipboard; a video as its file (named as made, next to the cache file).
    pub(super) fn copy_viewed(&mut self, cx: &mut gpui::Context<Self>) {
        if let Some((media, c)) = self.viewed_clip() {
            let named = c.file.with_extension("").join(media.name.replace(['/', ':'], "-"));
            let linked =
                named.exists() || (named.parent().is_some_and(|d| std::fs::create_dir_all(d).is_ok()) && std::fs::hard_link(&c.file, &named).is_ok());
            return self.viewer_note(if linked && av::copy_file(&named) { "Copied" } else { "Couldn't copy" }, cx);
        }
        let Some((media, l)) = self.viewed() else { return };
        // An SVG goes as its file (a raster would lose it); anything else as the image.
        if l.svg.is_some() {
            let copied = self.svg_file(&media, &l).is_some_and(|f| av::copy_file(&f));
            return self.viewer_note(if copied { "Copied" } else { "Couldn't copy" }, cx);
        }
        cx.write_to_clipboard(gpui::ClipboardItem::new_image(&l.image));
        self.viewer_note("Copied", cx);
    }

    /// The save panel (Downloads, the file's name), then its bytes written there.
    pub(super) fn save_viewed(&mut self, cx: &mut gpui::Context<Self>) {
        if let Some((media, c)) = self.viewed_clip() {
            let saved = self.save_clip(&media.name, c.file.clone(), cx);
            return cx
                .spawn(async move |this, cx| {
                    let Some(wrote) = saved.await else { return };
                    let _ = this.update(cx, |this, cx| this.viewer_note(if wrote { "Saved" } else { "Couldn't save" }, cx));
                })
                .detach();
        }
        let Some((media, l)) = self.viewed() else { return };
        let chosen = cx.prompt_for_new_path(&silo_web::downloads_dir(), Some(&media.name));
        cx.spawn(async move |this, cx| {
            let Some(path) = chosen.await.ok().and_then(Result::ok).flatten() else { return };
            let wrote = cx.background_executor().spawn(async move { std::fs::write(&path, l.image.bytes()) }).await;
            let _ = this.update(cx, |this, cx| this.viewer_note(if wrote.is_ok() { "Saved" } else { "Couldn't save" }, cx));
        })
        .detach();
    }
}

/// Chrome over the dark backdrop, the same in light and dark themes.
const INK: u32 = 0x00f2_f2f2;
const DIM: u32 = 0x00a3_a3a3;
/// The backdrop's blur while it fades.
const BLUR: f32 = 24.0;
/// How long a toolbar note shows.
const NOTE: std::time::Duration = std::time::Duration::from_millis(1400);

/// A toolbar button: an icon or a short label.
fn tool(id: &'static str, label: Option<&'static str>, glyph: Option<&'static str>) -> gpui::Stateful<gpui::Div> {
    use gpui::{div, prelude::*, px, rgb};
    div()
        .id(id)
        .flex_none()
        .h(px(28.0))
        .min_w(px(28.0))
        .px(px(if label.is_some() { 9.0 } else { 0.0 }))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(7.0))
        .text_size(px(12.0))
        .text_color(rgb(INK))
        .cursor_pointer()
        .hover(|s| s.bg(gpui::hsla(0.0, 0.0, 1.0, 0.1)))
        .active(|s| s.bg(gpui::hsla(0.0, 0.0, 1.0, 0.16)))
        .children(label)
        .children(glyph.map(|g| crate::assets::icon(g, 14.0, INK)))
}

/// A thin divider between the toolbar's groups.
fn divider() -> gpui::Div {
    use gpui::{div, prelude::*, px};
    div().flex_none().w(px(1.0)).h(px(16.0)).mx(px(4.0)).bg(gpui::hsla(0.0, 0.0, 1.0, 0.14))
}

/// A frosted dark chip the toolbar and side buttons sit on.
fn chip() -> gpui::Div {
    use gpui::{div, prelude::*, px};
    div()
        .occlude()
        .flex()
        .items_center()
        .p(px(3.0))
        .rounded(px(10.0))
        .bg(gpui::hsla(0.0, 0.0, 0.08, 0.78))
        .border_1()
        .border_color(gpui::hsla(0.0, 0.0, 1.0, 0.08))
}

impl AgentPane {
    /// The viewer over the pane, while open or fading out.
    #[allow(clippy::too_many_lines)]
    pub(super) fn render_viewer(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> Option<gpui::AnyElement> {
        use gpui::{MouseButton, div, prelude::*, px, rgb};
        let now = Instant::now();
        let v = self.viewer.as_ref()?;
        let all = self.generated();
        let ix = all.iter().position(|s| s.media.id == v.id);
        let t = match v.closing {
            Some(at) => 1.0 - super::view::eased(at, EXIT, now, cx).unwrap_or(1.0),
            None => super::view::eased(v.shown, ENTER, now, cx).unwrap_or(1.0),
        };
        // Closed, or its image left the transcript (another session).
        let Some(ix) = ix.filter(|_| t > 0.0 || v.closing.is_none()) else {
            if v.closing.is_none() {
                self.close_viewer(window, cx);
            }
            if let Some(v) = self.viewer.take() {
                for image in v.retired.into_iter().chain(v.detail.map(|d| d.image)).chain(v.still) {
                    let _ = window.drop_image(image);
                }
            }
            return None;
        };
        let (id, closing, bounds, focus) = (v.id.clone(), v.closing.is_some(), v.bounds.clone(), v.focus.clone());
        let note = v.note.filter(|(_, at)| now.duration_since(*at) < NOTE).map(|(n, _)| n);
        let dragging = v.drag.is_some();
        let settled = !dragging && t >= 1.0 && v.from.is_none_or(|(at, ..)| now.duration_since(at) >= GLIDE);
        let shown = &all[ix];
        let entry = self.media(&shown.media, cx);
        let video = shown.media.kind() == MediaKind::Video;
        if let (true, Entry::Clip(c)) = (video, &entry) {
            self.ensure_playback(&id, c, cx);
        }
        // The video plays once the viewer is open; its frame for now (the poster stands in before the first,
        // and a still of the last while closing: a surface draws at full opacity).
        let frame = match self.viewer.as_mut().and_then(|v| v.video.as_mut()).filter(|p| p.id == id) {
            Some(p) => {
                p.painted = now;
                if t >= 1.0 && !closing && !p.started {
                    p.started = true;
                    p.play(now);
                }
                if closing || t < 1.0 { None } else { p.video.frame() }
            }
            None => None,
        };
        let geo = self.viewer_geo();
        let (scale, pan) = geo.map_or((1.0, (0.0, 0.0)), |g| self.viewer_shown(&g, now, cx));
        let detail = match (&entry, geo) {
            (Entry::Ready(l), Some(g)) => self.svg_detail(&id, l, &g, (scale, pan), window.scale_factor(), settled, cx),
            _ => None,
        };
        for image in self.viewer.as_mut().map(|v| std::mem::take(&mut v.retired)).unwrap_or_default() {
            let _ = window.drop_image(image);
        }
        let entity = cx.entity().downgrade();
        // Bounds for the geometry; moves and releases anywhere (a drag may leave the pane).
        let probe = gpui::canvas(
            move |b, window, _| {
                if bounds.get() != Some(b) {
                    bounds.set(Some(b));
                    window.refresh();
                }
            },
            move |_, (), window, _| {
                let (moved, up) = (entity.clone(), entity.clone());
                window.on_mouse_event(move |e: &gpui::MouseMoveEvent, phase, _, cx| {
                    if phase == gpui::DispatchPhase::Bubble {
                        let _ = moved.update(cx, |this, cx| this.viewer_drag(e, cx));
                    }
                });
                window.on_mouse_event(move |_: &gpui::MouseUpEvent, phase, _, cx| {
                    if phase == gpui::DispatchPhase::Bubble {
                        let _ = up.update(cx, |this, cx| {
                            if let Some(v) = this.viewer.as_mut().filter(|v| v.drag.is_some()) {
                                v.drag = None;
                                cx.notify();
                            }
                            if this.viewer.as_ref().and_then(|v| v.video.as_ref()).is_some_and(|p| p.drag.is_some()) {
                                this.player_release(cx);
                            }
                        });
                    }
                });
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        // The image: opening grows it from 96 %; while it loads, a pulse at its size; failed, a quiet line.
        let still = self.viewer.as_ref().and_then(|v| v.still.clone());
        let picture = geo.map(|g| {
            let k = 0.96 + 0.04 * t;
            let (w, h) = (g.dims.0 * scale * k, g.dims.1 * scale * k);
            let (x, y) = (g.center.0 + pan.0 - w / 2.0, g.center.1 + pan.1 - h / 2.0);
            let over = g.overflows(scale) && !video;
            let el = div().id("viewer-image").absolute().left(px(x)).top(px(y)).w(px(w)).h(px(h)).opacity(t);
            let el = match &entry {
                // An id keeps a GIF playing; an SVG zoomed past gpui's raster gets a sharp one of what shows.
                Entry::Ready(l) => {
                    el.child(gpui::img(l.image.clone()).id("viewer-picture").size_full()).children(detail.clone().map(|(r, image)| {
                        gpui::img(image).absolute().left(px(r.0 * scale)).top(px(r.1 * scale)).w(px(r.2 * scale)).h(px(r.3 * scale))
                    }))
                }
                Entry::Clip(c) => {
                    let poster = c.poster.as_ref().filter(|_| frame.is_none() && still.is_none()).map(|(p, _)| p.clone());
                    let contain = |el: gpui::Img| el.absolute().size_full().object_fit(gpui::ObjectFit::Contain);
                    el.bg(gpui::black())
                        .children(poster.map(|p| contain(gpui::img(p))))
                        .children(still.clone().filter(|_| closing).map(|s| contain(gpui::img(s))))
                        .children(frame.clone().map(player::surface))
                }
                _ => el.rounded(px(8.0)).bg(gpui::hsla(0.0, 0.0, 1.0, 0.06)),
            };
            el.when(over, |el| el.cursor(if dragging { gpui::CursorStyle::ClosedHand } else { gpui::CursorStyle::OpenHand }))
                .when(!closing, |el| el.on_mouse_down(MouseButton::Left, cx.listener(|this, e, _, cx| this.viewer_press(e, cx))))
                .into_any_element()
        });
        let failed = matches!(entry, Entry::Failed).then(|| {
            div().absolute().size_full().flex().items_center().justify_center().text_size(px(13.0)).text_color(rgb(DIM)).child(if video {
                "Video unavailable"
            } else {
                "Image unavailable"
            })
        });
        let toolbar = self.viewer_toolbar(scale, note, matches!(entry, Entry::Ready(_) | Entry::Clip(_)), video, cx);
        let footer = Self::viewer_footer(shown, all.len(), ix, geo.map(|g| g.dims).filter(|_| !video));
        let sides = (all.len() > 1).then(|| self.viewer_sides(ix, all.len(), cx));
        let width = self.viewer.as_ref().and_then(|v| v.bounds.get()).map_or(0.0, |b| f32::from(b.size.width));
        // A video's chrome: the title and Close on top, the bar at the bottom, over scrims; faded together, and
        // hidden (with the pointer) while it plays untouched. An image's: the toolbar, the footer, the sides.
        let playback = self.viewer.as_ref().and_then(|v| Some((v.video.as_ref().filter(|p| p.id == id && video)?, v.prefs)));
        let chrome: Vec<gpui::AnyElement> = match playback {
            Some((p, prefs)) => {
                let alpha = t * super::view::toggled(p.hide.fade(), now, cx);
                let ended = p.video.player.ended() && !prefs.looping && p.drag.is_none() && !closing;
                let layer = (alpha > 0.0).then(|| {
                    div()
                        .absolute()
                        .size_full()
                        .opacity(alpha)
                        .child(player::scrim(true))
                        .child(player::scrim(false))
                        .child(div().absolute().top(px(10.0)).left(px(64.0)).right(px(64.0)).child(footer))
                        .child(div().absolute().top(px(12.0)).right(px(12.0)).child(toolbar))
                        .children(sides)
                        .child(Self::player_bar(p, prefs, width, cx))
                        .into_any_element()
                });
                ended.then(|| Self::replay_disc(cx).into_any_element()).into_iter().chain(layer).collect()
            }
            None => sides
                .into_iter()
                .chain([
                    div().absolute().top(px(12.0)).right(px(12.0)).opacity(t).child(toolbar).into_any_element(),
                    div().absolute().bottom(px(14.0)).left(px(SIDE)).right(px(SIDE)).opacity(t).child(footer).into_any_element(),
                ])
                .collect(),
        };
        // Opaque once open (nothing of the transcript or composer shows through), blurring what's behind on
        // the way in and out.
        let blur = crate::surface::current().frosted() && t < 1.0;
        Some(
            div()
                .id("agent-viewer")
                .track_focus(&focus)
                .key_context("MediaViewer")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .occlude()
                .overflow_hidden()
                .bg(gpui::hsla(0.0, 0.0, 0.035, t))
                .when(blur, |el| el.backdrop_blur(px(BLUR * t)))
                .font_family(crate::theme::ui_font())
                .child(probe)
                .when(!closing, |el| {
                    el.on_key_down(cx.listener(|this, e, window, cx| this.viewer_key(e, window, cx)))
                        .on_scroll_wheel(cx.listener(|this, e, _, cx| this.viewer_wheel(e, cx)))
                        .on_pinch(cx.listener(|this, e, _, cx| this.viewer_pinch(e, cx)))
                        .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| this.close_viewer(window, cx)))
                })
                .children(picture)
                .children(failed)
                .children(chrome)
                .into_any_element(),
        )
    }
}

impl AgentPane {
    /// Zoom %, Fit, 1:1, Copy, Save…, Close (a video: a passing note and Close).
    fn viewer_toolbar(&self, scale: f32, note: Option<&'static str>, ready: bool, video: bool, cx: &mut gpui::Context<Self>) -> gpui::Div {
        use gpui::{ClickEvent, div, prelude::*, px, rgb};
        #[allow(clippy::cast_possible_truncation)]
        let pct = format!("{}%", (scale * 100.0).round() as i32);
        // A video has no zoom: only a passing note shows there.
        let label = (!video || note.is_some()).then(|| {
            div()
                .min_w(px(52.0))
                .px(px(6.0))
                .flex()
                .justify_center()
                .text_size(px(12.0))
                .text_color(rgb(if note.is_some() { INK } else { DIM }))
                .font_features(super::view::tabular())
                .child(note.map_or(pct, str::to_owned))
        });
        let ready_only = |el: gpui::Stateful<gpui::Div>| if ready { el } else { el.opacity(0.4) };
        let zoom = (!video).then(|| {
            div()
                .flex()
                .items_center()
                .child(
                    tool("viewer-fit", Some("Fit"), None)
                        .tooltip(crate::tooltip::Tooltip::text("Fit  0"))
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.zoom_viewer(None, None, true, cx))),
                )
                .child(
                    tool("viewer-actual", Some("1:1"), None)
                        .tooltip(crate::tooltip::Tooltip::text("Actual size  1"))
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.zoom_viewer(Some(1.0), None, true, cx))),
                )
                .child(divider())
        });
        // A video's Copy and Save… sit in its player's bar.
        let files = (!video).then(|| {
            div()
                .flex()
                .items_center()
                .child(ready_only(
                    tool("viewer-copy", None, Some("copy"))
                        .tooltip(crate::tooltip::Tooltip::text("Copy image  ⌘C"))
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.copy_viewed(cx))),
                ))
                .child(ready_only(
                    tool("viewer-save", Some("Save…"), None)
                        .tooltip(crate::tooltip::Tooltip::text("Save  ⌘S"))
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| this.save_viewed(cx))),
                ))
                .child(divider())
        });
        chip().children(label.map(|l| div().flex().items_center().child(l).child(divider()))).children(zoom).children(files).child(
            tool("viewer-close", None, Some("close"))
                .tooltip(crate::tooltip::Tooltip::text("Close  esc"))
                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.close_viewer(window, cx))),
        )
    }

    /// Previous / next at the sides (hidden at the ends).
    fn viewer_sides(&self, ix: usize, count: usize, cx: &mut gpui::Context<Self>) -> gpui::AnyElement {
        use gpui::{ClickEvent, div, prelude::*, px};
        let side = |id: &'static str, dir: i32, cx: &mut gpui::Context<Self>| {
            let glyph = crate::assets::icon("chevron-right", 16.0, INK)
                .when(dir < 0, |g| g.with_transformation(gpui::Transformation::rotate(gpui::radians(std::f32::consts::PI))));
            chip().rounded_full().child(
                tool(id, None, None)
                    .size(px(32.0))
                    .rounded_full()
                    .child(glyph)
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.step_viewer(dir, cx))),
            )
        };
        let middle = |el: gpui::Div| el.absolute().top_0().bottom(px(BOTTOM - TOP)).flex().items_center();
        div()
            .absolute()
            .size_full()
            .when(ix > 0, |el| el.child(middle(div().left(px(12.0))).child(side("viewer-prev", -1, cx))))
            .when(ix + 1 < count, |el| el.child(middle(div().right(px(12.0))).child(side("viewer-next", 1, cx))))
            .into_any_element()
    }

    /// Name; `1024×1024 · PNG · 1.2 MB · OpenAI gpt-image · 2 of 5`; the prompt on one line (all of it on hover).
    fn viewer_footer(shown: &Shown, count: usize, ix: usize, dims: Option<(f32, f32)>) -> gpui::Div {
        use gpui::{FontWeight, div, prelude::*, px, rgb};
        let mut media = shown.media.clone();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let decoded = dims.map(|(w, h)| (w as u32, h as u32));
        media.dims = media.dims.or(decoded);
        let caption = std::iter::once(media.caption())
            .chain(shown.credit.clone())
            .chain((count > 1).then(|| format!("{} of {count}", ix + 1)))
            .collect::<Vec<_>>()
            .join(" · ");
        let line = |el: gpui::Div| el.max_w_full().overflow_hidden().whitespace_nowrap().text_ellipsis();
        let prompt = (!shown.prompt.is_empty()).then(|| {
            div()
                .id("viewer-prompt")
                .max_w_full()
                .child(line(div()).text_color(rgb(DIM)).child(super::transcript::clip(&shown.prompt, 400)))
                .tooltip(crate::tooltip::Tooltip::text(shown.prompt.clone()))
        });
        div().flex().justify_center().child(
            div()
                .occlude()
                .max_w(px(640.0))
                .min_w_0()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(2.0))
                .text_size(px(12.0))
                .child(line(div()).text_size(px(13.0)).font_weight(FontWeight::MEDIUM).text_color(rgb(INK)).child(media.name.clone()))
                .child(line(div()).text_color(rgb(DIM)).font_features(super::view::tabular()).child(caption))
                .children(prompt),
        )
    }
}

impl AgentPane {
    /// An SVG drawn past gpui's raster (2 px a unit): a raster of just what shows at `scale` (and `device` px a
    /// point), made off the main thread once the view settles; one at a time, the newest view next. Its region
    /// (image units) and image while one fits this SVG, placed by the scale drawn now.
    #[allow(clippy::too_many_arguments)]
    fn svg_detail(
        &mut self,
        id: &str,
        l: &Loaded,
        g: &Geo,
        (scale, pan): (f32, (f32, f32)),
        device: f32,
        settled: bool,
        cx: &mut gpui::Context<Self>,
    ) -> Option<(svg::Rect, std::sync::Arc<gpui::RenderImage>)> {
        let doc = l.svg.clone()?;
        let v = self.viewer.as_mut()?;
        let ds = scale * device;
        if ds <= 2.05 {
            v.retired.extend(v.detail.take().map(|d| d.image));
            return None;
        }
        let (w, h) = (g.dims.0 * scale, g.dims.1 * scale);
        let origin = (g.center.0 + pan.0 - w / 2.0, g.center.1 + pan.1 - h / 2.0);
        let region = svg::visible(g.dims, origin, scale, g.view, device)?;
        #[allow(clippy::cast_possible_truncation)]
        let px = |f: f32| f64::from(f).round() as i64;
        let key = (id.to_owned(), [px(ds * 1000.0), px(region.0 * ds), px(region.1 * ds), px(region.2 * ds), px(region.3 * ds)]);
        if settled && !v.detail_pending && v.detail.as_ref().is_none_or(|d| d.key != key) {
            v.detail_pending = true;
            let dims = g.dims;
            let made = cx.background_executor().spawn(async move { doc.render(dims, region, ds) });
            cx.spawn(async move |this, cx| {
                let image = made.await;
                let _ = this.update(cx, |this, cx| {
                    let Some(v) = this.viewer.as_mut() else { return };
                    v.detail_pending = false;
                    if let Some(image) = image.filter(|_| v.id == key.0) {
                        let old = v.detail.replace(Detail { key, region, image: std::sync::Arc::new(image) });
                        v.retired.extend(old.map(|d| d.image));
                    }
                    cx.notify();
                });
            })
            .detach();
        }
        v.detail.as_ref().filter(|d| d.key.0 == id).map(|d| (d.region, d.image.clone()))
    }

    /// The shown SVG as a file next to the cache (named as made), for the clipboard.
    fn svg_file(&self, media: &Media, l: &Loaded) -> Option<std::path::PathBuf> {
        let session = self.session_id.as_deref()?;
        let file = super::media::cache_file(&crate::cache::root(), session, &media.id, &media.name)
            .with_extension("")
            .join(media.name.replace(['/', ':'], "-"));
        if !file.exists() {
            std::fs::create_dir_all(file.parent()?).ok()?;
            std::fs::write(&file, l.image.bytes()).ok()?;
        }
        Some(file)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fits_without_upscaling_and_centres_what_fits() {
        let geo = Geo::new((800.0, 600.0), (1024.0, 1024.0));
        assert!((geo.fit - (600.0 - TOP - BOTTOM) / 1024.0).abs() < 1e-4);
        assert!((Geo::new((800.0, 600.0), (100.0, 50.0)).fit - 1.0).abs() < 1e-6, "never above 100 %");
        assert_eq!(geo.clamp(geo.fit, (50.0, -30.0)), (0.0, 0.0), "a fitted image stays centred");
        assert!(!geo.overflows(geo.fit) && geo.overflows(2.0));
    }

    #[test]
    fn a_video_fits_above_the_bar_or_fills_the_pane() {
        let geo = Geo::video((800.0, 600.0), (1280.0, 720.0), false);
        let h = 720.0 * geo.fit;
        assert!(geo.center.1 + h / 2.0 <= 600.0 - player::BAR_ROOM + 0.01, "clear of the bar");
        assert!(geo.center.1 - h / 2.0 >= VIDEO_TOP - 0.01, "clear of the title");
        let fill = Geo::video((800.0, 600.0), (1280.0, 720.0), true);
        assert!((fill.fit - 800.0 / 1280.0).abs() < 1e-5 && fill.center == (400.0, 300.0), "the whole pane, centred");
        assert!(Geo::video((800.0, 600.0), (64.0, 36.0), true).fit > 1.0, "upscaled");
    }

    #[test]
    fn zoom_keeps_the_point_under_the_pointer() {
        let geo = Geo::new((800.0, 600.0), (1000.0, 1000.0));
        let at = (geo.center.0 + 100.0, geo.center.1 + 50.0);
        let pan = geo.anchored(1.0, 2.0, (0.0, 0.0), at);
        // The image point under the pointer, before and after: (at - centre - pan) / scale.
        let before = ((at.0 - geo.center.0) / 1.0, (at.1 - geo.center.1) / 1.0);
        let after = ((at.0 - geo.center.0 - pan.0) / 2.0, (at.1 - geo.center.1 - pan.1) / 2.0);
        assert_eq!(before, after);
        // Panned past an edge, it stops there: the image still covers the view.
        let far = geo.clamp(2.0, (5000.0, -5000.0));
        assert_eq!(far, (1000.0 - geo.center.0, 600.0 - 1000.0 - geo.center.1));
    }
}
