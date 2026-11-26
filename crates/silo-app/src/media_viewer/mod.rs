//! The media viewer (`docs/MEDIA.md` §5.3, §6.4), one component hosted by the agent block and the Studio block:
//! an image or video over the host on a dimmed backdrop, fit to it; images zoom around the pointer (pinch,
//! scroll, ⌘-scroll, `+` `-`, 10 %–800 %; `0` fits, `1` is 100 %, double-click toggles) and drag to pan; a video
//! plays in gpui (`player/`); ←/→ (`[` `]` for a video, whose ←/→ seek) the host's other images and videos;
//! copy, save, close. The backdrop is opaque once open: nothing of the host shows through.
//!
//! The host gives a [`Source`] (what it can show, in order, and each item's media, fetched on first ask) and
//! hears [`ViewerEvent`]s (closed, gone, stepped, a note). It draws the entity over itself, last.

mod geo;
pub(crate) mod image;
mod player;

use std::{cell::Cell, path::PathBuf, rc::Rc, sync::Arc, time::Instant};

use gpui::{App, Bounds, Context, EventEmitter, FocusHandle, Pixels, Point, Render, Window};

pub use self::geo::{Geo, MAX, MIN};
pub(crate) use self::player::surface;
use self::{geo::SIDE, image::Detail};
use crate::agent::{
    av,
    media::{Clip, Entry, Loaded},
    view::{eased, keep_moving, toggled},
};

/// Opening and closing (the sign-in dialog's), and a zoom by key or button gliding.
pub(crate) const ENTER: std::time::Duration = std::time::Duration::from_millis(200);
pub(crate) const EXIT: std::time::Duration = std::time::Duration::from_millis(160);
pub(crate) const GLIDE: std::time::Duration = std::time::Duration::from_millis(180);

/// What an item is shown as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Image,
    Video,
}

/// One thing the viewer can show and step to, with what its footer says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// Stable: what stepping and the host's events name (an attachment, a node).
    pub id: String,
    /// The media it shows now; a change under the same id (a card's new take) starts the viewer over on it.
    pub key: String,
    /// The file's name (Save… offers it).
    pub name: String,
    /// What the footer names it by when not its file (a card's title, an entity's name).
    pub title: Option<String>,
    pub kind: Kind,
    /// `image/png`; its subtype is the footer's format.
    pub mime: String,
    /// As people read it (`1.2 MB`).
    pub size: Option<String>,
    /// Pixels, when known before it loads.
    pub dims: Option<(u32, u32)>,
    /// Playing time in ms.
    pub ms: Option<u64>,
    /// Provider and model (`OpenAI gpt-image-2.5`).
    pub credit: Option<String>,
    pub prompt: Option<String>,
    /// A folder for copies named as made (an SVG for the clipboard).
    pub scratch: Option<PathBuf>,
}

/// What the host gives the viewer.
pub trait Source: 'static {
    /// What it can show and step through, in order.
    fn items(&self, cx: &App) -> Vec<Item>;
    /// `item`'s media: decoded image, video file, or still loading (asked again on the next draw).
    fn media(&self, item: &Item, cx: &mut App) -> Entry;
}

/// What the viewer tells its host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ViewerEvent {
    /// It began to close. Focus went back where it was before it opened, unless `refocus`: then it is the host's
    /// to give.
    Closed { refocus: bool },
    /// Faded out: drop it.
    Gone,
    /// It shows item `id` now.
    Stepped(String),
    /// A passing word in its toolbar (`Copied`, `Saved`, `Couldn't save`).
    Note(&'static str),
}

/// The item `dir` steps from `at` in `items` (`None` past either end, or when `at` isn't there).
#[must_use]
pub fn stepped<'a>(items: &'a [Item], at: &str, dir: i32) -> Option<&'a Item> {
    let ix = items.iter().position(|s| s.id == at)?;
    ix.checked_add_signed(dir as isize).and_then(|i| items.get(i)).filter(|_| dir != 0)
}

