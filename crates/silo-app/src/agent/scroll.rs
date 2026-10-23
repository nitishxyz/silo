//! Scroll chaining between the transcript and the boxes inside it (an open
//! `›` detail, a code block): a gesture belongs to whoever it started on.
//! Started over a box that can scroll that way, the whole gesture (momentum
//! included) scrolls the box, even past its edge or once the pointer left
//! it; otherwise it scrolls the transcript and the box stays put, as macOS
//! scroll views do. While a box takes a gesture the transcript doesn't
//! follow its tail.
//!
//! gpui's scroll containers never stop a wheel event, so the transcript list
//! (outside) scrolled along with any box. Each box records itself in the
//! capture phase (its offset before the event); a listener painted after
//! the list decides who owns the event; the box's own bubble listener (after
//! its div scrolled, before the list) then stops the event, or undoes the
//! box's scroll and lets it through.

use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{
    AnyElement, DispatchPhase, Div, InteractiveElement as _, IntoElement as _, ListState, ParentElement as _, Pixels, Point, ScrollHandle,
    ScrollWheelEvent, Stateful, StatefulInteractiveElement as _, Styled as _, TouchPhase, point, px,
};

/// Events this close together are one gesture (momentum follows a lift within a frame or two).
const GAP: Duration = Duration::from_millis(250);

/// Who takes a gesture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Owner {
    Transcript,
    Box(String),
}

/// Whether a box at `offset` (0 … -`max` on each axis) can move by `delta` along its main axis.
pub fn can_scroll(offset: (f32, f32), max: (f32, f32), delta: (f32, f32)) -> bool {
    let along = |o: f32, m: f32, d: f32| (d > 0.0 && o < -0.5) || (d < 0.0 && o > -m + 0.5);
    if delta.0.abs() > delta.1.abs() { along(offset.0, max.0, delta.0) } else { along(offset.1, max.1, delta.1) }
}

/// The owner of the gesture under way.
#[derive(Debug, Default)]
pub struct Latch {
    owner: Option<Owner>,
    last: Option<Instant>,
}

impl Latch {
    /// An event over box `over` (its id, and whether it can scroll that way; `None`: the transcript
    /// elsewhere). `start`: a new touch began; `moved`: it has a delta. `None`: nobody owns it yet.
    pub fn route(&mut self, over: Option<(&str, bool)>, start: bool, moved: bool, now: Instant) -> Option<Owner> {
        if start || self.last.is_none_or(|l| now.saturating_duration_since(l) > GAP) {
            self.owner = None;
        }
        if moved {
            self.last = Some(now);
            if self.owner.is_none() {
                self.owner = Some(match over {
                    Some((id, true)) => Owner::Box(id.to_owned()),
                    _ => Owner::Transcript,
                });
            }
        }
        self.owner.clone()
    }

    /// The system took the touch.
    pub fn cancel(&mut self) {
        (self.owner, self.last) = (None, None);
    }

    /// A box takes the gesture under way.
    pub fn boxed(&self, now: Instant) -> bool {
        matches!(self.owner, Some(Owner::Box(_))) && self.last.is_some_and(|l| now.saturating_duration_since(l) <= GAP)
    }
}

/// The box under the pointer for this event: its id, whether it can scroll that way, its offset before.
struct Over {
    id: String,
    can: bool,
    prev: Point<Pixels>,
}

/// The transcript's inner scrollers (one per pane).
pub struct Inner {
    handles: RefCell<HashMap<String, ScrollHandle>>,
    latch: RefCell<Latch>,
    over: RefCell<Option<Over>>,
    /// This event's decision, for the box under the pointer (its bubble listener).
    decided: RefCell<Option<(Over, Owner)>>,
    /// The composer column floats over the list's bottom this tall: boxes under it aren't under the pointer.
    inset: std::cell::Cell<f32>,
    list: ListState,
}

fn delta_of(event: &ScrollWheelEvent, window: &gpui::Window) -> Point<Pixels> {
    event.delta.pixel_delta(window.line_height())
}

fn f(p: Point<Pixels>) -> (f32, f32) {
    (f32::from(p.x), f32::from(p.y))
}

impl Inner {
    pub fn new(list: ListState) -> Rc<Self> {
        Rc::new(Self {
            handles: RefCell::default(),
            latch: RefCell::default(),
            over: RefCell::default(),
            decided: RefCell::default(),
            inset: std::cell::Cell::default(),
            list,
        })
    }

    /// The composer column's height over the list's bottom.
    pub fn set_inset(&self, inset: f32) {
        self.inset.set(inset);
    }

