//! Drawing the branch actions: the sync row over the footer (branch,
//! Pull/Fetch, Push, the pull request), the banner of a stopped merge or
//! rebase, the conflict view of a conflicted file, and the Create PR card.

use gpui::{AnyElement, Context, SharedString, div, prelude::*, px, rgb};
use silo_proto::changes::Take;

use super::{
    ChangesPane,
    sync::{PrButton, counts, op_title, pr_button},
    view::Tint,
};
use crate::{
    agent::editor::{Editor, EditorColors},
    surface::Glassy as _,
    theme::{self, Continuous as _},
};

/// A footer button, as Commit's.
pub(super) fn pill(id: impl Into<gpui::ElementId>, label: impl Into<SharedString>, primary: bool, enabled: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .h(px(24.0))
        .px(px(9.0))
        .flex()
        .items_center()
        .gap(px(5.0))
        .continuous(7.0)
        .text_size(px(12.0))
        .font_weight(gpui::FontWeight::MEDIUM)
        .whitespace_nowrap()
        .when(primary, |el| el.bg(rgb(theme::accent())).text_color(rgb(theme::on_accent())).hover(|s| s.opacity(0.88)))
        .when(!primary, |el| el.bg(theme::control_fill()).hover(|s| s.bg(theme::row_fill())))
        .when(enabled, |el| el.cursor_pointer())
        .when(!enabled, |el| el.opacity(0.5))
        .child(label.into())
}

fn muted(text: impl Into<SharedString>) -> gpui::Div {
    div().text_color(rgb(theme::muted())).child(text.into())
}

impl ChangesPane {
    /// Branch · ↑2 ↓1 … [Fetch|Pull ↓1] [Push ↑2] [PR #12 ↗ | Create PR].
    pub(super) fn render_sync_row(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let status = self.status.as_ref().filter(|s| s.repo && self.syncs())?;
        let branch = status.branch.clone()?;
        let free = self.busy.is_none();
        let tracked = status.upstream.is_some();
        let mut row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap(px(6.0))
            .text_size(px(12.0))
            .child(crate::assets::icon("git-branch", 12.0, theme::muted()))
            .child(div().flex_none().max_w(px(220.0)).overflow_hidden().text_ellipsis().whitespace_nowrap().child(branch.clone()))
            .child(muted(counts(status)).flex_none());
        let line = match (&self.sync.progress, self.busy) {
            (Some(p), Some(_)) => Some(p.clone()),
            (None, Some(b)) if ["Pushing…", "Pulling…", "Fetching…", "Opening…"].contains(&b) => Some(b.to_owned()),
            _ => None,
        };
        row = row.child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().children(line.map(muted)));
        if tracked {
            let pull = status.behind > 0;
            let label = if pull { format!("Pull \u{2193}{}", status.behind) } else { "Fetch".to_owned() };
            let tip = if pull { "Pull the new commits from the upstream" } else { "Look for new commits on the remote" };
            row = row.child(
                pill("changes-pull", label, false, free)
                    .tooltip(crate::tooltip::Tooltip::text(tip))
                    .on_click(cx.listener(move |this, _, _, cx| this.pull_now(pull, cx))),
            );
        }
        if status.remote.is_some() && status.head.is_some() {
            let label = if status.ahead > 0 { format!("Push \u{2191}{}", status.ahead) } else { "Push".to_owned() };
            let tip = match (&status.remote, tracked) {
                (Some(remote), false) => format!("Push {branch} to {remote} and track it"),
                _ if status.ahead == 0 => "Nothing to push".to_owned(),
                _ => "Push the commits to the upstream".to_owned(),
            };
            let primary = status.ahead > 0 || !tracked;
            row = row.child(pill("changes-push", label, primary, free && primary).tooltip(crate::tooltip::Tooltip::text(tip)).on_click(cx.listener(
                move |this, _, _, cx| {
                    if primary {
                        this.push_now(cx);
                    }
                },
            )));
        }
        if let Some(button) = self.sync.pr.as_ref().and_then(|pr| pr_button(pr, &branch)) {
            let (label, tip): (SharedString, &str) = match &button {
                PrButton::Open(pr) => (format!("PR #{}", pr.number).into(), "Open the pull request"),
                PrButton::Create => ("Create PR".into(), "Open a pull request with gh"),
                PrButton::SignIn => ("Create PR".into(), "gh isn't signed in"),
            };
            let open = matches!(button, PrButton::Open(_));
            row = row.child(
                pill("changes-pr", label, false, free)
                    .when(open, |el| el.child(crate::assets::icon("arrow-up-right", 10.0, theme::muted())))
                    .tooltip(crate::tooltip::Tooltip::text(tip))
                    .on_click(cx.listener(move |this, _, window, cx| this.pr_action(button.clone(), window, cx))),
            );
        }
        Some(row.into_any_element())
    }

    /// "Merging · 2 conflicted files   [Abort] [Continue]".
    pub(super) fn render_op_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let op = self.status.as_ref().filter(|_| self.syncs()).and_then(|s| s.op)?;
        let left = self.conflicted();
        let free = self.busy.is_none();
        let hint = if left > 0 { "Open a conflicted file to pick a side" } else { "Continue to finish it" };
        Some(
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap(px(8.0))
                .px(px(12.0))
                .py(px(5.0))
                .border_b_1()
                .border_color(rgb(theme::border()))
                .bg(Tint::new(self.surface, theme::warning(), 0.12).paint)
                .text_size(px(12.5))
                .child(div().flex_none().font_weight(gpui::FontWeight::MEDIUM).child(op_title(op, left)))
                .child(muted(hint).flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap())
                .child(pill("changes-abort", "Abort", false, free).on_click(cx.listener(|this, _, _, cx| this.finish_op(true, cx))))
                .child(pill("changes-continue", "Continue", left == 0, free && left == 0).on_click(cx.listener(move |this, _, _, cx| {
                    if left == 0 {
                        this.finish_op(false, cx)
                    }
                })))
                .into_any_element(),
        )
    }
}

