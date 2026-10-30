//! Titlebar: one inline row with the traffic-light gap, the session pill,
//! compact window tabs and, at the far right, the Update pill (while an
//! update downloads, installs or is ready), the background-blocks button (while the session has
//! any, `background.rs`), the appearance toggle and the "+" button, all on the
//! terminal's own background. Tabs are small
//! pills: inactive ones filled only on hover, the current one a subtle step
//! brighter.
//!
//! When the tabs do not fit, only they scroll horizontally (trackpad, or a
//! mouse wheel's vertical motion); the pill and the right-hand buttons stay
//! put. Soft fades in the titlebar color mark a side with hidden tabs, and
//! the active tab is scrolled fully into view when it changes.
//!
//! With side tabs (`titlebar/sidebar.rs`) the strip's place is empty
//! titlebar, and a sidebar button after the traffic lights hides and shows
//! the sidebar.

use std::time::Instant;

use gpui::{AnyElement, Context, DragMoveEvent, FontWeight, IntoElement, Window, canvas, div, prelude::*, px, rgb};
use silo_proto::control::Method;

use crate::surface::Glassy as _;
use crate::theme::Continuous as _;
use crate::{
    assets::icon,
    hosts::TabPosition,
    motion::{self, TabGhost},
    theme,
    workspace::{TITLEBAR_HEIGHT, Workspace},
};

mod sidebar;

pub use sidebar::SIDEBAR_DEFAULT;

/// Width reserved for the traffic lights.
pub const TRAFFIC_LIGHTS: f32 = 86.0;
/// Height of the titlebar's icon buttons (appearance, "+").
pub const PILL_HEIGHT: f32 = 30.0;
/// Height of a tab entry; the session pill matches it.
const TAB_HEIGHT: f32 = 26.0;
/// Space between tabs.
const TAB_GAP: f32 = 4.0;
/// Longest tab, title ellipsized beyond it.
const TAB_MAX: f32 = 190.0;
/// Trailing padding inside a tab pill.
const TAB_PAD: f32 = 10.0;
/// The tab's icon, and its inset from the pill's left edge: the same as
/// from its top and bottom, so it sits centred in the pill's rounded end.
const TAB_ICON: f32 = 12.0;
const ICON_INSET: f32 = (TAB_HEIGHT - TAB_ICON) / 2.0;
/// A shown close button sits at the pill's left end this far from its
/// left, top and bottom edges alike, so it is concentric with the pill's
/// rounded end.
const CLOSE_INSET: f32 = 4.0;
/// Close button diameter: the pill's height less the inset above and below.
const CLOSE_SIZE: f32 = TAB_HEIGHT - 2.0 * CLOSE_INSET;
/// How far the close button reaches past the icon it replaces on each side.
const CLOSE_OVERHANG: f32 = (CLOSE_SIZE - TAB_ICON) / 2.0;
/// The side tabs' space before their close button.
const CLOSE_GAP: f32 = 4.0;
/// Gap between the session pill and the first tab.
const PILL_GAP: f32 = TAB_GAP;
/// The update pill: its height, its text's side padding, its border, and
/// with a spinner, the spinner's left padding (inside the border) that puts
/// it as far from the left as from the top and bottom, and the gap after it
/// (a tab's icon-to-title gap).
const UPDATE_HEIGHT: f32 = TAB_HEIGHT - 2.0;
const UPDATE_PAD: f32 = 11.0;
const UPDATE_BORDER: f32 = 1.0;
const UPDATE_ICON_PAD: f32 = (UPDATE_HEIGHT - TAB_ICON) / 2.0 - UPDATE_BORDER;
const UPDATE_GAP: f32 = 6.0;
/// Left inset in fullscreen, where there are no traffic lights.
const FULLSCREEN_INSET: f32 = 10.0;
/// Width of the edge fade over a side with hidden tabs.
const FADE_WIDTH: f32 = 28.0;
/// Space kept between a revealed tab and the strip's edge, clear of the
/// fade.
const REVEAL_MARGIN: f32 = FADE_WIDTH;
/// Fraction of the remaining distance a reveal scrolls per frame.
const REVEAL_EASE: f32 = 0.35;
/// Frames a reveal may wait for the active tab's layout before giving up.
const REVEAL_FRAMES: u16 = 90;

/// The scroll offset (`<= 0`, GPUI convention) that shows the content span
/// `item_left..item_left + item_width` (relative to the unscrolled content
/// start) with `margin` to spare inside a viewport `width` wide, moving as
/// little as possible: unchanged if it is already in view. Clamped to the
/// scrollable range `0..=max`.
#[must_use]
pub fn reveal_offset(offset: f32, width: f32, item_left: f32, item_width: f32, margin: f32, max: f32) -> f32 {
    let mut scroll = -offset;
    let margin = margin.min(((width - item_width) / 2.0).max(0.0));
    if item_left - margin < scroll || item_width + 2.0 * margin > width {
        scroll = item_left - margin;
    } else if item_left + item_width + margin > scroll + width {
        scroll = item_left + item_width + margin - width;
    }
    -scroll.clamp(0.0, max.max(0.0))
}

/// Which edges hide tabs (left, right) at a scroll offset in `-max..=0`.
#[must_use]
pub fn edge_fades(offset: f32, max: f32, room: f32) -> (bool, bool) {
    // Room left after the strip: it isn't overflowing, whatever its scroll
    // range says (a tab growing in or collapsing out briefly reports one).
    if room > 0.5 {
        return (false, false);
    }
    let scrolled = -offset;
    (scrolled > 0.5, scrolled < max - 0.5)
}

/// One frame of an eased scroll from `from` toward `to`; snaps when close.
#[must_use]
pub fn ease_toward(from: f32, to: f32) -> f32 {
    if (to - from).abs() < 0.5 { to } else { from + (to - from) * REVEAL_EASE }
}

/// Keep a press on a titlebar control from moving the window.
fn claim_press<E: InteractiveElement>(el: E) -> E {
    el.on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
}

/// Where a tab dragged from `from` lands when dropped before (or `after`)
/// the tab at `target`: its index once removed and reinserted, or `None`
/// when it would not move.
#[must_use]
pub fn drop_index(from: usize, target: usize, after: bool) -> Option<usize> {
    let slot = target + usize::from(after);
    let to = if slot > from { slot - 1 } else { slot };
    (to != from).then_some(to)
}