    /// The list's visible area, less what the composer covers.
    fn open(&self) -> gpui::Bounds<Pixels> {
        let mut b = self.list.viewport_bounds();
        b.size.height = (b.size.height - px(self.inset.get())).max(px(0.0));
        b
    }

    /// Another session: forget the boxes' offsets.
    pub fn clear(&self) {
        self.handles.borrow_mut().clear();
        self.latch.borrow_mut().cancel();
    }

    /// A box takes the gesture under way: the transcript doesn't follow its tail meanwhile.
    pub fn holding(&self, now: Instant) -> bool {
        self.latch.borrow().boxed(now)
    }

    pub(super) fn handle(&self, id: &str) -> ScrollHandle {
        self.handles.borrow_mut().entry(id.to_owned()).or_default().clone()
    }

    /// Make `el` (stateful, sized) a box that scrolls vertically, and horizontally with `x`.
    pub fn scroller(self: &Rc<Self>, id: &str, el: Stateful<Div>, x: bool) -> Stateful<Div> {
        let handle = self.handle(id);
        let el = if x { el.overflow_scroll() } else { el.overflow_y_scroll() };
        let (inner, key, h) = (self.clone(), id.to_owned(), handle.clone());
        // Capture phase: record this box when the event is over it (its bounds from this frame, inside the list).
        let probe = gpui::canvas(
            |_, _, _| {},
            move |_, _, window, _| {
                let (inner, key, h) = (inner.clone(), key.clone(), h.clone());
                window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, _| {
                    if phase != DispatchPhase::Capture || !h.bounds().contains(&event.position) || !inner.open().contains(&event.position) {
                        return;
                    }
                    let (prev, max) = (h.offset(), h.max_offset());
                    let can = can_scroll(f(prev), f(max), f(delta_of(event, window)));
                    *inner.over.borrow_mut() = Some(Over { id: key.clone(), can, prev });
                });
            },
        )
        .absolute()
        .size_0();
        let (inner, key) = (self.clone(), id.to_owned());
        el.track_scroll(&handle).child(probe).on_scroll_wheel(move |_, _, cx| {
            let decided = inner.decided.borrow_mut().take_if(|(over, _)| over.id == key);
            match decided {
                Some((_, Owner::Box(id))) if id == key => cx.stop_propagation(),
                // The transcript's gesture: the box stays where it was.
                Some((over, _)) => handle.set_offset(over.prev),
                None => {}
            }
        })
    }

    /// Painted after the transcript list (its capture listener runs after the boxes'): decides who owns each event.
    pub fn router(self: &Rc<Self>) -> AnyElement {
        let inner = self.clone();
        gpui::canvas(
            |_, _, _| {},
            move |_, _, window, _| {
                let inner = inner.clone();
                window.on_mouse_event(move |event: &ScrollWheelEvent, phase, window, cx| {
                    if phase != DispatchPhase::Capture {
                        return;
                    }
                    let over = inner.over.borrow_mut().take();
                    *inner.decided.borrow_mut() = None;
                    if event.touch_phase == TouchPhase::Cancelled {
                        inner.latch.borrow_mut().cancel();
                        return;
                    }
                    let delta = delta_of(event, window);
                    let moved = delta.x != px(0.0) || delta.y != px(0.0);
                    let start = event.touch_phase == TouchPhase::Started;
                    let owner =
                        inner.latch.borrow_mut().route(over.as_ref().map(|o| (o.id.as_str(), o.can)), start, moved, std::time::Instant::now());
                    match (owner, over) {
                        (Some(Owner::Box(id)), over) if over.as_ref().is_none_or(|o| o.id != id) => {
                            // The pointer left the box mid-gesture: the box keeps scrolling, nothing else does.
                            if let Some(h) = inner.handles.borrow().get(&id) {
                                let (o, m) = (h.offset(), h.max_offset());
                                let along_x = delta.x.abs() > delta.y.abs();
                                let next = if along_x {
                                    point((o.x + delta.x).clamp(-m.x, px(0.0)), o.y)
                                } else {
                                    point(o.x, (o.y + delta.y).clamp(-m.y, px(0.0)))
                                };
                                if next != o {
                                    h.set_offset(next);
                                    window.refresh();
                                }
                            }
                            inner.list.pause_following_tail();
                            cx.stop_propagation();
                        }
                        (Some(owner), Some(over)) => {
                            if matches!(owner, Owner::Box(_)) {
                                inner.list.pause_following_tail();
                            }
                            *inner.decided.borrow_mut() = Some((over, owner));
                        }
                        _ => {}
                    }
                });
            },
        )
        .absolute()
        .size_0()
        .into_any_element()
    }
}