pub struct MediaViewer {
    source: Rc<dyn Source>,
    /// The item shown, and the media key it showed last (`None` before the first draw).
    id: String,
    key: Option<String>,
    focus: FocusHandle,
    /// Focus before it opened, given back when it closes.
    back: Option<FocusHandle>,
    /// The scale (1 is 100 %); `None` fits.
    zoom: Option<f32>,
    /// The image centre's offset from the view's.
    pan: (f32, f32),
    /// The scale and pan a glide starts from, and when.
    from: Option<(Instant, f32, (f32, f32))>,
    /// A drag's start: pointer and pan.
    drag: Option<(Point<Pixels>, (f32, f32))>,
    shown: Instant,
    closing: Option<Instant>,
    /// Faded out (`ViewerEvent::Gone` sent).
    gone: bool,
    /// The backdrop's bounds as last painted (window coordinates).
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// A passing word in the toolbar (`Copied`, `Saved`, `Couldn't save`), and since when.
    note: Option<(&'static str, Instant)>,
    /// The shown video's player, and what carries to the next video (volume, loop, fill).
    video: Option<player::Playback>,
    prefs: player::Prefs,
    /// A closing video's last frame, fading out (a surface has no opacity).
    still: Option<Arc<gpui::RenderImage>>,
    /// An SVG's sharp raster of what shows, the one being made, and old ones to drop from the atlas.
    detail: Option<Detail>,
    detail_pending: bool,
    retired: Vec<Arc<gpui::RenderImage>>,
}

impl EventEmitter<ViewerEvent> for MediaViewer {}

impl MediaViewer {
    /// Open on item `id` of `source`; it takes key focus.
    pub fn new(source: Rc<dyn Source>, id: &str, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        let back = window.focused(cx);
        window.focus(&focus, cx);
        Self {
            source,
            id: id.to_owned(),
            key: None,
            focus,
            back,
            zoom: None,
            pan: (0.0, 0.0),
            from: None,
            drag: None,
            shown: Instant::now(),
            closing: None,
            gone: false,
            bounds: Rc::default(),
            note: None,
            video: None,
            prefs: player::Prefs::default(),
            still: None,
            detail: None,
            detail_pending: false,
            retired: Vec::new(),
        }
    }

    /// Open and not on its way out.
    #[must_use]
    pub fn open(&self) -> bool {
        self.closing.is_none()
    }

    /// Fade out and give focus back.
    pub fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.closing.is_some() {
            return;
        }
        self.closing = Some(Instant::now());
        self.drag = None;
        if let Some(p) = &mut self.video {
            p.video.player.pause();
            p.drag = None;
            self.still = p.video.frame().and_then(|f| av::still(&f)).map(Arc::new);
        }
        let back = self.back.take();
        if let Some(back) = &back {
            window.focus(back, cx);
        }
        cx.emit(ViewerEvent::Closed { refocus: back.is_none() });
        cx.notify();
    }

    /// The shown item, while the host still has it.
    fn item(&self, cx: &App) -> Option<Item> {
        self.source.items(cx).into_iter().find(|s| s.id == self.id)
    }

    /// The shown item's media.
    fn entry(&self, item: &Item, cx: &mut App) -> Entry {
        let source = self.source.clone();
        source.media(item, cx)
    }

    /// The shown image's or video's geometry, once the viewer was painted and its size is known.
    pub fn geo(&self, cx: &mut App) -> Option<Geo> {
        let b = self.bounds.get()?;
        let item = self.item(cx)?;
        let (dims, video) = match self.entry(&item, cx) {
            Entry::Ready(l) => (l.dims, false),
            Entry::Clip(c) => (item.dims.or(c.poster.as_ref().map(|p| p.1)).unwrap_or((1280, 720)), true),
            _ => {
                let video = item.kind == Kind::Video;
                (item.dims.or(video.then_some((1280, 720)))?, video)
            }
        };
        #[allow(clippy::cast_precision_loss)]
        let (view, dims) = ((f32::from(b.size.width), f32::from(b.size.height)), (dims.0 as f32, dims.1 as f32));
        Some(if video { Geo::video(view, dims, self.prefs.fill) } else { Geo::new(view, dims) })
    }

    /// The shown item is a video (no zoom).
    pub fn viewing_video(&self, cx: &App) -> bool {
        self.item(cx).is_some_and(|s| s.kind == Kind::Video)
    }

    /// The scale and pan drawn now: the target, or on the way to it.
    pub fn shown_at(&self, geo: &Geo, now: Instant, cx: &App) -> (f32, (f32, f32)) {
        let scale = self.zoom.unwrap_or(geo.fit);
        match self.from.and_then(|(at, s, p)| Some((eased(at, GLIDE, now, cx)?, s, p))) {
            Some((t, s, p)) => (s + (scale - s) * t, (p.0 + (self.pan.0 - p.0) * t, p.1 + (self.pan.1 - p.1) * t)),
            None => (scale, self.pan),
        }
    }