/// A side's label as git wrote it, without a path git adds on renames
/// (`HEAD:src/a.rs`).
pub(super) fn side_label(label: &str, side: &str) -> String {
    let label = match label.split_once(':') {
        Some((rev, _)) if !rev.contains(' ') => rev,
        _ => label,
    };
    if label.is_empty() { side.to_owned() } else { format!("{label} \u{b7} {side}") }
}

fn code_lines(lines: &[String], tint: gpui::Hsla) -> impl Iterator<Item = gpui::Div> + '_ {
    let empty = lines.is_empty().then(|| div().px(px(10.0)).text_color(rgb(theme::muted())).child("(nothing)"));
    lines
        .iter()
        .map(move |l| {
            div().px(px(10.0)).bg(tint).whitespace_nowrap().overflow_hidden().child(if l.is_empty() {
                " ".to_owned()
            } else {
                l.replace('\t', "    ")
            })
        })
        .chain(empty)
}

impl ChangesPane {
    /// The active conflicted file: each region's sides, a side to take per
    /// region or for the file, and Mark resolved.
    pub(super) fn render_conflict(&self, path: &str, cx: &mut Context<Self>) -> AnyElement {
        let Some(file) = self.sync.conflict.as_ref().filter(|f| f.path == path) else {
            return div().size_full().flex().items_center().justify_center().child(muted("Loading…")).into_any_element();
        };
        let free = self.busy.is_none();
        let name = crate::files::model::name(path).to_owned();
        let n = file.regions.len();
        let title = match (file.whole, n) {
            (true, _) => format!("{name} \u{b7} take a side"),
            (false, 0) => format!("{name} \u{b7} no conflicts left"),
            (false, 1) => format!("{name} \u{b7} 1 conflict"),
            (false, n) => format!("{name} \u{b7} {n} conflicts"),
        };
        let take_file = |id: &'static str, label: &'static str, take: Take, cx: &mut Context<Self>| {
            let path = path.to_owned();
            pill(id, label, false, free).on_click(cx.listener(move |this, _, _, cx| this.resolve(&path, Some(take), None, cx)))
        };
        let mark = {
            let path = path.to_owned();
            pill("conflict-resolved", "Mark resolved", n == 0 && !file.whole, free)
                .tooltip(crate::tooltip::Tooltip::text("Stage the file as it is now"))
                .on_click(cx.listener(move |this, _, _, cx| this.resolve(&path, None, None, cx)))
        };
        let header = div()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(6.0))
            .px(px(12.0))
            .py(px(6.0))
            .border_b_1()
            .border_color(rgb(theme::border()))
            .text_size(px(12.5))
            .child(div().flex_1().min_w_0().font_weight(gpui::FontWeight::MEDIUM).overflow_hidden().text_ellipsis().whitespace_nowrap().child(title))
            .child(
                take_file("conflict-file-ours", "Take all ours", Take::Ours, cx)
                    .tooltip(crate::tooltip::Tooltip::text("Keep the checked-out side and mark it resolved")),
            )
            .child(
                take_file("conflict-file-theirs", "Take all theirs", Take::Theirs, cx)
                    .tooltip(crate::tooltip::Tooltip::text("Take the incoming side and mark it resolved")),
            )
            .child(mark);
        let tint = |color, t| Tint::new(self.surface, color, t).paint;
        let (ours_tint, theirs_tint, base_tint) = (tint(theme::success(), 0.12), tint(theme::accent(), 0.14), tint(theme::muted(), 0.10));
        let (_, family, _) = theme::summary();
        let cards = file.regions.iter().enumerate().map(|(i, r)| {
            let take = |label: &'static str, take: Take, cx: &mut Context<Self>| {
                let (path, id) = (path.to_owned(), r.id.clone());
                pill(("conflict-take", i * 3 + take as usize), label, false, free)
                    .on_click(cx.listener(move |this, _, _, cx| this.resolve(&path, Some(take), Some(vec![id.clone()]), cx)))
            };
            let section = |text: String| div().px(px(10.0)).pt(px(4.0)).pb(px(2.0)).text_size(px(11.5)).text_color(rgb(theme::muted())).child(text);
            div()
                .flex()
                .flex_col()
                .mx(px(10.0))
                .mt(px(10.0))
                .pb(px(4.0))
                .continuous(8.0)
                .border_1()
                .border_color(rgb(theme::border()))
                .overflow_hidden()
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .px(px(10.0))
                        .py(px(5.0))
                        .bg(theme::control_fill())
                        .text_size(px(12.0))
                        .child(div().flex_1().child(format!("Line {}", r.line)))
                        .child(take("Ours", Take::Ours, cx))
                        .child(take("Theirs", Take::Theirs, cx))
                        .child(take("Both", Take::Both, cx)),
                )
                .child(section(side_label(&r.ours_label, "ours")))
                .child(div().font_family(SharedString::from(family.clone())).children(code_lines(&r.ours, ours_tint)))
                .children(r.base.as_ref().map(|base| {
                    div()
                        .child(section("base".into()))
                        .child(div().font_family(SharedString::from(family.clone())).children(code_lines(base, base_tint)))
                }))
                .child(section(side_label(&r.theirs_label, "theirs")))
                .child(div().font_family(SharedString::from(family.clone())).children(code_lines(&r.theirs, theirs_tint)))
        });
        let note = match (file.whole, n) {
            (true, _) => Some("This file can't be merged line by line: take a whole side."),
            (false, 0) => Some("Every conflict has a side now. Mark the file resolved to stage it."),
            _ => None,
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .child(header)
            .child(
                div()
                    .id("changes-conflict")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .pb(px(10.0))
                    .children(note.map(|t| muted(t).p(px(16.0)).text_size(px(12.5))))
                    .children(cards),
            )
            .into_any_element()
    }
}