/// Where a dragged tab would land: between tabs, or onto one to merge into it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TabDrop {
    /// Reorder: before or after the tab at `target`.
    Insert { target: usize, after: bool },
    /// Move all of the dragged tab's blocks into the tab at `target`.
    Merge { target: usize },
}

impl TabDrop {
    pub(crate) fn target(self) -> usize {
        match self {
            Self::Insert { target, .. } | Self::Merge { target } => target,
        }
    }
}

/// The drop for a pointer at `at` along a tab spanning `start..end` on the
/// strip's axis: its middle half merges (when `merge` is offered), its ends
/// reorder before or after it.
#[must_use]
pub(crate) fn tab_drop_at(target: usize, start: f32, end: f32, at: f32, merge: bool) -> TabDrop {
    let quarter = (end - start) / 4.0;
    if merge && at > start + quarter && at < end - quarter {
        return TabDrop::Merge { target };
    }
    TabDrop::Insert { target, after: at > (start + end) / 2.0 }
}

/// Whether `drop` changes anything for the tab at `from`: not a reorder to
/// its own place, not a merge into itself.
#[must_use]
pub(crate) fn tab_drop_does_something(from: usize, drop: TabDrop) -> bool {
    match drop {
        TabDrop::Insert { target, after } => drop_index(from, target, after).is_some(),
        TabDrop::Merge { target } => target != from,
    }
}

/// A tab being dragged; also renders the pill that follows the pointer.
#[derive(Clone)]
struct TabDrag {
    window_id: String,
    title: String,
    icon: &'static str,
    /// Grab point within the tab; the preview pads it away so the pill
    /// sits just below-right of the pointer.
    offset: gpui::Point<gpui::Pixels>,
    chrome: crate::adaptive::Chrome,
}

impl Render for TabDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let pill = div()
            .flex()
            .flex_row()
            .items_center()
            .h(px(TAB_HEIGHT))
            .max_w(px(TAB_MAX))
            .pl(px(ICON_INSET))
            .pr(px(TAB_PAD))
            .glass(TAB_HEIGHT / 2.0)
            .capsule()
            .text_color(rgb(theme::foreground()))
            .font_family(theme::ui_font())
            .text_size(px(12.0))
            .child(icon(self.icon, TAB_ICON, self.chrome.accent))
            .child(div().ml(px(6.0)).min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(self.title.clone()));
        div().pl(self.offset.x + px(10.0)).pt(self.offset.y + px(10.0)).child(pill)
    }
}