    /// Zoom to `to` (`None` fits) around `at` (window coordinates; the centre when `None`), gliding there for
    /// keys and buttons.
    pub fn zoom_to(&mut self, to: Option<f32>, at: Option<Point<Pixels>>, glide: bool, cx: &mut Context<Self>) {
        let Some(geo) = self.geo(cx) else { return };
        let now = Instant::now();
        let shown = self.shown_at(&geo, now, cx);
        let from = self.zoom.unwrap_or(geo.fit);
        let origin = self.bounds.get().map_or(Point::default(), |b| b.origin);
        let at = at.map_or(geo.center, |p| (f32::from(p.x - origin.x), f32::from(p.y - origin.y)));
        self.from = (glide && !cx.reduce_motion()).then_some((now, shown.0, shown.1));
        match to {
            None => (self.zoom, self.pan) = (None, (0.0, 0.0)),
            Some(to) => {
                let to = to.clamp(MIN, MAX);
                self.pan = geo.anchored(from, to, self.pan, at);
                self.zoom = Some(to);
            }
        }
        cx.notify();
    }

    /// The current scale (fit resolved).
    pub fn scale(&self, cx: &mut App) -> Option<f32> {
        let geo = self.geo(cx)?;
        Some(self.zoom.unwrap_or(geo.fit))
    }

    /// Back to fit, the player and the SVG raster dropped: a new item or new media.
    fn start_over(&mut self) {
        (self.zoom, self.pan, self.from, self.drag, self.video) = (None, (0.0, 0.0), None, None, None);
        self.retired.extend(self.detail.take().map(|d| d.image));
    }

    /// The previous (`-1`) or next (`1`) item, fitted.
    pub fn step(&mut self, dir: i32, cx: &mut Context<Self>) {
        let all = self.source.items(cx);
        let Some(next) = stepped(&all, &self.id, dir) else { return };
        self.id.clone_from(&next.id);
        self.key = Some(next.key.clone());
        self.start_over();
        cx.emit(ViewerEvent::Stepped(next.id.clone()));
        cx.notify();
    }
}

impl MediaViewer {
    /// Keys while the viewer has focus; none reach the host.
    pub fn key(&mut self, event: &gpui::KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let k = &event.keystroke;
        let m = &k.modifiers;
        let scale = self.scale(cx);
        let video = self.viewing_video(cx);
        if video && let Some(key) = player::map(&k.key, m.platform, m.control) {
            self.player_key(key, window, cx);
            return cx.stop_propagation();
        }
        match (k.key.as_str(), m.platform) {
            ("escape", _) => self.close(window, cx),
            ("left" | "[", false) => self.step(-1, cx),
            ("right" | "]", false) => self.step(1, cx),
            ("=" | "+" | "-" | "0" | "1", _) if video => {}
            ("=" | "+", _) => self.zoom_to(scale.map(|s| crate::files::zoom::stepped(s, 1, MIN, MAX)), None, true, cx),
            ("-", _) => self.zoom_to(scale.map(|s| crate::files::zoom::stepped(s, -1, MIN, MAX)), None, true, cx),
            ("0", _) => self.zoom_to(None, None, true, cx),
            ("1", false) => self.zoom_to(Some(1.0), None, true, cx),
            ("c", true) => self.copy(cx),
            ("s", true) => self.save(cx),
            // Window and app chords (⌘W, ⌘Q, …) go on.
            _ if m.platform || m.control => return,
            _ => {}
        }
        cx.stop_propagation();
    }

    /// Scroll: ⌘-scroll zooms, and so does plain scroll while the image fits; once it overflows, scroll pans.
    pub fn wheel(&mut self, event: &gpui::ScrollWheelEvent, cx: &mut Context<Self>) {
        if self.viewing_video(cx) {
            return;
        }
        let (Some(geo), Some(scale)) = (self.geo(cx), self.scale(cx)) else { return };
        let d = event.delta.pixel_delta(gpui::px(16.0));
        let (dx, dy) = (f32::from(d.x), f32::from(d.y));
        if event.modifiers.platform || !geo.overflows(scale) {
            if dy.abs() > 0.0 {
                self.zoom_to(Some(scale * (dy * 0.006).exp()), Some(event.position), false, cx);
            }
        } else {
            self.from = None;
            self.pan = geo.clamp(scale, (self.pan.0 + dx, self.pan.1 + dy));
            cx.notify();
        }
    }

    pub fn pinch(&mut self, event: &gpui::PinchEvent, cx: &mut Context<Self>) {
        if self.viewing_video(cx) {
            return;
        }
        if let Some(scale) = self.scale(cx) {
            self.zoom_to(Some(crate::files::zoom::pinched(scale, event.delta, MIN, MAX)), Some(event.position), false, cx);
        }
    }

