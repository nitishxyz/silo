//! Titlebar: one inline row with the traffic-light gap, the session pill,
//! compact window tabs and, at the far right, the Update pill (once an
//! update is ready), the appearance toggle and the "+" button, all on the
//! terminal's own background. Tabs are small
//! pills: inactive ones filled only on hover, the current one a subtle step
//! brighter.
//!
//! When the tabs do not fit, only they scroll horizontally (trackpad, or a
//! mouse wheel's vertical motion); the pill and the right-hand buttons stay
//! put. Soft fades in the titlebar color mark a side with hidden tabs, and
//! the active tab is scrolled fully into view when it changes.

use std::time::Instant;

use gpui::{AnyElement, Context, DragMoveEvent, FontWeight, IntoElement, Window, div, prelude::*, px, rgb};
use silo_proto::control::Method;

use crate::theme::Continuous as _;
use crate::{
    assets::icon,
    motion::{self, TabGhost},
    theme,
    workspace::{TITLEBAR_HEIGHT, Workspace},
};

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
/// Leading and resting trailing padding inside a tab pill.
const TAB_PAD: f32 = 10.0;
/// Space between a shown close button and the pill's right edge.
const CLOSE_INSET: f32 = 2.0;
/// Close button diameter and its space before it; both collapse to zero
/// when the tab is not hovered.
const CLOSE_SIZE: f32 = 16.0;
const CLOSE_GAP: f32 = 4.0;
/// Gap between the session pill and the first tab.
const PILL_GAP: f32 = TAB_GAP;
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
pub fn edge_fades(offset: f32, max: f32) -> (bool, bool) {
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
            .px(px(TAB_PAD))
            .capsule()
            .map(|el| match theme::veils() {
                // A floating pill of the material: veil and hairline.
                Some(_) => el.bg(theme::overlay_fill()).border_1().border_color(rgb(theme::border())).text_color(rgb(theme::foreground())),
                None => el.bg(rgb(theme::tab_selected_on(self.chrome.surface))).shadow_md().text_color(rgb(self.chrome.ink)),
            })
            .font_family(theme::ui_font())
            .text_size(px(12.0))
            .child(icon(self.icon, 12.0, self.chrome.accent))
            .child(div().ml(px(6.0)).min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(self.title.clone()));
        div().pl(self.offset.x + px(10.0)).pt(self.offset.y + px(10.0)).child(pill)
    }
}