impl Workspace {
    /// The tab's glyph: a globe when the window's focused block is a browser.
    pub(crate) fn tab_icon(&self, w: &silo_proto::model::WindowInfo) -> &'static str {
        self.block_icon(&w.focused)
    }

    /// Block `id`'s icon slot at `size`: the spinner while its page loads or
    /// its agent works, its favicon, else its glyph. `slot` keeps spinners apart.
    pub(crate) fn block_icon_view(&self, id: &str, slot: &str, size: f32, color: u32, cx: &gpui::App) -> AnyElement {
        use crate::browser_pane::Icon;
        if self.icon_spins(id) {
            return crate::browser_pane::spinner(&format!("{slot}-{id}"), size, color, cx);
        }
        let pane = self.block_pane(id);
        match pane.as_ref().map_or(Icon::Glyph(self.block_icon(id)), |p| p.icon(cx)) {
            Icon::Spinner => crate::browser_pane::spinner(&format!("{slot}-{id}"), size, color, cx),
            Icon::Favicon => match pane.and_then(|p| p.favicon(cx)) {
                Some(image) => gpui::img(image).flex_none().size(px(size)).into_any_element(),
                None => icon("globe", size, color).into_any_element(),
            },
            Icon::Glyph(glyph) => icon(glyph, size, color).into_any_element(),
        }
    }

    pub(crate) fn render_titlebar(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let side = self.state.tabs == TabPosition::Side;
        let hover = self.chrome.hover();
        let fullscreen = window.is_fullscreen();
        let inset = if fullscreen { FULLSCREEN_INSET } else { TRAFFIC_LIGHTS };
        if self.last_fullscreen.replace(Some(fullscreen)) != Some(fullscreen) {
            tracing::debug!(fullscreen, inset, "titlebar inset");
        }
        let compact = self.density() == crate::hosts::Density::Compact;
        let mut bar = div()
            .id("titlebar")
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .h(px(TITLEBAR_HEIGHT))
            .pl(px(inset))
            .pr(px(10.0))
            // Compact: the same hairline as the pane dividers, closing the grid.
            .when(compact, |el| el.border_b_1().border_color(rgb(theme::border())))
            // The app owns titlebar dragging: a press on empty space moves
            // the window once dragged, and a double click zooms it.
            // Controls stop the press, so it never reaches here.
            .window_control_area(gpui::WindowControlArea::Drag)
            .on_mouse_down(gpui::MouseButton::Left, cx.listener(|this, _, _, _| this.titlebar_move = true))
            .on_mouse_up(gpui::MouseButton::Left, cx.listener(|this, _, _, _| this.titlebar_move = false))
            .on_mouse_down_out(cx.listener(|this, _, _, _| this.titlebar_move = false))
            .on_mouse_move(cx.listener(|this, _, window, _| {
                if std::mem::take(&mut this.titlebar_move) {
                    window.start_window_move();
                }
            }))
            .on_click(|event, window, _| {
                if event.click_count() == 2 {
                    window.titlebar_double_click();
                }
            })
            .font_family(theme::ui_font())
            .when(side && self.view.is_some(), |el| el.child(self.render_sidebar_toggle(cx)))
            .child(self.render_pill(cx));
        if self.view.is_some() {
            let menu_open = self.appearance_menu.open;
            let new_tab_open = self.launcher.is_open_for_tab();
            // Side tabs: the strip's place is plain titlebar, dragging the window.
            let zone = if side { div().flex_1().h_full().into_any_element() } else { self.render_tab_strip(window, cx) };
            bar = bar
                .child(zone)
                .children(self.render_watchers())
                .children(self.render_update_pill(cx))
                .children(self.render_background_button(cx))
                .child(
                    div()
                        .id("appearance")
                        .flex()
                        .items_center()
                        .justify_center()
                        .flex_none()
                        .ml(px(6.0))
                        .size(px(PILL_HEIGHT))
                        .capsule()
                        .hover(|s| s.bg(hover))
                        .cursor_pointer()
                        .when(menu_open, |el| el.bg(hover))
                        .when(!menu_open, |el| el.tooltip(crate::tooltip::Tooltip::text("Settings")))
                        .child(icon("settings", 15.0, self.chrome.icon))
                        .map(claim_press)
                        .on_click(cx.listener(|this, _, window, cx| this.toggle_appearance_menu(window, cx))),
                )
                .child(
                    div()
                        .id("new-tab")
                        .flex()
                        .items_center()
                        .justify_center()
                        .flex_none()
                        .ml(px(2.0))
                        .size(px(PILL_HEIGHT))
                        .capsule()
                        .hover(|s| s.bg(hover))
                        .cursor_pointer()
                        .when(new_tab_open, |el| el.bg(hover))
                        .when(!new_tab_open, |el| el.tooltip(crate::tooltip::Tooltip::text("New Tab")))
                        .child(icon("plus", 15.0, self.chrome.icon))
                        .map(claim_press)
                        .on_click(cx.listener(|this, _, window, cx| this.toggle_launcher(crate::commands::Command::NewTabChooser, window, cx))),
                );
        }
        bar.into_any_element()
    }

    /// The titlebar's tab strip, with the empty space after it (both take
    /// tab drops).
    fn render_tab_strip(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let Some(view) = &self.view else { return div().flex_1().into_any_element() };
        let several = view.session.windows.len() > 1;
        let now = Instant::now();
        let active_index = view.session.windows.iter().position(|w| w.id == view.session.active_window);
        let active_id = view.session.active_window.clone();
        self.note_active_tab(TabPosition::Top, &active_id);
        // Tabs size to their titles, ellipsized beyond `TAB_MAX`, and
        // scroll when they overflow. Vertical wheel motion scrolls
        // horizontally too (GPUI maps it when only x scrolls).
        let mut tabs = div()
            .id("tab-strip")
            .flex()
            .flex_row()
            .items_center()
            // No row gap: each tab after the first leads with its own
            // (a closing tab's shrinks with it). A collapsing tab's
            // negative margin closing a gap left gpui's scroll row
            // measured narrower than its tabs: it clipped them all.
            .pl(px(PILL_GAP))
            // Room for the insertion bar after the last tab.
            .pr(px(TAB_GAP))
            .h_full()
            .min_w_0()
            .flex_shrink(1.0)
            .overflow_x_scroll()
            .track_scroll(&self.tab_scroll)
            // Scrolled by hand: stop pulling the active tab back into view.
            .on_scroll_wheel(cx.listener(|this, _, _, _| this.tab_follow.set(false)));
        let mut ghosts: Vec<&TabGhost> = self.motion.tab_ghosts.iter().collect();
        ghosts.sort_by_key(|g| g.index);
        let mut ghosts = ghosts.into_iter().peekable();
        // Child index of the active tab among tabs and ghosts.
        let mut child = 0;
        let mut active_child = None;
        let mut active_entering = false;
        let (fade_left, fade_right) =
            edge_fades(f32::from(self.tab_scroll.offset().x), f32::from(self.tab_scroll.max_offset().x), self.tab_room.get());
        let strip = self.tab_scroll.bounds();
        // Tabs fade out where the strip scrolls on, by their own
        // opacity (no overlay to stack on a frosted titlebar).
        let fade = |child: usize| {
            self.tab_scroll.bounds_for_item(child).map_or(1.0, |tab| {
                let ramp = |inset: gpui::Pixels| 0.3 + 0.7 * (f32::from(inset) / FADE_WIDTH).clamp(0.0, 1.0);
                let left = if fade_left { ramp(tab.left() - strip.left()) } else { 1.0 };
                let right = if fade_right { ramp(strip.right() - tab.right()) } else { 1.0 };
                left.min(right)
            })
        };
        // The gap before the next tab: none at the start, a full one after
        // a tab, and shrinking with a closing first tab (so it ends at none).
        let mut gap_before = 0.0;
        for (index, w) in view.session.windows.iter().enumerate() {
            while let Some(ghost) = ghosts.next_if(|g| g.index <= index) {
                let keep = 1.0 - motion::progress(ghost.started, motion::TAB_EXIT, now).unwrap_or(1.0);
                if let Some(el) = render_tab_ghost(ghost, child > 0, self.chrome, now) {
                    tabs = tabs.child(el);
                    if child == 0 {
                        gap_before = keep;
                    }
                    child += 1;
                }
            }
            let selected = several && active_index == Some(index);
            let appear = self.motion.tab_enter.get(&w.id).and_then(|s| motion::progress(*s, motion::TAB_ENTER, now)).unwrap_or(1.0);

            if w.id == active_id {
                active_child = Some(child);
                active_entering = appear < 1.0;
            }
            // Its gap grows in with it.
            let lead = TAB_GAP * appear * gap_before;
            gap_before = 1.0;
            tabs = tabs.child(self.render_tab(w, index, selected, several, appear, fade(child), lead, now, cx));
            child += 1;
        }
        for ghost in ghosts {
            if let Some(ghost) = render_tab_ghost(ghost, child > 0, self.chrome, now) {
                tabs = tabs.child(ghost);
                child += 1;
            }
        }
        self.step_tab_reveal(&self.tab_scroll, false, &active_id, active_child, active_entering, !cx.reduce_motion(), window);
        let strip_view = div().relative().flex().flex_row().h_full().min_w_0().flex_shrink(1.0).child(tabs);
        // The strip and the empty space after it take tab drops. The
        // space after the last tab means "move to the end".
        let last = view.session.windows.len().saturating_sub(1);
        let room = self.tab_room.clone();
        let spacer = div()
            .relative()
            .flex_1()
            .h_full()
            .child(canvas(move |b, _, _| room.set(f32::from(b.size.width)), |_, (), _, _| {}).absolute().inset_0())
            .on_drag_move(cx.listener(move |this, ev: &DragMoveEvent<TabDrag>, _, cx| {
                if ev.bounds.contains(&ev.event.position) {
                    let id = ev.drag(cx).window_id.clone();
                    this.set_tab_drop(&id, Some(TabDrop::Insert { target: last, after: true }), cx);
                }
            }));
        let drop_zone = div()
            .flex()
            .flex_row()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(strip_view)
            .child(spacer)
            .on_drag_move(cx.listener(|this, ev: &DragMoveEvent<TabDrag>, _, cx| {
                if !ev.bounds.contains(&ev.event.position) {
                    this.set_tab_drop("", None, cx);
                }
            }))
            .on_drop(cx.listener(|this, drag: &TabDrag, _, cx| {
                if let Some(drop) = this.tab_drop.take() {
                    this.drop_tab(&drag.window_id, drop, cx);
                }
                cx.notify();
            }));
        drop_zone.into_any_element()
    }

    /// Scroll the active tab into view once it changes, or the tabs move
    /// between the titlebar and the sidebar; while following
    /// ([`Workspace::tab_follow`]), check it again on every frame.
    fn note_active_tab(&self, position: TabPosition, active_id: &str) {
        let key = Some((position, active_id.to_owned()));
        if *self.tab_last_active.borrow() != key {
            *self.tab_last_active.borrow_mut() = key;
            self.tab_follow.set(true);
            *self.tab_reveal.borrow_mut() = Some((active_id.to_owned(), REVEAL_FRAMES));
        } else if self.tab_follow.get() && self.tab_reveal.borrow().is_none() {
            *self.tab_reveal.borrow_mut() = Some((active_id.to_owned(), 1));
        }
    }

    /// Advance a pending scroll-into-view of the active tab by one frame,
    /// using the previous frame's layout. It stays pending (and requests
    /// frames) while the tab is still growing in or not laid out yet.
    #[allow(clippy::too_many_arguments)]
    fn step_tab_reveal(
        &self,
        scroll: &gpui::ScrollHandle,
        vertical: bool,
        active_id: &str,
        child: Option<usize>,
        entering: bool,
        smooth: bool,
        window: &mut Window,
    ) {
        let mut pending = self.tab_reveal.borrow_mut();
        let Some((id, frames)) = pending.take() else {
            return;
        };
        if id != active_id || frames == 0 {
            return;
        }
        let Some(child) = child else { return };
        let viewport = scroll.bounds();
        // Along the strip's axis: x on top, y in the sidebar.
        let along = |p: gpui::Point<gpui::Pixels>| f32::from(if vertical { p.y } else { p.x });
        let extent = |s: gpui::Size<gpui::Pixels>| f32::from(if vertical { s.height } else { s.width });
        let keep = match scroll.bounds_for_item(child) {
            Some(item) if extent(viewport.size) > 0.0 => {
                let offset = scroll.offset();
                let from = along(offset);
                let target = reveal_offset(
                    from,
                    extent(viewport.size),
                    along(item.origin) - along(viewport.origin),
                    extent(item.size),
                    REVEAL_MARGIN,
                    along(scroll.max_offset()),
                );
                let next = if smooth { ease_toward(from, target) } else { target };
                if (next - from).abs() > 0.01 {
                    scroll.set_offset(if vertical { gpui::point(offset.x, px(next)) } else { gpui::point(px(next), offset.y) });
                }
                // The layout moved since the last check (a tab growing or
                // shrinking): one more frame checks where it settles.
                let seen = [along(item.origin) - along(viewport.origin) - from, extent(item.size), extent(viewport.size), along(scroll.max_offset())];
                if self.tab_seen.replace(Some(seen)) != Some(seen) {
                    window.request_animation_frame();
                }
                entering || (target - next).abs() > 0.01
            }
            // Not laid out yet (a brand-new tab): try next frame.
            _ => true,
        };
        if keep {
            *pending = Some((id, frames - 1));
            window.request_animation_frame();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_tab(
        &self,
        w: &silo_proto::model::WindowInfo,
        index: usize,
        selected: bool,
        several: bool,
        appear: f32,
        fade: f32,
        lead: f32,
        now: Instant,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let primary = selected || !several;
        let close = self.motion.close_button(&w.id, now);
        let title = self.window_title(w, cx);
        let glyph = self.tab_icon(w);
        let chrome = self.chrome;
        // The colour of the app running in another tab, where it shows.
        let swatch = (!selected)
            .then(|| self.backdrop(&w.focused, cx).or_else(|| crate::floats::window_blocks(w).iter().find_map(|b| self.backdrop(b, cx))))
            .flatten()
            .filter(|&c| chrome.shows(c));
        let drag = TabDrag { window_id: w.id.clone(), title: title.clone(), icon: glyph, offset: gpui::Point::default(), chrome };
        // Insertion bar on the side a dragged tab would drop, centred in
        // the gap to the neighbour.
        let drop = self.tab_drop.filter(|d| d.target() == index && self.tab_dragging.is_some());
        let insert = match drop {
            Some(TabDrop::Insert { after, .. }) => Some(after),
            _ => None,
        };
        // Dropped here, the dragged tab's blocks join this one: outlined.
        let merging = matches!(drop, Some(TabDrop::Merge { .. }));
        let workspace = cx.entity().downgrade();
        let window_id = w.id.clone();
        let group = format!("tab-{}", w.id);
        let hover = chrome.hover();
        let hover_id = w.id.clone();
        // Every tab is a full pill on the window background: inactive ones
        // only show a fill on hover, the current one a subtle step brighter.
        let fill = chrome.selected();
        let label = div()
            .flex()
            .flex_row()
            .items_center()
            .h(px(TAB_HEIGHT))
            .min_w_0()
            // The icon sits `ICON_INSET` from the left, top and bottom alike.
            .pl(px(ICON_INSET))
            .pr(px(TAB_PAD))
            .capsule()
            .when(several && selected, |el| el.bg(fill))
            .when(!selected, |el| el.group_hover(group.clone(), |s| s.bg(hover)))
            .when(merging, |el| el.bg(hover).border_1().border_color(rgb(chrome.accent)))
            // Hovered, the close button takes the icon's place, centred on it
            // (`CLOSE_INSET` from the pill's left, top and bottom): the tab
            // keeps its width, so hovering never shifts the strip.
            .child(
                div()
                    .relative()
                    .flex_none()
                    .size(px(TAB_ICON))
                    .child(div().size_full().opacity(1.0 - close).child(self.block_icon_view(
                        &w.focused,
                        "tab",
                        TAB_ICON,
                        if primary { chrome.accent } else { chrome.icon },
                        cx,
                    )))
                    .when(close > 0.0, |el| {
                        el.child(
                            div()
                                .id(gpui::ElementId::Name(format!("close-{}", w.id).into()))
                                .absolute()
                                .top(px(-CLOSE_OVERHANG))
                                .left(px(-CLOSE_OVERHANG))
                                .flex()
                                .items_center()
                                .justify_center()
                                .size(px(CLOSE_SIZE))
                                .capsule()
                                .opacity(close)
                                .hover(|s| s.bg(hover))
                                .child(icon("close", 12.0, chrome.icon))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.close(crate::close_confirm::CloseTarget::Tab(window_id.clone()), window, cx);
                                })),
                        )
                    }),
            )
            .child(div().ml(px(6.0)).min_w_0().flex_shrink(1.0).overflow_hidden().whitespace_nowrap().text_ellipsis().child(title))
            .when_some(self.window_mark(w), |el, mark| {
                let glyph = crate::block_status::glyph(mark, &format!("tab-{}", w.id), 10.0, chrome.surface, chrome.accent, cx);
                el.child(div().flex_none().ml(px(6.0)).flex().items_center().child(glyph))
            })
            .when_some(swatch, |el, color| el.child(div().flex_none().ml(px(6.0)).size(px(7.0)).capsule().bg(rgb(color))));
        let entering = appear < 1.0;
        div()
            .id(gpui::ElementId::Name(group.clone().into()))
            .group(group)
            .relative()
            .flex()
            .flex_col()
            // Full size in the scrolling strip; overflow scrolls instead.
            .flex_none()
            .ml(px(lead))
            // Content-sized: no minimum width, so nothing pads the end.
            .max_w(px(TAB_MAX * appear))
            .when(entering, |el| el.overflow_hidden())
            // The source stays in place, faded, under the drag preview.
            .opacity(fade * appear * if self.tab_dragging.as_ref() == Some(&w.id) { 0.4 } else { 1.0 })
            .text_size(px(12.0))
            .text_color(rgb(if primary { chrome.ink } else { chrome.secondary }))
            .cursor_pointer()
            .child(label)
            .when_some(insert, |el, after| {
                el.child(
                    div()
                        .absolute()
                        .top(px(5.0))
                        .bottom(px(5.0))
                        .w(px(2.0))
                        .map(|el| if after { el.right(px(-(TAB_GAP + 2.0) / 2.0)) } else { el.left(px(-(TAB_GAP + 2.0) / 2.0)) })
                        .continuous(1.0)
                        .bg(rgb(chrome.accent)),
                )
            })
            .map(claim_press)
            // The preview lives exactly as long as the drag, so its release
            // ends the drag state however the drag ended.
            .on_drag(drag, move |drag, offset, _, cx| {
                let id = drag.window_id.clone();
                _ = workspace.update(cx, |this, cx| {
                    this.tab_dragging = Some(id);
                    this.tab_drop = None;
                    cx.notify();
                });
                let workspace = workspace.clone();
                cx.new(|cx| {
                    cx.on_release(move |_, cx| {
                        _ = workspace.update(cx, |this, cx| {
                            this.tab_dragging = None;
                            this.tab_drop = None;
                            cx.notify();
                        });
                    })
                    .detach();
                    TabDrag { offset, ..drag.clone() }
                })
            })
            .on_drag_move(cx.listener(move |this, ev: &DragMoveEvent<TabDrag>, _, cx| {
                if ev.bounds.contains(&ev.event.position) {
                    let (b, at) = (ev.bounds, ev.event.position.x);
                    let drop = tab_drop_at(index, f32::from(b.left()), f32::from(b.right()), f32::from(at), this.merges_tabs());
                    let id = ev.drag(cx).window_id.clone();
                    this.set_tab_drop(&id, Some(drop), cx);
                }
            }))
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                this.motion.set_hover(&hover_id, *hovered, Instant::now(), !cx.reduce_motion());
                cx.notify();
            }))
            .on_click(cx.listener(move |this, _, _, _| this.select_window(index)))
            .into_any_element()
    }

    /// Record where a drag of `window_id` would drop; a no-op spot (its own
    /// place, or onto itself) clears it.
    pub(crate) fn set_tab_drop(&mut self, window_id: &str, drop: Option<TabDrop>, cx: &mut Context<Self>) {
        let from = self.view.as_ref().and_then(|v| v.session.windows.iter().position(|w| w.id == window_id));
        let drop = drop.filter(|&drop| from.is_some_and(|from| tab_drop_does_something(from, drop)));
        if self.tab_drop != drop {
            self.tab_drop = drop;
            cx.notify();
        }
    }

    /// Whether this host can merge tabs (`window.merge`).
    pub(crate) fn merges_tabs(&self) -> bool {
        self.host_supports(silo_proto::compat::feature::WINDOW_MERGE)
    }

    pub(crate) fn drop_tab(&mut self, window_id: &str, drop: TabDrop, cx: &mut Context<Self>) {
        match drop {
            TabDrop::Insert { target, after } => {
                crate::sound::play(crate::sound::Sound::Drop);
                self.move_tab(window_id, target, after, cx);
            }
            TabDrop::Merge { target } => {
                crate::sound::play(crate::sound::Sound::Merge);
                self.merge_tab(window_id, target, cx);
            }
        }
    }

    /// Move every block of tab `window_id` into the tab at `target`, beside
    /// its layout along the target's longer side. The server applies it (its
    /// `session.changed` follows at once); a failure is flashed.
    fn merge_tab(&mut self, window_id: &str, target: usize, cx: &mut Context<Self>) {
        let Some(into) = self.view.as_ref().and_then(|v| v.session.windows.get(target)).map(|w| w.id.clone()) else {
            return;
        };
        if into == window_id {
            return;
        }
        let axis = self.merge_axis();
        let method = Method::WindowMerge { window_id: into, source: window_id.to_owned(), axis };
        self.call_optimistic(method, "Couldn't merge tabs", |_| {}, cx);
    }

    /// Side by side when the pane area is wider than tall, else stacked.
    fn merge_axis(&self) -> silo_proto::model::Axis {
        let area = self.motion.measured.area.get();
        if area.is_some_and(|b| b.size.height > b.size.width) { silo_proto::model::Axis::Vertical } else { silo_proto::model::Axis::Horizontal }
    }

    /// Move a tab locally for an instant drop, then on the server, whose
    /// `session.changed` confirms it; a failed call puts it back.
    fn move_tab(&mut self, window_id: &str, target: usize, after: bool, cx: &mut Context<Self>) {
        let Some(view) = self.view.as_mut() else {
            return;
        };
        let windows = &mut view.session.windows;
        let Some(from) = windows.iter().position(|w| w.id == window_id) else {
            return;
        };
        let Some(to) = drop_index(from, target, after).filter(|&to| to < windows.len()) else {
            return;
        };
        let window = windows.remove(from);
        windows.insert(to, window);
        let id = window_id.to_owned();
        let revert = move |this: &mut Self| {
            let Some(windows) = this.view.as_mut().map(|v| &mut v.session.windows) else {
                return;
            };
            if let Some(at) = windows.iter().position(|w| w.id == id) {
                let window = windows.remove(at);
                windows.insert(from.min(windows.len()), window);
            }
        };
        let method = Method::WindowMove { window_id: window_id.to_owned(), index: to };
        self.call_optimistic(method, "Couldn't move tab", revert, cx);
    }

    fn render_pill(&self, cx: &mut Context<Self>) -> AnyElement {
        let host = &self.hosts[self.active_host];
        let name = self.view.as_ref().map_or_else(|| "—".to_owned(), |v| v.session.name.clone());
        let open = self.switcher.is_some();
        let chrome = self.chrome;
        // One line at tab scale: session name, then the host, muted.
        div()
            .id("session-pill")
            .flex()
            .flex_row()
            .flex_none()
            .items_center()
            .gap(px(6.0))
            .h(px(TAB_HEIGHT))
            .pl(px(9.0))
            .pr(px(10.0))
            .max_w(px(240.0))
            .capsule()
            .whitespace_nowrap()
            .text_size(px(12.0))
            // Plain until the switcher is open; a faint wash on hover.
            .when(open, |el| el.bg(chrome.tint(0.11)))
            .when(!open, |el| el.hover(|s| s.bg(chrome.hover())))
            .cursor_pointer()
            .child(icon(if host.info.local { "laptop" } else { "globe" }, 13.0, chrome.ink))
            .child(
                div()
                    .min_w_0()
                    .flex_shrink(1.0)
                    .overflow_hidden()
                    .text_ellipsis()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(rgb(chrome.ink))
                    .child(name),
            )
            .child(
                div()
                    .flex_none()
                    .max_w(px(90.0))
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_size(px(11.0))
                    .text_color(rgb(chrome.muted))
                    .child(if host.info.transport.is_shared() { format!("{} · shared", host.info.name) } else { host.info.name.clone() }),
            )
            .on_click(cx.listener(|this, _, window, cx| this.toggle_switcher(window, cx)))
            .map(claim_press)
            // Right-click renames the current session.
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(|this, _, window, cx| {
                    cx.stop_propagation();
                    this.open_switcher_rename(window, cx);
                }),
            )
            .into_any_element()
    }

    /// Who else watches this session (other devices, tokens and links,
    /// never the owner's own apps): an eye and their names, all of them in
    /// the tooltip.
    fn render_watchers(&self) -> Option<AnyElement> {
        let view = self.view.as_ref()?;
        // Offline, the last list is stale.
        let me = self.control()?.client_id();
        let labels = watcher_labels(&view.watchers, Some(me));
        let text = match labels.as_slice() {
            [] => return None,
            [one] => one.clone(),
            [first, rest @ ..] => format!("{first} +{}", rest.len()),
        };
        let chrome = self.chrome;
        Some(
            div()
                .id("watchers")
                .flex()
                .flex_none()
                .items_center()
                .gap(px(5.0))
                .ml(px(6.0))
                .h(px(TAB_HEIGHT - 2.0))
                .max_w(px(200.0))
                .px(px(9.0))
                .capsule()
                .whitespace_nowrap()
                .text_size(px(11.5))
                .text_color(rgb(chrome.secondary))
                .child(icon("eye", 13.0, chrome.icon))
                .child(div().min_w_0().overflow_hidden().text_ellipsis().child(text))
                .tooltip(crate::tooltip::Tooltip::below(format!("Watching: {}", labels.join(", ")), TITLEBAR_HEIGHT))
                .map(claim_press)
                .into_any_element(),
        )
    }

    /// The Update pill: outlined with a spinner while a release downloads
    /// (and a progress ring with the percent once its size is known), is
    /// verified or installs, accent-filled once one is staged (a click
    /// installs it and restarts), outlined when this copy of Silo cannot be
    /// replaced.
    fn render_update_pill(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        use crate::updater::PillKind;
        let (label, tooltip, kind) = crate::updater::pill(self.updater.read(cx).state())?;
        let (chrome, ready) = (self.chrome, kind == PillKind::Ready);
        let busy = matches!(kind, PillKind::Busy | PillKind::Progress(_));
        let pill = div()
            .id("update")
            .flex()
            .flex_none()
            .items_center()
            .gap(px(UPDATE_GAP))
            .ml(px(6.0))
            .h(px(UPDATE_HEIGHT))
            // With its spinner, the spinner sits as a tab's icon does: as
            // far from the left as from the top and bottom (the border
            // counted), so it is concentric with the rounded end.
            .pl(px(if busy { UPDATE_ICON_PAD } else { UPDATE_PAD }))
            .pr(px(UPDATE_PAD))
            .capsule()
            .whitespace_nowrap()
            .text_size(px(12.0))
            .font_weight(FontWeight::MEDIUM)
            .map(|el| {
                if ready {
                    el.bg(rgb(theme::update_blue())).text_color(rgb(0x00ff_ffff)).cursor_pointer().hover(|s| s.opacity(0.88))
                } else {
                    el.border_1().border_color(chrome.tint(0.18)).text_color(rgb(chrome.muted))
                }
            })
            .when(busy, |el| el.child(crate::browser_pane::spinner("update", TAB_ICON, chrome.muted, cx)))
            .child(label)
            .when_some(if let PillKind::Progress(percent) = kind { Some(percent) } else { None }, |el, percent| {
                el.child(crate::agent::ring::ring(f32::from(percent) / 100.0, chrome.accent))
            })
            // The percent in a slot sized by a hidden `100%`, in tabular
            // figures: the pill keeps its width as the number grows.
            .when_some(if let PillKind::Progress(percent) = kind { Some(percent) } else { None }, |el, percent| {
                let figures = gpui::FontFeatures(std::sync::Arc::new(vec![("tnum".into(), 1)]));
                el.child(
                    div()
                        .relative()
                        .flex_none()
                        .font_features(figures)
                        .child(div().invisible().child("100%"))
                        .child(div().absolute().top_0().right_0().child(format!("{percent}%"))),
                )
            })
            .tooltip(crate::tooltip::Tooltip::below(tooltip, TITLEBAR_HEIGHT))
            .map(claim_press);
        Some(if ready {
            pill.on_click(cx.listener(|this, _, _, cx| this.updater.update(cx, |updater, cx| updater.restart(cx)))).into_any_element()
        } else {
            pill.into_any_element()
        })
    }
}

