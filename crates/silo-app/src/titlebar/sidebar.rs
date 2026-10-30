//! Side tabs (Settings › Appearance › Tabs: Side): the session's tabs as a
//! list in a sidebar under the titlebar, doing all the titlebar strip does:
//! select, × on hover, drag to reorder, status marks, the colour of the app
//! in another tab, grow-in and collapse-out motion, scrolling with soft
//! edges and the active tab kept in view. New Tab ends the list. Dragging
//! the right edge resizes it for every window (double-click: default
//! width); the titlebar's sidebar button hides it in this window.

use std::time::Instant;

use gpui::{AnyElement, Context, DragMoveEvent, IntoElement, Window, canvas, div, prelude::*, px, rgb};

use super::{CLOSE_GAP, CLOSE_SIZE, FADE_WIDTH, PILL_HEIGHT, TabDrag, claim_press, edge_fades};
use crate::theme::Continuous as _;
use crate::{
    assets::icon,
    hosts::{Density, TabPosition},
    motion::{self, TabGhost},
    theme,
    workspace::Workspace,
};

/// The sidebar's width before it is ever dragged, and its limits.
pub const SIDEBAR_DEFAULT: f32 = 200.0;
const SIDEBAR_MIN: f32 = 140.0;
const SIDEBAR_MAX: f32 = 420.0;
/// Most of the window's width the sidebar takes.
const SIDEBAR_SHARE: f32 = 0.4;
const ROW_HEIGHT: f32 = 30.0;
const ROW_GAP: f32 = 2.0;
const ROW_PAD: f32 = 8.0;
/// The close button's gap to the row's right edge: as above and below it.
const SIDE_CLOSE_INSET: f32 = (ROW_HEIGHT - CLOSE_SIZE) / 2.0;
/// The list's inset from the sidebar's edges.
const INSET: f32 = 8.0;
/// Pointer target of the resizable right edge, inside the sidebar.
const EDGE_HIT: f32 = 6.0;

/// The sidebar's width for a stored `width` in a window `window` wide:
/// within its limits and never more than [`SIDEBAR_SHARE`] of the window
/// (down to its minimum). An unreadable width is the default.
#[must_use]
pub fn sidebar_width(width: f32, window: f32) -> f32 {
    let width = if width.is_finite() { width } else { SIDEBAR_DEFAULT };
    let max = (window * SIDEBAR_SHARE).clamp(SIDEBAR_MIN, SIDEBAR_MAX);
    width.max(SIDEBAR_MIN).min(max)
}

/// The (invisible) view a resize of the sidebar's edge carries.
struct SidebarGrab;

impl Render for SidebarGrab {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

impl Workspace {
    /// The sidebar's width while side tabs show in this window.
    pub(crate) fn tab_sidebar_width(&self, window: &Window) -> Option<f32> {
        (self.state.tabs == TabPosition::Side && !self.sidebar_hidden && self.view.is_some()).then(|| {
            let width = self.sidebar_resize.map_or(self.state.tab_sidebar_width, |(_, w)| w);
            sidebar_width(width, f32::from(window.viewport_size().width))
        })
    }

    pub(crate) fn set_tab_position(&mut self, tabs: TabPosition, cx: &mut Context<Self>) {
        tracing::debug!(?tabs, "tab position");
        // Choosing Side again shows a sidebar hidden here.
        self.sidebar_hidden = false;
        self.shared.update(cx, |shared, cx| shared.update_state(|s| s.tabs = tabs, cx));
        cx.notify();
    }

    fn set_tab_sidebar_width(&mut self, width: f32, cx: &mut Context<Self>) {
        self.shared.update(cx, |shared, cx| shared.update_state(|s| s.tab_sidebar_width = width, cx));
    }

    fn end_sidebar_press(&mut self, cx: &mut Context<Self>) {
        if !cx.has_active_drag() && self.sidebar_resize.take().is_some() {
            cx.notify();
        }
    }

    /// The titlebar button hiding and showing this window's sidebar.
    pub(crate) fn render_sidebar_toggle(&self, cx: &mut Context<Self>) -> AnyElement {
        let label = if self.sidebar_hidden { "Show Tabs" } else { "Hide Tabs" };
        div()
            .id("sidebar-toggle")
            .flex()
            .items_center()
            .justify_center()
            .flex_none()
            .mr(px(4.0))
            .size(px(PILL_HEIGHT))
            .capsule()
            .hover(|s| s.bg(self.chrome.hover()))
            .cursor_pointer()
            .tooltip(crate::tooltip::Tooltip::text(label))
            .child(icon("sidebar-left", 15.0, self.chrome.icon))
            .map(claim_press)
            .on_click(cx.listener(|this, _, _, cx| {
                this.sidebar_hidden = !this.sidebar_hidden;
                cx.notify();
            }))
            .into_any_element()
    }