impl Workspace {
    /// The tab's glyph: a globe when the window's focused block is a browser.
    pub(crate) fn tab_icon(&self, w: &silo_proto::model::WindowInfo) -> &'static str {
        self.block_icon(&w.focused)
    }

    /// A block's glyph: a globe for a browser, else a terminal.
    pub(crate) fn block_icon(&self, id: &str) -> &'static str {
        let block = self.view.as_ref().and_then(|v| v.blocks.iter().find(|b| b.id == id));
        match crate::browser_pane::kind_of(block) {
            crate::browser_pane::Kind::Browser => "globe",
            crate::browser_pane::Kind::Terminal => "terminal",
            crate::browser_pane::Kind::Unsupported => "info",
        }
    }

    /// Block `id`'s icon slot at `size`: the spinner while its page loads,
    /// its favicon, else its glyph. `slot` keeps spinners apart.
    pub(crate) fn block_icon_view(&self, id: &str, slot: &str, size: f32, color: u32, cx: &gpui::App) -> AnyElement {
        use crate::browser_pane::Icon;
        let pane = self.browsers.get(id).map(|(p, _)| p.read(cx));
        match pane.map_or(Icon::Glyph(self.block_icon(id)), crate::browser_pane::BrowserPane::icon) {
            Icon::Spinner => crate::browser_pane::spinner(&format!("{slot}-{id}"), size, color, cx),
            Icon::Favicon => match pane.and_then(crate::browser_pane::BrowserPane::favicon_image) {
                Some(image) => gpui::img(image).flex_none().size(px(size)).into_any_element(),
                None => icon("globe", size, color).into_any_element(),
            },
            Icon::Glyph(glyph) => icon(glyph, size, color).into_any_element(),
        }
    }

    pub(crate) fn render_titlebar(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let several = self.view.as_ref().is_some_and(|v| v.session.windows.len() > 1);
        let hover = self.chrome.hover();
        let fullscreen = window.is_fullscreen();
        let inset = if fullscreen { FULLSCREEN_INSET } else { TRAFFIC_LIGHTS };
        if self.last_fullscreen.replace(Some(fullscreen)) != Some(fullscreen) {
            tracing::debug!(fullscreen, inset, "titlebar inset");
        }
        let now = Instant::now();
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
            .child(self.render_pill(cx));
        if let Some(view) = &self.view {
            let active_index = view.session.windows.iter().position(|w| w.id == view.session.active_window);
            let active_id = view.session.active_window.clone();
            if self.tab_last_active.replace(Some(active_id.clone())).as_ref() != Some(&active_id) {
                *self.tab_reveal.borrow_mut() = Some((active_id.clone(), REVEAL_FRAMES));
            }
            // Tabs size to their titles, ellipsized beyond `TAB_MAX`, and
            // scroll when they overflow. Vertical wheel motion scrolls
            // horizontally too (GPUI maps it when only x scrolls).
            let mut tabs = div()
                .id("tab-strip")
                .flex()
                .flex_row()
                .items_center()
                .gap(px(TAB_GAP))
                .pl(px(PILL_GAP))
                // Room for the insertion bar after the last tab.
                .pr(px(TAB_GAP))
                .h_full()
                .min_w_0()
                .flex_shrink(1.0)
                .overflow_x_scroll()
                .track_scroll(&self.tab_scroll);
            let mut ghosts: Vec<&TabGhost> = self.motion.tab_ghosts.iter().collect();
            ghosts.sort_by_key(|g| g.index);
            let mut ghosts = ghosts.into_iter().peekable();
            // Child index of the active tab among tabs and ghosts.
            let mut child = 0;
            let mut active_child = None;
            let mut active_entering = false;
            let (fade_left, fade_right) = edge_fades(f32::from(self.tab_scroll.offset().x), f32::from(self.tab_scroll.max_offset().x));
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
            for (index, w) in view.session.windows.iter().enumerate() {
                while let Some(ghost) = ghosts.next_if(|g| g.index <= index) {
                    if let Some(ghost) = render_tab_ghost(ghost, self.chrome, now) {
                        tabs = tabs.child(ghost);
                        child += 1;
                    }
                }
                let selected = several && active_index == Some(index);
                let appear = self.motion.tab_enter.get(&w.id).and_then(|s| motion::progress(*s, motion::TAB_ENTER, now)).unwrap_or(1.0);
                if w.id == active_id {
                    active_child = Some(child);
                    active_entering = appear < 1.0;
                }
                tabs = tabs.child(self.render_tab(w, index, selected, several, appear, fade(child), now, cx));
                child += 1;
            }
            for ghost in ghosts {
                tabs = tabs.children(render_tab_ghost(ghost, self.chrome, now));
            }
            self.step_tab_reveal(&active_id, active_child, active_entering, !cx.reduce_motion(), window);
            let strip_view = div().relative().flex().flex_row().h_full().min_w_0().flex_shrink(1.0).child(tabs);
            let comfortable = !compact;
            let menu_open = self.appearance_menu.open;
            let new_tab_open = self.new_tab_menu.is_open();
            // The strip and the empty space after it take tab drops. The
            // space after the last tab means "move to the end".
            let last = view.session.windows.len().saturating_sub(1);
            let spacer = div().flex_1().h_full().on_drag_move(cx.listener(move |this, ev: &DragMoveEvent<TabDrag>, _, cx| {
                if ev.bounds.contains(&ev.event.position) {
                    let id = ev.drag(cx).window_id.clone();
                    this.set_tab_drop(&id, Some((last, true)), cx);
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
                    if let Some((target, after)) = this.tab_drop.take() {
                        this.move_tab(&drag.window_id, target, after, cx);
                    }
                    cx.notify();
                }));
            bar = bar
                .child(drop_zone)
                .children(self.render_watchers())
                .children(self.render_update_pill(cx))
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
                        .when(!menu_open, |el| {
                            el.tooltip(crate::tooltip::Tooltip::text(if comfortable { "Appearance: Comfortable" } else { "Appearance: Compact" }))
                        })
                        .child(icon("eye", 15.0, self.chrome.icon))
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
                        .child(self.new_tab_menu.button_probe())
                        .map(claim_press)
                        .on_click(cx.listener(|this, _, window, cx| this.toggle_new_tab_menu(window, cx))),
                );
        }
        bar.into_any_element()
    }

    /// Advance a pending scroll-into-view of the active tab by one frame,
    /// using the previous frame's layout. It stays pending (and requests
    /// frames) while the tab is still growing in or not laid out yet.
    fn step_tab_reveal(&self, active_id: &str, child: Option<usize>, entering: bool, smooth: bool, window: &mut Window) {
        let mut pending = self.tab_reveal.borrow_mut();
        let Some((id, frames)) = pending.take() else {
            return;
        };
        if id != active_id || frames == 0 {
            return;
        }
        let Some(child) = child else { return };
        let viewport = self.tab_scroll.bounds();
        let keep = match self.tab_scroll.bounds_for_item(child) {
            Some(item) if viewport.size.width > px(0.0) => {
                let offset = self.tab_scroll.offset();
                let from = f32::from(offset.x);
                let target = reveal_offset(
                    from,
                    f32::from(viewport.size.width),
                    f32::from(item.left() - viewport.left()),
                    f32::from(item.size.width),
                    REVEAL_MARGIN,
                    f32::from(self.tab_scroll.max_offset().x),
                );
                let next = if smooth { ease_toward(from, target) } else { target };
                if (next - from).abs() > 0.01 {
                    self.tab_scroll.set_offset(gpui::point(px(next), offset.y));
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
        let insert = self.tab_drop.filter(|&(target, _)| target == index && self.tab_dragging.is_some()).map(|(_, after)| after);
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
            .pl(px(TAB_PAD))
            // Balanced padding at rest. A shown close button takes the
            // trailing padding down to `CLOSE_INSET`, so it sits at the
            // pill's right edge.
            .pr(px(TAB_PAD - (TAB_PAD - CLOSE_INSET) * close))
            .capsule()
            .when(several && selected, |el| el.bg(fill))
            .when(!selected, |el| el.group_hover(group.clone(), |s| s.bg(hover)))
            .child(self.block_icon_view(&w.focused, "tab", 12.0, if primary { chrome.accent } else { chrome.icon }, cx))
            .child(div().ml(px(6.0)).min_w_0().flex_shrink(1.0).overflow_hidden().whitespace_nowrap().text_ellipsis().child(title))
            .when_some(self.window_mark(w), |el, mark| {
                let glyph = crate::block_status::glyph(mark, &format!("tab-{}", w.id), 10.0, chrome.surface, chrome.accent, cx);
                el.child(div().flex_none().ml(px(6.0)).flex().items_center().child(glyph))
            })
            .when_some(swatch, |el, color| el.child(div().flex_none().ml(px(6.0)).size(px(7.0)).capsule().bg(rgb(color))))
            // Zero width unless this tab is hovered; grows and fades in.
            .when(close > 0.0, |el| {
                el.child(
                    div().flex().flex_none().justify_end().w(px((CLOSE_GAP + CLOSE_SIZE) * close)).overflow_hidden().opacity(close).child(
                        div()
                            .id(gpui::ElementId::Name(format!("close-{}", w.id).into()))
                            .flex()
                            .items_center()
                            .justify_center()
                            .size(px(CLOSE_SIZE))
                            .flex_none()
                            .capsule()
                            .hover(|s| s.bg(hover))
                            .child(icon("close", 12.0, chrome.icon))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.call(Method::WindowClose { window_id: window_id.clone(), force: false });
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
            // Full size in the scrolling strip; overflow scrolls instead.
            .flex_none()
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
                    let after = ev.event.position.x > ev.bounds.center().x;
                    let id = ev.drag(cx).window_id.clone();
                    this.set_tab_drop(&id, Some((index, after)), cx);
                }
            }))
            .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                this.motion.set_hover(&hover_id, *hovered, Instant::now(), !cx.reduce_motion());
                cx.notify();
            }))
            .on_click(cx.listener(move |this, _, _, _| this.select_window(index)))
            .into_any_element()
    }

    /// Record where a drag of `window_id` would drop; a no-op spot clears it.
    fn set_tab_drop(&mut self, window_id: &str, drop: Option<(usize, bool)>, cx: &mut Context<Self>) {
        let from = self.view.as_ref().and_then(|v| v.session.windows.iter().position(|w| w.id == window_id));
        let drop = drop.filter(|&(target, after)| from.is_some_and(|from| drop_index(from, target, after).is_some()));
        if self.tab_drop != drop {
            self.tab_drop = drop;
            cx.notify();
        }
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
        let me = self.control().map(|c| c.client_id());
        let labels = watcher_labels(&view.watchers, me);
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

    /// The Update pill: accent-filled once an update is staged (a click
    /// installs it and restarts), outlined when this copy of Silo cannot
    /// be replaced; nothing otherwise, downloads included.
    fn render_update_pill(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        use crate::updater::State;
        let (label, tooltip, ready) = match self.updater.read(cx).state() {
            State::Ready { version, .. } => ("Update", Some(crate::updater::ready(version)), true),
            State::Installing => ("Updating…", None, false),
            State::Blocked { version, reason } => ("Can't Update", Some(format!("Silo {version} is available. {reason}.")), false),
            State::Idle | State::Downloading(_) => return None,
        };
        let chrome = self.chrome;
        let pill = div()
            .id("update")
            .flex()
            .flex_none()
            .items_center()
            .ml(px(6.0))
            .h(px(TAB_HEIGHT - 2.0))
            .px(px(11.0))
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
            .child(label)
            .when_some(tooltip, |el, text| el.tooltip(crate::tooltip::Tooltip::below(text, TITLEBAR_HEIGHT)))
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
fn render_tab_ghost(ghost: &TabGhost, chrome: crate::adaptive::Chrome, now: Instant) -> Option<AnyElement> {
    let gone = motion::progress(ghost.started, motion::TAB_EXIT, now)?;
    let keep = 1.0 - gone;
    Some(
        div()
            .id(gpui::ElementId::Name(format!("tab-ghost-{}", ghost.id).into()))
            .flex()
            .flex_col()
            .flex_none()
            .max_w(px(TAB_MAX * keep))
            // Collapse the row gap too, so neighbours close up smoothly.
            .mr(px(-TAB_GAP * gone))
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
        assert_eq!(edge_fades(0.0, 0.0), (false, false), "everything fits");
        assert_eq!(edge_fades(0.0, 200.0), (false, true), "at the start");
        assert_eq!(edge_fades(-80.0, 200.0), (true, true), "in the middle");
        assert_eq!(edge_fades(-200.0, 200.0), (true, false), "at the end");
        assert_eq!(edge_fades(-199.8, 200.0), (true, false), "sub-pixel rounding at the end");
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
