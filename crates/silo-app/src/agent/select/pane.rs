//! The selection controller's state and gestures, independent of the pane (a test view drives it too).

use std::{cell::RefCell, collections::HashMap, rc::Rc};

use gpui::{Bounds, FocusHandle, Pixels, Point, Task};

use super::{Anchor, Leaf, Pos, Selection, Source, Unit, block_leaves, user_leaf, view::Registry};
use crate::agent::{markdown::Doc, transcript::Row};

/// A press in the transcript, until the button goes up.
struct Drag {
    from: Point<Pixels>,
    /// Past the drag threshold (or a double/triple/shift click, which select on the press).
    moved: bool,
    /// Pressed outside any text: where a drag from there starts.
    origin: Option<Anchor>,
}

/// The pane's one selection controller.
pub(in crate::agent) struct Select {
    pub sel: Option<Selection>,
    pub registry: Rc<RefCell<Registry>>,
    /// Holds keyboard focus while there's a selection (⌘C, ⌘A); typing moves on to the composer.
    pub focus: FocusHandle,
    /// The selection's bounds as of this frame's render (rows drawn read their part from it).
    pub frame: Option<(Pos, Pos)>,
    drag: Option<Drag>,
    pub pointer: Point<Pixels>,
    /// Points per tick the transcript scrolls while a drag is past its top or bottom edge.
    pub speed: f32,
    pub autoscroll: Option<Task<()>>,
    /// The transcript's bounds as last painted.
    pub viewport: Rc<std::cell::Cell<Option<Bounds<Pixels>>>>,
    /// The session the selection is in (a change clears it).
    pub session: Option<Option<String>>,
}

impl Select {
    pub fn new(focus: FocusHandle) -> Self {
        Self {
            sel: None,
            registry: Rc::default(),
            focus,
            frame: None,
            drag: None,
            pointer: Point::default(),
            speed: 0.0,
            autoscroll: None,
            viewport: Rc::default(),
            session: None,
        }
    }

    pub fn dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// The left button went down at `p` (`clicks`: its click count).
    pub fn down(&mut self, src: &dyn Source, p: Point<Pixels>, clicks: usize, shift: bool) {
        let registry = self.registry.borrow();
        self.pointer = p;
        if shift && let Some(sel) = &mut self.sel {
            if let Some(head) = registry.hit(p, false) {
                sel.head = head;
            }
            self.drag = Some(Drag { from: p, moved: true, origin: None });
            return;
        }
        match registry.hit(p, true) {
            Some(at) => {
                let unit = match clicks {
                    0 | 1 => Unit::Char,
                    2 => Unit::Word,
                    _ => Unit::Block,
                };
                self.sel = Some(Selection::new(src, at, unit));
                self.drag = Some(Drag { from: p, moved: unit != Unit::Char, origin: None });
            }
            None => self.drag = Some(Drag { from: p, moved: false, origin: registry.hit(p, false) }),
        }
    }

    /// The pointer is at `p` with the button held: true when the selection changed.
    pub fn drag_to(&mut self, src: &dyn Source, p: Point<Pixels>) -> bool {
        self.pointer = p;
        let Some(drag) = &mut self.drag else { return false };
        let (dx, dy) = (f32::from(p.x - drag.from.x), f32::from(p.y - drag.from.y));
        if !drag.moved && dx.hypot(dy) < 3.0 {
            return false;
        }
        drag.moved = true;
        let Some(head) = self.registry.borrow().hit(p, false) else { return false };
        if let Some(origin) = drag.origin.take() {
            self.sel = Some(Selection::new(src, origin, Unit::Char));
        }
        match &mut self.sel {
            Some(sel) if sel.head != head => {
                sel.head = head;
                true
            }
            _ => false,
        }
    }

    /// The button went up: true when a selection stays (a click without a drag clears it).
    pub fn up(&mut self, src: &dyn Source) -> bool {
        self.speed = 0.0;
        let Some(drag) = self.drag.take() else { return false };
        if !drag.moved || self.sel.as_ref().and_then(|s| s.bounds(src)).is_none_or(|(a, b)| a == b) {
            self.sel = None;
        }
        self.sel.is_some()
    }

    pub fn clear(&mut self) {
        self.sel = None;
        self.frame = None;
        self.drag = None;
        self.speed = 0.0;
    }
}

/// How fast a drag at `y` scrolls a transcript showing `top..bottom`: up above it, down below, faster further out.
pub(super) fn edge_speed(y: f32, top: f32, bottom: f32) -> f32 {
    if y < top {
        -((top - y) * 0.5).clamp(2.0, 40.0)
    } else if y > bottom {
        ((y - bottom) * 0.5).clamp(2.0, 40.0)
    } else {
        0.0
    }
}

/// The pane's rows as selection sees them: user bubbles and assistant texts (as last parsed) have leaves.
pub(super) struct Rows<'a> {
    pub rows: &'a [Row],
    pub parsed: &'a HashMap<String, Doc>,
}

impl Source for Rows<'_> {
    fn len(&self) -> usize {
        self.rows.len()
    }

    fn key(&self, row: usize) -> &str {
        self.rows[row].key()
    }

    fn leaves(&self, row: usize) -> Vec<Leaf> {
        match &self.rows[row] {
            Row::User { text, .. } => user_leaf(text).into_iter().collect(),
            Row::Text { key, text, streaming } => match self.parsed.get(key) {
                Some(doc) => block_leaves(key, doc.blocks()),
                None => {
                    let mut doc = Doc::default();
                    doc.update(text, *streaming);
                    block_leaves(key, doc.blocks())
                }
            },
            Row::Group(_) | Row::Marker { .. } | Row::Report { .. } => Vec::new(),
        }
    }
}