/// Names of the session's watchers other than the owner's clients and
/// `me`, once each: a link's or token's marked with its rights.
#[must_use]
pub fn watcher_labels(watchers: &[silo_proto::model::Watcher], me: Option<u64>) -> Vec<String> {
    use silo_proto::{access::Rights, model::WatcherKind};
    let mut labels: Vec<String> = Vec::new();
    for w in watchers.iter().filter(|w| w.principal != WatcherKind::Owner && Some(w.client_id) != me) {
        let label = match (w.principal, w.rights) {
            (WatcherKind::Link | WatcherKind::Token, Rights::View) => format!("{} (view)", w.label),
            _ => w.label.clone(),
        };
        if !labels.contains(&label) {
            labels.push(label);
        }
    }
    labels
}

/// A closed tab collapsing in width and fading at its old position. Visual
/// only: no listeners, and the server close was already sent.
fn render_tab_ghost(ghost: &TabGhost, lead: bool, chrome: crate::adaptive::Chrome, now: Instant) -> Option<AnyElement> {
    let gone = motion::progress(ghost.started, motion::TAB_EXIT, now)?;
    let keep = 1.0 - gone;
    Some(
        div()
            .id(gpui::ElementId::Name(format!("tab-ghost-{}", ghost.id).into()))
            .flex()
            .flex_col()
            .flex_none()
            // An exact width as it collapses: a max width alone left the
            // scrolling strip mismeasured (its tabs clipped and faded).
            .map(|el| match ghost.width {
                Some(w) => el.w(px(w * keep)),
                None => el.max_w(px(TAB_MAX * keep)),
            })
            // Its gap collapses with it, so neighbours close up smoothly.
            .when(lead, |el| el.ml(px(TAB_GAP * keep)))
            .overflow_hidden()
            .opacity(keep)
            .text_size(px(12.0))
            .text_color(rgb(chrome.secondary))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .h(px(TAB_HEIGHT))
                    .min_w_0()
                    .px(px(10.0))
                    .capsule()
                    .child(icon(ghost.icon, 12.0, chrome.icon))
                    .child(div().ml(px(6.0)).min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(ghost.title.clone())),
            )
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tabs_middle_merges_and_its_ends_reorder() {
        // A tab from 100 to 200: quarters at 125 and 175.
        assert_eq!(tab_drop_at(3, 100.0, 200.0, 110.0, true), TabDrop::Insert { target: 3, after: false });
        assert_eq!(tab_drop_at(3, 100.0, 200.0, 130.0, true), TabDrop::Merge { target: 3 });
        assert_eq!(tab_drop_at(3, 100.0, 200.0, 170.0, true), TabDrop::Merge { target: 3 });
        assert_eq!(tab_drop_at(3, 100.0, 200.0, 190.0, true), TabDrop::Insert { target: 3, after: true });
        // A host without window.merge: the whole tab reorders, by halves.
        assert_eq!(tab_drop_at(3, 100.0, 200.0, 140.0, false), TabDrop::Insert { target: 3, after: false });
        assert_eq!(tab_drop_at(3, 100.0, 200.0, 160.0, false), TabDrop::Insert { target: 3, after: true });
        // Onto itself, or back to its own place: nothing to do.
        assert!(!tab_drop_does_something(3, TabDrop::Merge { target: 3 }));
        assert!(tab_drop_does_something(3, TabDrop::Merge { target: 1 }));
        assert!(!tab_drop_does_something(3, TabDrop::Insert { target: 3, after: true }));
        assert!(!tab_drop_does_something(3, TabDrop::Insert { target: 4, after: false }));
        assert!(tab_drop_does_something(3, TabDrop::Insert { target: 1, after: false }));
    }

    #[test]
    fn the_close_button_and_the_icon_are_as_far_from_the_pills_left_as_from_its_top_and_bottom() {
        let top = (TAB_HEIGHT - CLOSE_SIZE) / 2.0;
        assert!((top - CLOSE_INSET).abs() < f32::EPSILON, "top and bottom {top}, left {CLOSE_INSET}");
        const { assert!(CLOSE_SIZE >= 16.0, "still a comfortable target") };
        let icon_top = (TAB_HEIGHT - TAB_ICON) / 2.0;
        assert!((icon_top - ICON_INSET).abs() < f32::EPSILON, "icon: top and bottom {icon_top}, left {ICON_INSET}");
        // The close button replaces the icon in place: the same centre.
        assert!((ICON_INSET + TAB_ICON / 2.0 - (CLOSE_INSET + CLOSE_SIZE / 2.0)).abs() < f32::EPSILON);
        assert!((ICON_INSET - CLOSE_OVERHANG - CLOSE_INSET).abs() < f32::EPSILON);
        // The update pill's spinner: as far from its left edge as from its top and bottom.
        let spinner_top = (UPDATE_HEIGHT - TAB_ICON) / 2.0;
        assert!((UPDATE_BORDER + UPDATE_ICON_PAD - spinner_top).abs() < f32::EPSILON, "spinner: left {UPDATE_ICON_PAD} + border, top {spinner_top}");
    }

    /// The tab strip's layout (as `render_titlebar` builds it) while a closing tab collapses: the strip
    /// always holds its tabs. A negative margin closing the gap made gpui measure it at its padding.
    mod strip_layout {
        use std::{cell::RefCell, rc::Rc};

        use gpui::{Bounds, Context, IntoElement, Pixels, Render, TestAppContext, Window, canvas, div, prelude::*, px, size};

        use super::super::{PILL_GAP, TAB_GAP, TAB_HEIGHT, TAB_MAX};

        type Seen = Rc<RefCell<Vec<(&'static str, Bounds<Pixels>)>>>;

        struct Bar {
            keep: f32,
            seen: Seen,
        }

        fn probe(name: &'static str, seen: &Seen) -> impl IntoElement {
            let seen = seen.clone();
            canvas(move |b, _, _| seen.borrow_mut().push((name, b)), |_, _, _, _| {}).absolute().top_0().left_0().size_full()
        }

        impl Render for Bar {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let label = || div().w(px(46.0)).h(px(TAB_HEIGHT));
                let tab = div().id("tab").relative().flex().flex_col().flex_none().max_w(px(TAB_MAX)).child(label()).child(probe("tab", &self.seen));
                let ghost = div()
                    .id("ghost")
                    .relative()
                    .flex()
                    .flex_col()
                    .flex_none()
                    .w(px(46.0 * self.keep))
                    .ml(px(TAB_GAP * self.keep))
                    .overflow_hidden()
                    .child(label());
                let strip = div()
                    .id("tab-strip")
                    .relative()
                    .flex()
                    .flex_row()
                    .items_center()
                    .pl(px(PILL_GAP))
                    .pr(px(TAB_GAP))
                    .h_full()
                    .min_w_0()
                    .flex_shrink(1.0)
                    .overflow_x_scroll()
                    .child(tab)
                    .child(ghost)
                    .child(probe("strip", &self.seen));
                let view = div().relative().flex().flex_row().h_full().min_w_0().flex_shrink(1.0).child(strip);
                let zone = div().flex().flex_row().flex_1().min_w_0().h_full().child(view).child(div().flex_1().h_full());
                div().size_full().flex().flex_row().child(div().w(px(240.0)).flex_none()).child(zone)
            }
        }

        #[gpui::test]
        fn a_closing_tab_never_clips_the_strip(cx: &mut TestAppContext) {
            for keep in [1.0, 0.75, 0.5, 0.25, 0.0] {
                let seen = Seen::default();
                let (_, cx) = cx.add_window_view({
                    let seen = seen.clone();
                    move |_, _| Bar { keep, seen }
                });
                cx.simulate_resize(size(px(1000.0), px(40.0)));
                cx.run_until_parked();
                let seen = seen.borrow();
                let last = |name| seen.iter().rev().find(|(n, _)| *n == name).map(|(_, b)| f32::from(b.size.width)).unwrap();
                let want = PILL_GAP + 46.0 + TAB_GAP * keep + 46.0 * keep + TAB_GAP;
                assert!((last("strip") - want).abs() < 0.6, "keep {keep}: strip {} for {want}", last("strip"));
                assert!((last("tab") - 46.0).abs() < 0.01);
            }
        }
    }

    #[test]
    fn reveal_moves_minimally_and_clamps() {
        // Viewport 300 wide, content scrollable by 500.
        // Already fully visible (with margin): no movement.
        assert!((reveal_offset(-100.0, 300.0, 200.0, 100.0, 28.0, 500.0) - -100.0).abs() < f32::EPSILON);
        // Hidden to the right: scroll just enough, keeping the margin.
        assert!((reveal_offset(0.0, 300.0, 400.0, 100.0, 28.0, 500.0) - -228.0).abs() < f32::EPSILON);
        // Hidden to the left: align its start after the margin.
        assert!((reveal_offset(-300.0, 300.0, 100.0, 100.0, 28.0, 500.0) - -72.0).abs() < f32::EPSILON);
        // The first tab goes all the way back to the start, the last to the end.
        assert!((reveal_offset(-300.0, 300.0, 4.0, 100.0, 28.0, 500.0) - 0.0).abs() < f32::EPSILON);
        assert!((reveal_offset(0.0, 300.0, 700.0, 100.0, 28.0, 500.0) - -500.0).abs() < f32::EPSILON);
        // Nothing to scroll: always 0.
        assert!((reveal_offset(0.0, 300.0, 250.0, 100.0, 28.0, 0.0) - 0.0).abs() < f32::EPSILON);
        // Wider than the viewport: its start is shown.
        assert!((reveal_offset(0.0, 100.0, 200.0, 190.0, 28.0, 500.0) - -200.0).abs() < f32::EPSILON);
    }

    #[test]
    fn watchers_leave_out_the_owner_and_this_app() {
        use silo_proto::{
            access::Rights,
            model::{Watcher, WatcherKind},
        };
        let w = |client_id, label: &str, principal, rights| Watcher { client_id, label: label.into(), principal, rights, blocks: Vec::new() };
        let watchers = [
            w(1, "Silo", WatcherKind::Owner, Rights::Owner),
            w(2, "Silo · laptop", WatcherKind::Device, Rights::Control),
            w(3, "Silo · Ana", WatcherKind::Link, Rights::View),
            w(4, "Silo · Ana", WatcherKind::Link, Rights::View),
            w(5, "Silo · Ben", WatcherKind::Link, Rights::Interact),
        ];
        assert_eq!(watcher_labels(&watchers, Some(2)), ["Silo · Ana (view)", "Silo · Ben"]);
        assert_eq!(watcher_labels(&watchers, None)[0], "Silo · laptop");
        assert!(watcher_labels(&watchers[..1], None).is_empty());
    }

    #[test]
    fn fades_only_where_tabs_are_hidden() {
        assert_eq!(edge_fades(0.0, 0.0, 0.0), (false, false), "everything fits");
        assert_eq!(edge_fades(0.0, 200.0, 0.0), (false, true), "at the start");
        assert_eq!(edge_fades(-80.0, 200.0, 0.0), (true, true), "in the middle");
        assert_eq!(edge_fades(-200.0, 200.0, 0.0), (true, false), "at the end");
        assert_eq!(edge_fades(-199.8, 200.0, 0.0), (true, false), "sub-pixel rounding at the end");
        // Room left after the strip: a tab growing in or collapsing out reports a scroll range; nothing fades.
        assert_eq!(edge_fades(0.0, 64.0, 300.0), (false, false), "not overflowing");
    }

    #[test]
    fn reveal_eases_and_settles() {
        let mut offset = 0.0;
        let mut frames = 0;
        while (offset - -228.0_f32).abs() > f32::EPSILON {
            let next = ease_toward(offset, -228.0);
            assert!(next <= offset && next >= -228.0, "monotonic, no overshoot");
            offset = next;
            frames += 1;
            assert!(frames < 40, "settles quickly");
        }
    }

    #[test]
    fn drop_index_accounts_for_the_removed_source() {
        // Tabs 0..4, dragging tab 1.
        assert_eq!(drop_index(1, 3, false), Some(2), "before 3");
        assert_eq!(drop_index(1, 3, true), Some(3), "after 3, the end");
        assert_eq!(drop_index(1, 0, false), Some(0));
        assert_eq!(drop_index(1, 0, true), None, "after 0 is where it is");
        assert_eq!(drop_index(1, 2, false), None, "before its neighbour");
        assert_eq!(drop_index(1, 1, false), None, "onto itself");
        assert_eq!(drop_index(1, 1, true), None);
        // Dragging the last tab to the front, and the first to the end.
        assert_eq!(drop_index(3, 0, false), Some(0));
        assert_eq!(drop_index(0, 3, true), Some(3));
    }
}