    /// The sidebar of side tabs, when this window shows it.
    pub(crate) fn render_tab_sidebar(&self, now: Instant, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let width = self.tab_sidebar_width(window)?;
        let view = self.view.as_ref()?;
        let several = view.session.windows.len() > 1;
        let active_index = view.session.windows.iter().position(|w| w.id == view.session.active_window);
        let active_id = view.session.active_window.clone();
        self.note_active_tab(TabPosition::Side, &active_id);
        let scroll = &self.sidebar_scroll;
        let mut list = div()
            .id("tab-list")
            .flex()
            .flex_col()
            .w_full()
            .min_h_0()
            .flex_shrink(1.0)
            .pl(px(INSET))
            // The panes keep their own margin (Comfortable): the gap on the
            // right matches the left, not both added up.
            .pr(px(INSET - self.panes_bottom()))
            // Room for the insertion bar before the first tab.
            .py(px(ROW_GAP))
            .overflow_y_scroll()
            .track_scroll(scroll)
            // Scrolled by hand: stop pulling the active tab back into view.
            .on_scroll_wheel(cx.listener(|this, _, _, _| this.tab_follow.set(false)));
        let mut ghosts: Vec<&TabGhost> = self.motion.tab_ghosts.iter().collect();
        ghosts.sort_by_key(|g| g.index);
        let mut ghosts = ghosts.into_iter().peekable();
        // As in the titlebar's strip: children are the tabs and ghosts only
        // (New Tab comes after them all), and rows fade where it scrolls on.
        let (mut child, mut active_child, mut active_entering) = (0, None, false);
        let (fade_top, fade_bottom) = edge_fades(f32::from(scroll.offset().y), f32::from(scroll.max_offset().y), self.sidebar_room.get());
        let bounds = scroll.bounds();
        let fade = |child: usize| {
            scroll.bounds_for_item(child).map_or(1.0, |row| {
                let ramp = |inset: gpui::Pixels| 0.3 + 0.7 * (f32::from(inset) / FADE_WIDTH).clamp(0.0, 1.0);
                let top = if fade_top { ramp(row.top() - bounds.top()) } else { 1.0 };
                let bottom = if fade_bottom { ramp(bounds.bottom() - row.bottom()) } else { 1.0 };
                top.min(bottom)
            })
        };
        let mut gap_before = 0.0;
        for (index, w) in view.session.windows.iter().enumerate() {
            while let Some(ghost) = ghosts.next_if(|g| g.index <= index) {
                let keep = 1.0 - motion::progress(ghost.started, motion::TAB_EXIT, now).unwrap_or(1.0);
                if let Some(el) = render_side_ghost(ghost, child > 0, self.chrome, now) {
                    list = list.child(el);
                    if child == 0 {
                        gap_before = keep;
                    }
                    child += 1;
                }
            }
            let appear = self.motion.tab_enter.get(&w.id).and_then(|s| motion::progress(*s, motion::TAB_ENTER, now)).unwrap_or(1.0);
            if w.id == active_id {
                active_child = Some(child);
                active_entering = appear < 1.0;
            }
            let lead = ROW_GAP * appear * gap_before;
            gap_before = 1.0;
            let selected = several && active_index == Some(index);
            list = list.child(self.render_side_tab(w, index, selected, several, appear, fade(child), lead, now, cx));
            child += 1;
        }
        for ghost in ghosts {
            if let Some(ghost) = render_side_ghost(ghost, child > 0, self.chrome, now) {
                list = list.child(ghost);
                child += 1;
            }
        }
        self.step_tab_reveal(scroll, true, &active_id, active_child, active_entering, !cx.reduce_motion(), window);
        let last = view.session.windows.len().saturating_sub(1);
        let to_end = move |this: &mut Self, ev: &DragMoveEvent<TabDrag>, cx: &mut Context<Self>| {
            if ev.bounds.contains(&ev.event.position) {
                let id = ev.drag(cx).window_id.clone();
                this.set_tab_drop(&id, Some(super::TabDrop::Insert { target: last, after: true }), cx);
            }
        };
        let open = self.launcher.is_open_for_tab();
        let chrome = self.chrome;
        list = list.child(
            div()
                .id("side-new-tab")
                .flex()
                .flex_none()
                .items_center()
                .gap(px(8.0))
                .w_full()
                .h(px(ROW_HEIGHT))
                .mt(px(if child > 0 { ROW_GAP } else { 0.0 }))
                .px(px(ROW_PAD))
                .continuous(crate::radius::MENU_ROW)
                .text_color(rgb(chrome.muted))
                .hover(|s| s.bg(chrome.hover()))
                .when(open, |el| el.bg(chrome.hover()))
                .cursor_pointer()
                .child(icon("plus", 13.0, chrome.icon))
                .child("New Tab")
                .on_drag_move(cx.listener(move |this, ev, _, cx| to_end(this, ev, cx)))
                .on_click(cx.listener(|this, _, window, cx| this.toggle_launcher(crate::commands::Command::NewTabChooser, window, cx))),
        );
        // The space below the list means "move to the end", as after the
        // titlebar's strip; its height tells whether the list overflows.
        let room = self.sidebar_room.clone();
        let spacer = div()
            .relative()
            .flex_1()
            .w_full()
            .child(canvas(move |b, _, _| room.set(f32::from(b.size.height)), |_, (), _, _| {}).absolute().inset_0())
            .on_drag_move(cx.listener(move |this, ev, _, cx| to_end(this, ev, cx)));
        Some(
            div()
                .id("tab-sidebar")
                .relative()
                .flex()
                .flex_col()
                .flex_none()
                .w(px(width))
                .h_full()
                .when(self.frosted(), |el| el.bg(chrome.fill()))
                // Compact: the same hairline as the pane dividers.
                .when(self.density() == Density::Compact, |el| el.border_r_1().border_color(rgb(theme::border())))
                .font_family(theme::ui_font())
                .text_size(px(12.5))
                .child(list)
                .child(spacer)
                .child(self.render_sidebar_edge(width, cx))
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
                }))
                .on_drag_move(cx.listener(|this, ev: &DragMoveEvent<SidebarGrab>, window, cx| {
                    let Some((grab, old)) = this.sidebar_resize else { return };
                    let new = sidebar_width(f32::from(ev.event.position.x - ev.bounds.left()) - grab, f32::from(window.viewport_size().width));
                    if (new - old).abs() > 0.1 {
                        this.sidebar_resize = Some((grab, new));
                        cx.notify();
                    }
                }))
                .into_any_element(),
        )
    }

    /// The sidebar's right edge: drag to resize, double-click for the
    /// default width. An accent line shows while it is dragged.
    fn render_sidebar_edge(&self, width: f32, cx: &mut Context<Self>) -> AnyElement {
        let workspace = cx.entity().downgrade();
        div()
            .id("tab-sidebar-edge")
            .absolute()
            .top_0()
            .bottom_0()
            .right_0()
            .w(px(EDGE_HIT))
            .occlude()
            .cursor(gpui::CursorStyle::ResizeLeftRight)
            .when(self.sidebar_resize.is_some(), |el| {
                el.child(div().absolute().top_0().bottom_0().right_0().w(px(2.0)).bg(rgb(theme::accent()).opacity(0.35)))
            })
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, e: &gpui::MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    if e.click_count == 2 {
                        this.sidebar_resize = None;
                        this.set_tab_sidebar_width(SIDEBAR_DEFAULT, cx);
                    } else {
                        // Where on the edge it was pressed: the width follows the pointer from there.
                        this.sidebar_resize = Some((f32::from(e.position.x) - width, width));
                    }
                    cx.notify();
                }),
            )
            // Saved once released, however the drag ends.
            .on_drag(SidebarGrab, move |_, _, _, cx| {
                let workspace = workspace.clone();
                cx.new(|cx| {
                    cx.on_release(move |_, cx| {
                        _ = workspace.update(cx, |this, cx| {
                            if let Some((_, width)) = this.sidebar_resize.take() {
                                this.set_tab_sidebar_width(width, cx);
                            }
                            cx.notify();
                        });
                    })
                    .detach();
                    SidebarGrab
                })
            })
            // A press that never became a drag (a drag's release saves it).
            .on_mouse_up(gpui::MouseButton::Left, cx.listener(|this, _, _, cx| this.end_sidebar_press(cx)))
            .on_mouse_up_out(gpui::MouseButton::Left, cx.listener(|this, _, _, cx| this.end_sidebar_press(cx)))
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn render_side_tab(
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
        let chrome = self.chrome;
        let drag = TabDrag { window_id: w.id.clone(), title: title.clone(), icon: self.tab_icon(w), offset: gpui::Point::default(), chrome };
        // Insertion bar on the side a dragged tab would drop, in the gap.
        let drop = self.tab_drop.filter(|d| d.target() == index && self.tab_dragging.is_some());
        let insert = match drop {
            Some(super::TabDrop::Insert { after, .. }) => Some(after),
            _ => None,
        };
        // Dropped here, the dragged tab's blocks join this one: outlined.
        let merging = matches!(drop, Some(super::TabDrop::Merge { .. }));
        let workspace = cx.entity().downgrade();
        let window_id = w.id.clone();
        let group = format!("side-tab-{}", w.id);
        let hover = chrome.hover();
        let hover_id = w.id.clone();
        let row = div()
            .flex()
            .flex_row()
            .items_center()
            .flex_none()
            .w_full()
            .h(px(ROW_HEIGHT))
            .pl(px(ROW_PAD))
            .pr(px(ROW_PAD - (ROW_PAD - SIDE_CLOSE_INSET) * close))
            .continuous(crate::radius::MENU_ROW)
            .when(several && selected, |el| el.bg(chrome.selected()))
            .when(!selected, |el| el.group_hover(group.clone(), |s| s.bg(hover)))
            .when(merging, |el| el.bg(hover).border_1().border_color(rgb(chrome.accent)))
            .child(self.block_icon_view(&w.focused, "side-tab", 13.0, if primary { chrome.accent } else { chrome.icon }, cx))
            .child(div().ml(px(8.0)).flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(title))
            .when_some(self.window_mark(w), |el, mark| {
                let glyph = crate::block_status::glyph(mark, &format!("side-tab-{}", w.id), 10.0, chrome.surface, chrome.accent, cx);
                el.child(div().flex_none().ml(px(6.0)).flex().items_center().child(glyph))
            })
            // Zero width unless this tab is hovered; grows and fades in.
            .when(close > 0.0, |el| {
                el.child(
                    div().flex().flex_none().justify_end().w(px((CLOSE_GAP + CLOSE_SIZE) * close)).overflow_hidden().opacity(close).child(
                        div()
                            .id(gpui::ElementId::Name(format!("side-close-{}", w.id).into()))
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(CLOSE_SIZE))
                            .flex_none()
                            .capsule()
                            .hover(|s| s.bg(hover))
                            .child(icon("close", 12.0, chrome.icon))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close(crate::close_confirm::CloseTarget::Tab(window_id.clone()), window, cx);
                            })),
                    ),
                )
            });
        let entering = appear < 1.0;
        div()
            .id(gpui::ElementId::Name(group.clone().into()))
            .group(group)
            .relative()
            .flex()
            .flex_col()
            .flex_none()
            .w_full()
            .mt(px(lead))
            // Grows in from nothing, as a top tab widens in.
            .when(entering, |el| el.h(px(ROW_HEIGHT * appear)).overflow_hidden())
            // The source stays in place, faded, under the drag preview.
            .opacity(fade * appear * if self.tab_dragging.as_ref() == Some(&w.id) { 0.4 } else { 1.0 })
            .text_color(rgb(if primary { chrome.ink } else { chrome.secondary }))
            .cursor_pointer()
            .child(row)
            .when_some(insert, |el, after| {
                let at = px(-(ROW_GAP + 2.0) / 2.0);
                el.child(
                    div()
                        .absolute()
                        .left(px(6.0))
                        .right(px(6.0))
                        .h(px(2.0))
                        .map(|el| if after { el.bottom(at) } else { el.top(at) })
                        .continuous(1.0)
                        .bg(rgb(chrome.accent)),
                )
            })
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
                    let (b, at) = (ev.bounds, ev.event.position.y);
                    let drop = super::tab_drop_at(index, f32::from(b.top()), f32::from(b.bottom()), f32::from(at), this.merges_tabs());
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
}