    /// A press on the image: a double-click toggles fit / 100 % at the pointer, else a drag starts.
    pub fn press(&mut self, event: &gpui::MouseDownEvent, cx: &mut Context<Self>) {
        cx.stop_propagation();
        // A video: a click plays or pauses; a double-click (its first click undone) fills or fits.
        if self.viewing_video(cx) {
            self.player_activity(cx);
            self.player_toggle(cx);
            if event.click_count == 2 {
                self.player_fill(cx);
            }
            return;
        }
        let (Some(geo), Some(scale)) = (self.geo(cx), self.scale(cx)) else { return };
        if event.click_count == 2 {
            let to = if (scale - 1.0).abs() < 0.01 && geo.fit < 0.99 { None } else { Some(1.0) };
            return self.zoom_to(to, Some(event.position), true, cx);
        }
        self.drag = Some((event.position, self.pan));
        self.from = None;
    }

    pub fn moved(&mut self, event: &gpui::MouseMoveEvent, cx: &mut Context<Self>) {
        if let Some(p) = self.video.as_mut() {
            p.pointer_in = self.bounds.get().is_some_and(|b| b.contains(&event.position));
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
        let Some((start, pan)) = self.drag else { return };
        let (Some(geo), Some(scale)) = (self.geo(cx), self.scale(cx)) else { return };
        if event.pressed_button != Some(gpui::MouseButton::Left) {
            self.drag = None;
            return cx.notify();
        }
        let d = event.position - start;
        self.pan = geo.clamp(scale, (pan.0 + f32::from(d.x), pan.1 + f32::from(d.y)));
        cx.notify();
    }

    /// A release anywhere: a pan or a player drag ends.
    fn released(&mut self, cx: &mut Context<Self>) {
        if self.drag.take().is_some() {
            cx.notify();
        }
        if self.video.as_ref().is_some_and(|p| p.drag.is_some()) {
            self.player_release(cx);
        }
    }
}

/// The backdrop's blur while it fades.
const BLUR: f32 = 24.0;
/// Chrome over the dark backdrop, the same in light and dark themes.
const INK: u32 = 0x00f2_f2f2;
const DIM: u32 = 0x00a3_a3a3;
/// How long a toolbar note shows.
const NOTE: std::time::Duration = std::time::Duration::from_millis(1400);

impl MediaViewer {
    /// The viewer, while open or fading out (`None` once gone).
    #[allow(clippy::too_many_lines)]
    fn render_viewer(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        use gpui::{MouseButton, div, prelude::*, px, rgb};
        if self.gone {
            return None;
        }
        let now = Instant::now();
        let all = self.source.items(cx);
        let ix = all.iter().position(|s| s.id == self.id);
        let t = match self.closing {
            Some(at) => 1.0 - eased(at, EXIT, now, cx).unwrap_or(1.0),
            None => eased(self.shown, ENTER, now, cx).unwrap_or(1.0),
        };
        // Closed, or its item left the host (another session, a node deleted).
        let Some(ix) = ix.filter(|_| t > 0.0 || self.closing.is_none()) else {
            if self.closing.is_none() {
                self.close(window, cx);
            }
            self.gone = true;
            self.video = None;
            for image in std::mem::take(&mut self.retired).into_iter().chain(self.detail.take().map(|d| d.image)).chain(self.still.take()) {
                let _ = window.drop_image(image);
            }
            cx.emit(ViewerEvent::Gone);
            return None;
        };
        let shown = all[ix].clone();
        // New media under the same item (a card's new take): start over on it.
        if self.key.as_ref().is_some_and(|k| *k != shown.key) {
            self.start_over();
        }
        self.key = Some(shown.key.clone());
        let key = shown.key.clone();
        let (closing, bounds, focus) = (self.closing.is_some(), self.bounds.clone(), self.focus.clone());
        let note = self.note.filter(|(_, at)| now.duration_since(*at) < NOTE).map(|(n, _)| n);
        let dragging = self.drag.is_some();
        let settled = !dragging && t >= 1.0 && self.from.is_none_or(|(at, ..)| now.duration_since(at) >= GLIDE);
        let entry = self.entry(&shown, cx);
        let video = shown.kind == Kind::Video;
        if let (true, Entry::Clip(c)) = (video, &entry) {
            self.ensure_playback(&key, c, cx);
        }
        // The video plays once the viewer is open; its frame for now (the poster stands in before the first,
        // and a still of the last while closing: a surface draws at full opacity).
        let frame = match self.video.as_mut().filter(|p| p.id == key) {
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
        let geo = self.geo(cx);
        let (scale, pan) = geo.map_or((1.0, (0.0, 0.0)), |g| self.shown_at(&g, now, cx));
        let detail = match (&entry, geo) {
            (Entry::Ready(l), Some(g)) => self.svg_detail(&key, l, &g, (scale, pan), window.scale_factor(), settled, cx),
            _ => None,
        };
        for image in std::mem::take(&mut self.retired) {
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
                        let _ = moved.update(cx, |this, cx| this.moved(e, cx));
                    }
                });
                window.on_mouse_event(move |_: &gpui::MouseUpEvent, phase, _, cx| {
                    if phase == gpui::DispatchPhase::Bubble {
                        let _ = up.update(cx, |this, cx| this.released(cx));
                    }
                });
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full();
        // The image: opening grows it from 96 %; while it loads, a pulse at its size; failed, a quiet line.
        let still = self.still.clone();
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
                .when(!closing, |el| el.on_mouse_down(MouseButton::Left, cx.listener(|this, e, _, cx| this.press(e, cx))))
                .into_any_element()
        });
        let failed = matches!(entry, Entry::Failed).then(|| {
            div().absolute().size_full().flex().items_center().justify_center().text_size(px(13.0)).text_color(rgb(DIM)).child(if video {
                "Video unavailable"
            } else {
                "Image unavailable"
            })
        });
        let toolbar = self.toolbar(scale, note, matches!(entry, Entry::Ready(_) | Entry::Clip(_)), video, cx);
        let footer = image::footer(&shown, all.len(), ix, geo.map(|g| g.dims).filter(|_| !video));
        let sides = (all.len() > 1).then(|| Self::sides(ix, all.len(), cx));
        let width = self.bounds.get().map_or(0.0, |b| f32::from(b.size.width));
        // A video's chrome: the title and Close on top, the bar at the bottom, over scrims; faded together, and
        // hidden (with the pointer) while it plays untouched. An image's: the toolbar, the footer, the sides.
        let prefs = self.prefs;
        let playback = self.video.as_ref().filter(|p| p.id == key && video);
        let chrome: Vec<gpui::AnyElement> = match playback {
            Some(p) => {
                let alpha = t * toggled(p.hide.fade(), now, cx);
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
        // Opaque once open (nothing of the host shows through), blurring what's behind on the way in and out.
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
                    el.on_key_down(cx.listener(|this, e, window, cx| this.key(e, window, cx)))
                        .on_scroll_wheel(cx.listener(|this, e, _, cx| this.wheel(e, cx)))
                        .on_pinch(cx.listener(|this, e, _, cx| this.pinch(e, cx)))
                        .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| this.close(window, cx)))
                })
                .children(picture)
                .children(failed)
                .children(chrome)
                .into_any_element(),
        )
    }
}