impl ChangesPane {
    /// The Create PR card: base, title, description, Draft.
    pub(super) fn render_pr_form(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let _glass = crate::surface::enter();
        let form = self.sync.form.as_ref()?;
        let bases = self.sync.pr.as_ref().map(|p| p.bases.clone()).unwrap_or_default();
        let base = form.base.clone().unwrap_or_else(|| "the default branch".into());
        let colors = EditorColors {
            text: rgb(theme::foreground()).into(),
            placeholder: rgb(theme::muted()).into(),
            caret: rgb(theme::foreground()).into(),
            selection: {
                let mut h: gpui::Hsla = rgb(theme::selection()).into();
                h.a = 0.6;
                h
            },
        };
        let well = || div().px(px(8.0)).continuous(6.0).bg(theme::well_fill());
        let list = form.bases_open.then(|| {
            div().id("pr-bases").flex().flex_col().max_h(px(160.0)).overflow_y_scroll().children(bases.iter().enumerate().map(|(i, b)| {
                let on = form.base.as_deref() == Some(b.as_str());
                let pick = b.clone();
                div()
                    .id(("pr-base", i))
                    .h(px(24.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .continuous(6.0)
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::hover_fill()))
                    .child(div().w(px(12.0)).flex_none().children(on.then(|| crate::assets::icon("check", 11.0, theme::accent()))))
                    .child(b.clone())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if let Some(f) = this.sync.form.as_mut() {
                            (f.base, f.bases_open) = (Some(pick.clone()), false);
                        }
                        cx.notify();
                    }))
            }))
        });
        let draft = form.draft;
        let free = self.busy.is_none();
        Some(
            div()
                .id("changes-pr-form")
                .absolute()
                .bottom(px(56.0))
                .right(px(10.0))
                .w(px(420.0))
                .flex()
                .flex_col()
                .gap(px(6.0))
                .p(px(10.0))
                .glass(10.0)
                .text_size(px(12.5))
                .on_mouse_down(gpui::MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(div().flex().items_center().gap(px(6.0)).child(div().font_weight(gpui::FontWeight::MEDIUM).child("Pull request into")).child(
                    pill("pr-base-button", base, false, bases.len() > 1).child(crate::assets::icon("chevron-down", 10.0, theme::muted())).on_click(
                        cx.listener(|this, _, _, cx| {
                            if let Some(f) = this.sync.form.as_mut() {
                                f.bases_open = !f.bases_open;
                            }
                            cx.notify();
                        }),
                    ),
                ))
                .children(list)
                .child(well().h(px(28.0)).flex().items_center().child(form.title.clone()))
                .child(well().py(px(5.0)).min_h(px(90.0)).child(Editor::element(&form.body, 8, colors, cx)))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            pill("pr-draft", "Draft", false, true)
                                .children(draft.then(|| crate::assets::icon("check", 10.0, theme::accent())))
                                .tooltip(crate::tooltip::Tooltip::text("Open it as a draft"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    if let Some(f) = this.sync.form.as_mut() {
                                        f.draft = !f.draft;
                                    }
                                    cx.notify();
                                })),
                        )
                        .child(div().flex_1())
                        .child(pill("pr-cancel", "Cancel", false, true).on_click(cx.listener(|this, _, window, cx| this.close_pr_form(window, cx))))
                        .child(
                            pill("pr-create", self.busy.unwrap_or("Create PR"), true, free)
                                .tooltip(crate::tooltip::Tooltip::text("Pushes the branch first when needed"))
                                .on_click(cx.listener(|this, _, _, cx| this.create_pr(cx))),
                        ),
                )
                .into_any_element(),
        )
    }
}