/// A closed side tab collapsing in height and fading at its old position.
/// Visual only: no listeners, and the server close was already sent.
fn render_side_ghost(ghost: &TabGhost, lead: bool, chrome: crate::adaptive::Chrome, now: Instant) -> Option<AnyElement> {
    let keep = 1.0 - motion::progress(ghost.started, motion::TAB_EXIT, now)?;
    Some(
        div()
            .id(gpui::ElementId::Name(format!("side-ghost-{}", ghost.id).into()))
            .flex_none()
            .w_full()
            .h(px(ROW_HEIGHT * keep))
            // Its gap collapses with it, so neighbours close up smoothly.
            .when(lead, |el| el.mt(px(ROW_GAP * keep)))
            .overflow_hidden()
            .opacity(keep)
            .text_color(rgb(chrome.secondary))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .h(px(ROW_HEIGHT))
                    .px(px(ROW_PAD))
                    .child(icon(ghost.icon, 13.0, chrome.icon))
                    .child(div().flex_1().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(ghost.title.clone())),
            )
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use gpui::{Bounds, Pixels, TestAppContext, size};

    use super::*;

    type Seen = Rc<RefCell<Vec<(&'static str, Bounds<Pixels>)>>>;

    /// The sidebar's list and the space below it, as `render_tab_sidebar` lays them out.
    struct Column {
        rows: usize,
        seen: Seen,
    }

    fn probe(name: &'static str, seen: &Seen) -> impl IntoElement {
        let seen = seen.clone();
        canvas(move |b, _, _| seen.borrow_mut().push((name, b)), |_, _, _, _| {}).absolute().top_0().left_0().size_full()
    }

    impl Render for Column {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let rows = (0..self.rows).map(|i| div().flex_none().w_full().h(px(ROW_HEIGHT)).mt(px(if i > 0 { ROW_GAP } else { 0.0 })));
            let list = div()
                .id("tab-list")
                .relative()
                .flex()
                .flex_col()
                .w_full()
                .min_h_0()
                .flex_shrink(1.0)
                .px(px(INSET))
                .py(px(ROW_GAP))
                .overflow_y_scroll()
                .children(rows)
                .child(probe("list", &self.seen));
            let spacer = div().relative().flex_1().w_full().child(probe("spacer", &self.seen));
            div().flex().flex_col().w(px(SIDEBAR_DEFAULT)).h_full().child(list).child(spacer)
        }
    }

    #[gpui::test]
    fn the_list_fits_its_rows_and_scrolls_once_they_overflow(cx: &mut TestAppContext) {
        for (rows, list, spacer) in [(3, 2.0 * ROW_GAP + 3.0 * ROW_HEIGHT + 2.0 * ROW_GAP, 400.0 - 98.0), (30, 400.0, 0.0)] {
            let seen = Seen::default();
            let (_, cx) = cx.add_window_view({
                let seen = seen.clone();
                move |_, _| Column { rows, seen }
            });
            cx.simulate_resize(size(px(800.0), px(400.0)));
            cx.run_until_parked();
            let seen = seen.borrow();
            let last = |name| seen.iter().rev().find(|(n, _)| *n == name).map(|(_, b)| f32::from(b.size.height)).unwrap();
            assert!((last("list") - list).abs() < 0.6, "{rows} rows: list {} for {list}", last("list"));
            assert!((last("spacer") - spacer).abs() < 0.6, "{rows} rows: spacer {} for {spacer}", last("spacer"));
        }
    }

    #[test]
    fn width_stays_within_its_limits_and_the_window() {
        let near = |a: f32, b: f32| (a - b).abs() < 0.01;
        assert!(near(sidebar_width(SIDEBAR_DEFAULT, 1400.0), SIDEBAR_DEFAULT), "the default fits any normal window");
        assert!(near(sidebar_width(60.0, 1400.0), SIDEBAR_MIN));
        assert!(near(sidebar_width(900.0, 2400.0), SIDEBAR_MAX));
        // A narrow window caps it at its share, never below the minimum.
        assert!(near(sidebar_width(300.0, 600.0), 240.0));
        assert!(near(sidebar_width(300.0, 200.0), SIDEBAR_MIN));
        // Unreadable values (a hand-edited file, a window not laid out yet).
        assert!(near(sidebar_width(f32::NAN, 1400.0), SIDEBAR_DEFAULT));
        assert!(near(sidebar_width(f32::INFINITY, 1400.0), SIDEBAR_DEFAULT));
        assert!(near(sidebar_width(250.0, f32::NAN), 250.0));
        assert!(near(sidebar_width(250.0, 0.0), SIDEBAR_MIN));
    }
}