impl Render for MediaViewer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        use gpui::IntoElement as _;
        let el = self.render_viewer(window, cx).unwrap_or_else(|| gpui::Empty.into_any_element());
        // Its fades and glides ask for the next frame (its host's draw has already looked).
        keep_moving(window);
        el
    }
}

/// The shown video, written for AVFoundation; the shown image, decoded.
impl MediaViewer {
    fn viewed(&self, cx: &mut App) -> Option<(Item, Arc<Loaded>)> {
        let item = self.item(cx)?;
        let Entry::Ready(l) = self.entry(&item, cx) else { return None };
        Some((item, l))
    }

    fn viewed_clip(&self, cx: &mut App) -> Option<(Item, Arc<Clip>)> {
        let item = self.item(cx)?;
        let Entry::Clip(c) = self.entry(&item, cx) else { return None };
        Some((item, c))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str) -> Item {
        Item {
            id: id.to_owned(),
            key: id.to_owned(),
            name: format!("{id}.png"),
            title: None,
            kind: Kind::Image,
            mime: "image/png".to_owned(),
            size: None,
            dims: None,
            ms: None,
            credit: None,
            prompt: None,
            scratch: None,
        }
    }

    #[test]
    fn steps_stop_at_the_ends() {
        let items = [item("a"), item("b"), item("c")];
        assert_eq!(stepped(&items, "b", 1).map(|i| i.id.as_str()), Some("c"));
        assert_eq!(stepped(&items, "b", -1).map(|i| i.id.as_str()), Some("a"));
        assert_eq!(stepped(&items, "a", -1), None, "before the first: stays");
        assert_eq!(stepped(&items, "c", 1), None, "after the last: stays");
        assert_eq!(stepped(&items, "x", 1), None, "not there");
        assert_eq!(stepped(&items[..1], "a", 1), None, "a single item");
    }
}
