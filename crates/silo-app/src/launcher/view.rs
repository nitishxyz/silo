//! The launcher dialog, centred over the window like the palette, and the
//! ghost of a split's destination drawn in the layout.

use gpui::{AnyElement, Context, FontWeight, HighlightStyle, IntoElement, StyledText, Window, div, prelude::*, px, relative, rgb};
use silo_proto::model::Axis;

use super::{Kind, Row, Target, row_hint};
use crate::theme::Continuous as _;
use crate::{assets::icon, radius::CardCorners, theme, workspace::Workspace};

impl Workspace {
    pub(crate) fn render_launcher(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let dialog = self.launcher.dialog.as_ref()?;
        let highlight = HighlightStyle { color: Some(rgb(theme::accent()).into()), font_weight: Some(FontWeight::BOLD), ..Default::default() };
        let mut list = div().flex().flex_col().py(px(4.0));
        for (index, row) in dialog.rows.iter().enumerate() {
            let selected = index == dialog.selected;
            let kind = row.kind();
            let supported = self.launcher_supported(kind, &dialog.target);
            let (glyph, label, ranges, detail) = match row {
                Row::Kind(kind, ranges) => (kind.icon(), kind.label().to_owned(), ranges.clone(), None),
                Row::Open { url, title } if title.is_empty() => ("globe", format!("Open {url}"), Vec::new(), None),
                Row::Open { url, title } => ("globe", title.clone(), Vec::new(), Some(url.clone())),
                Row::Run(command) => ("terminal", format!("Run {command}"), Vec::new(), Some("in a new terminal".to_owned())),
            };
            let hint = matches!(row, Row::Kind(..)).then(|| row_hint(kind, &dialog.target, dialog.same)).flatten();
            // A host without the feature: muted, saying so instead of the shortcut.
            let hint = if supported { hint } else { Some(self.launcher_unsupported_note(kind)) };
            let digit = (index < 9 && dialog.field.read(cx).text.is_empty()).then(|| (index + 1).to_string());
            let label = StyledText::new(label).with_highlights(ranges.into_iter().map(|r| (r, highlight)).collect::<Vec<_>>());
            list = list.child(
                div()
                    .id(("launcher-row", index))
                    .flex_none()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .h(px(36.0))
                    .mx(px(6.0))
                    .px(px(10.0))
                    .continuous(8.0)
                    .when(selected, |el| el.bg(theme::row_fill()))
                    .when(!selected, |el| el.hover(|s| s.bg(theme::hover_fill())))
                    .when(supported, |el| el.cursor_pointer())
                    .child(div().w(px(16.0)).flex_none().flex().justify_center().child(icon(glyph, 14.0, theme::muted())))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_baseline()
                            .gap(px(8.0))
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .child(div().flex_none().text_color(rgb(if supported { theme::foreground() } else { theme::muted() })).child(label))
                            .children(detail.map(|d| div().min_w_0().text_ellipsis().text_size(px(11.5)).text_color(rgb(theme::muted())).child(d))),
                    )
                    .children(hint.map(|h| div().flex_none().text_size(px(11.5)).text_color(rgb(theme::muted())).child(h)))
                    .children(digit.map(|d| {
                        div()
                            .flex_none()
                            .px(px(6.0))
                            .py(px(1.0))
                            .continuous(5.0)
                            .bg(theme::hover_fill())
                            .text_size(px(11.5))
                            .text_color(rgb(theme::muted()))
                            .child(d)
                    }))
                    .on_mouse_move(cx.listener(move |this, _, _, cx| {
                        if let Some(d) = this.launcher.dialog.as_mut()
                            && d.selected != index
                        {
                            d.selected = index;
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, window, cx| this.launcher_choose(index, window, cx))),
            );
        }
        let title = match &dialog.target {
            Target::Tab => "New Tab",
            Target::Split { via, .. } => via.title(),
        };
        let empty = dialog.rows.is_empty();
        let card = div()
            .id("launcher")
            .occlude()
            .w(px(520.0))
            .flex()
            .flex_col()
            .continuous(14.0)
            .bg(theme::overlay_fill())
            .border_1()
            .border_color(rgb(theme::border()))
            .shadow_xl()
            .overflow_hidden()
            .font_family(theme::ui_font())
            .text_size(px(13.5))
            .text_color(rgb(theme::foreground()))
            .capture_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                let k = &event.keystroke;
                let plain = !(k.modifiers.platform || k.modifiers.control || k.modifiers.alt || k.modifiers.shift);
                if plain && this.launcher_digit(&k.key, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .child(div().px(px(16.0)).pt(px(10.0)).text_size(px(11.5)).text_color(rgb(theme::muted())).child(title))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(10.0))
                    .h(px(46.0))
                    .px(px(16.0))
                    .text_size(px(15.0))
                    .border_b_1()
                    .border_color(rgb(theme::border()))
                    .child(icon("plus", 16.0, theme::muted()))
                    .child(dialog.field.clone()),
            )
            .children(dialog.note.clone().map(|n| div().px(px(16.0)).pt(px(8.0)).text_size(px(11.5)).text_color(rgb(theme::muted())).child(n)))
            .when(empty, |el| el.child(div().px(px(16.0)).py(px(12.0)).text_color(rgb(theme::muted())).child("No matches")))
            .when(!empty, |el| el.child(list))
            .child(
                div()
                    .px(px(16.0))
                    .py(px(8.0))
                    .border_t_1()
                    .border_color(rgb(theme::border()))
                    .text_size(px(11.5))
                    .text_color(rgb(theme::muted()))
                    .child("↩ open · 1–9 pick · address · >command · @agent · esc cancel"),
            );
        let top = crate::workspace::TITLEBAR_HEIGHT + (f32::from(window.viewport_size().height) * 0.12).clamp(24.0, 120.0);
        Some(
            div()
                .id("launcher-backdrop")
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .occlude()
                .flex()
                .flex_col()
                .items_center()
                .pt(px(top))
                .on_mouse_down(gpui::MouseButton::Left, cx.listener(|this, _, window, cx| this.close_launcher(window, cx)))
                .on_mouse_down(gpui::MouseButton::Right, cx.listener(|this, _, window, cx| this.close_launcher(window, cx)))
                .child(card)
                .into_any_element(),
        )
    }

    /// The ghost of a split being chosen: an outlined placeholder over the
    /// half of the pane the new block will take (`second` half along
    /// `axis`), labelled with the selected kind. Client-side only.
    pub(crate) fn render_split_ghost(axis: Axis, kind: Option<Kind>, corners: CardCorners) -> AnyElement {
        let accent = rgb(theme::accent());
        let half = div().absolute().p(px(4.0)).map(|el| match axis {
            Axis::Horizontal => el.top_0().bottom_0().right_0().w(relative(0.5)),
            Axis::Vertical => el.left_0().right_0().bottom_0().h(relative(0.5)),
        });
        let label = kind.map(|kind| {
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap(px(8.0))
                .px(px(12.0))
                .py(px(6.0))
                .capsule()
                .bg(theme::overlay_fill())
                .font_family(theme::ui_font())
                .text_size(px(13.0))
                .text_color(accent)
                .child(icon(kind.icon(), 14.0, theme::accent()))
                .child(kind.label())
        });
        half.child(
            div()
                .id("launcher-ghost")
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .continuous_corners(corners)
                .bg(accent.opacity(0.10))
                .border_2()
                .border_dashed()
                .border_color(accent.opacity(0.8))
                .children(label),
        )
        .into_any_element()
    }
}