/// A live window following its newest line: whether it's pinned to the bottom, and its overflow last frame.
/// It stays pinned while the content only grew (a line arrived); scrolled up off the bottom it stays where the
/// user put it until they scroll back down.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pin {
    pub pinned: bool,
    max: f32,
}

impl Default for Pin {
    fn default() -> Self {
        Self { pinned: true, max: 0.0 }
    }
}

impl Pin {
    /// This frame's offset (0 … -`max`) and overflow: the offset to set, if it follows.
    pub fn follow(&mut self, offset: f32, max: f32) -> Option<f32> {
        if max <= self.max + 0.5 {
            self.pinned = max < 0.5 || offset <= -max + 1.0;
        }
        self.max = max;
        self.pinned.then_some(-max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_live_window_follows_its_newest_line_until_scrolled_up() {
        let mut p = Pin::default();
        assert_eq!(p.follow(0.0, 0.0), Some(0.0), "nothing to scroll");
        // A line arrived (the overflow grew): still at the bottom.
        assert_eq!(p.follow(0.0, 24.0), Some(-24.0));
        assert_eq!(p.follow(-24.0, 48.0), Some(-48.0));
        // Scrolled up: it stays put, through new lines too.
        assert_eq!(p.follow(-20.0, 48.0), None);
        assert!(!p.pinned);
        assert_eq!(p.follow(-20.0, 72.0), None, "a new line doesn't pull it down");
        // Back at the bottom: it follows again.
        assert_eq!(p.follow(-72.0, 72.0), Some(-72.0));
        assert_eq!(p.follow(-72.0, 96.0), Some(-96.0));
    }

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn a_box_scrolls_only_toward_room_it_has() {
        // At the top of a 100 tall overflow: down (negative delta) yes, up no.
        assert!(can_scroll((0.0, 0.0), (0.0, 100.0), (0.0, -10.0)));
        assert!(!can_scroll((0.0, 0.0), (0.0, 100.0), (0.0, 10.0)));
        // At the bottom: the reverse.
        assert!(can_scroll((0.0, -100.0), (0.0, 100.0), (0.0, 10.0)));
        assert!(!can_scroll((0.0, -100.0), (0.0, 100.0), (0.0, -10.0)));
        // Nothing to scroll vertically (a code block): vertical goes on, horizontal is taken.
        assert!(!can_scroll((0.0, 0.0), (300.0, 0.0), (0.0, -10.0)));
        assert!(can_scroll((0.0, 0.0), (300.0, 0.0), (-10.0, -2.0)), "the main axis decides");
        assert!(!can_scroll((-300.0, 0.0), (300.0, 0.0), (-10.0, 0.0)), "at the right edge");
    }

    #[test]
    fn a_gesture_stays_with_what_it_started_on() {
        let now = Instant::now();
        let mut l = Latch::default();
        // Began over a box that can scroll: the box, even at its edge and once the pointer left it.
        assert_eq!(l.route(Some(("a", true)), true, true, now), Some(Owner::Box("a".into())));
        assert_eq!(l.route(Some(("a", false)), false, true, now + 16 * MS), Some(Owner::Box("a".into())));
        assert_eq!(l.route(None, false, true, now + 32 * MS), Some(Owner::Box("a".into())));
        assert!(l.boxed(now + 32 * MS));
        // Momentum after the lift continues it; a pause ends it.
        assert_eq!(l.route(Some(("b", true)), false, true, now + 48 * MS), Some(Owner::Box("a".into())));
        let later = now + Duration::from_secs(1);
        assert!(!l.boxed(later), "the transcript may follow again");
        assert_eq!(l.route(Some(("a", false)), false, true, later), Some(Owner::Transcript), "at its edge: the transcript");
        // Began on the transcript: passing over a box doesn't take it.
        assert_eq!(l.route(Some(("b", true)), false, true, later + 16 * MS), Some(Owner::Transcript));
        // A new touch routes afresh.
        assert_eq!(l.route(Some(("b", true)), true, true, later + 32 * MS), Some(Owner::Box("b".into())));
    }

    #[test]
    fn an_empty_begin_waits_for_the_first_delta() {
        let now = Instant::now();
        let mut l = Latch::default();
        assert_eq!(l.route(None, true, false, now), None);
        assert_eq!(l.route(Some(("a", true)), false, true, now + MS), Some(Owner::Box("a".into())));
        l.cancel();
        assert!(!l.boxed(now + MS));
        assert_eq!(l.route(None, false, true, now + 2 * MS), Some(Owner::Transcript));
    }
}
