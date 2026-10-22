//! Sub-agents (`docs/AGENT_BLOCK.md` §4f): the bar listing the shown
//! session's children, opening one in its own agent block, and the strip
//! that marks a pane showing a child, with the way back to its parent.

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use gpui::{AnyElement, ClickEvent, Context, Task, div, prelude::*, px, rgb};
use otto_proto::{Event, ServerMessage, SessionInfo, SessionStatus};

use super::{
    AgentPane, AgentPaneEvent,
    view::{BAR_ROW, Bar, ink},
};
use crate::{assets::icon, theme, tooltip::Tooltip};

/// A child that finished stays in the bar this long, noted `Returned`.
const RETURNED: Duration = Duration::from_secs(8);
/// `sessions.changed` bursts (a turn starting, approvals) refresh the list once.
const SETTLE: Duration = Duration::from_millis(150);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SubState {
    Running,
    /// Waiting for an approval or an answer.
    Waiting,
    /// The user stopped it: its report is held back from the parent.
    Stopped,
    Returned,
}

impl SubState {
    fn note(self) -> &'static str {
        match self {
            Self::Running => "Running",
            Self::Waiting => "Needs approval",
            Self::Stopped => "Stopped",
            Self::Returned => "Returned",
        }
    }

    fn active(self) -> bool {
        matches!(self, Self::Running | Self::Waiting)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct SubRow {
    pub id: String,
    pub agent: String,
    pub title: String,
    pub state: SubState,
}

/// The children this pane saw running, and when those that finished returned.
#[derive(Default)]
pub(super) struct Subs {
    running: HashSet<String>,
    returned: HashMap<String, Instant>,
    /// Listens for otto's `sessions.changed`.
    watch: Option<Task<()>>,
    /// A coalesced session-list refresh is due.
    refreshing: bool,
}

/// Running, waiting, or holding its report: it has not returned yet.
fn out(s: &SessionInfo) -> bool {
    s.report_held || matches!(s.status, SessionStatus::Busy | SessionStatus::WaitingApproval)
}

impl Subs {
    /// Note the children of `parent` in a fresh session list; `true` when one
    /// just returned (its report went to the parent).
    pub(super) fn track(&mut self, sessions: &[SessionInfo], parent: &str, now: Instant) -> bool {
        let mut returned = false;
        for s in sessions.iter().filter(|s| s.parent_id.as_deref() == Some(parent)) {
            if out(s) {
                self.running.insert(s.id.clone());
                self.returned.remove(&s.id);
            } else if self.running.remove(&s.id) {
                self.returned.insert(s.id.clone(), now);
                returned = true;
            }
        }
        self.returned.retain(|_, at| now.duration_since(*at) < RETURNED);
        returned
    }

    /// Another session is shown: nothing of the last one carries over.
    pub(super) fn reset(&mut self) {
        self.running.clear();
        self.returned.clear();
    }

    /// The bar's rows: children running, waiting or stopped, then those just returned, oldest first.
    pub(super) fn rows(&self, sessions: &[SessionInfo], parent: &str, now: Instant) -> Vec<SubRow> {
        let mut rows: Vec<(&SessionInfo, SubState)> = sessions
            .iter()
            .filter(|s| s.parent_id.as_deref() == Some(parent))
            .filter_map(|s| {
                let state = match s.status {
                    SessionStatus::WaitingApproval => SubState::Waiting,
                    SessionStatus::Busy => SubState::Running,
                    _ if s.report_held => SubState::Stopped,
                    _ if self.returned.get(&s.id).is_some_and(|at| now.duration_since(*at) < RETURNED) => SubState::Returned,
                    _ => return None,
                };
                Some((s, state))
            })
            .collect();
        rows.sort_by_key(|(s, state)| (*state == SubState::Returned, s.created_at));
        rows.into_iter().map(|(s, state)| SubRow { id: s.id.clone(), agent: s.agent.clone(), title: s.title.clone(), state }).collect()
    }
}

impl AgentPane {
    pub(super) fn subagent_rows(&self) -> Vec<SubRow> {
        let Some(id) = self.session_id.as_deref() else { return Vec::new() };
        self.subs.rows(&self.sessions, id, Instant::now())
    }

    /// The shown session's parent, when it is a sub-agent.
    fn parent_session(&self) -> Option<&str> {
        self.mirror().and_then(|m| m.info.parent_id.as_deref()).or_else(|| {
            let id = self.session_id.as_deref()?;
            self.sessions.iter().find(|s| s.id == id).and_then(|s| s.parent_id.as_deref())
        })
    }

    /// The pane shows a sub-agent: the header drops History / New Session.
    pub fn is_subagent(&self) -> bool {
        self.parent_session().is_some()
    }

    /// A fresh session list arrived: a child that just returned stays a while, then leaves.
    pub(super) fn track_subagents(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.session_id.clone() else { return };
        if self.subs.track(&self.sessions, &id, Instant::now()) {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(RETURNED).await;
                let _ = this.update(cx, |this, cx| {
                    this.track_subagents(cx);
                    cx.notify();
                });
            })
            .detach();
        }
    }

    pub(super) fn reset_subagents(&mut self) {
        self.subs.reset();
    }

    /// Refresh the session list when otto says sessions changed (children
    /// start, wait, finish), while sub-agents matter to this pane.
    pub(super) fn watch_sessions(&mut self, cx: &mut Context<Self>) {
        if self.subs.watch.is_some() {
            return;
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        self.rt.spawn(async move {
            loop {
                if let Ok(connection) = super::client::connection().await {
                    let mut events = connection.events();
                    loop {
                        let changed = match events.recv().await {
                            Ok(Ok(ServerMessage::Event { session_id: None, event, .. })) => matches!(*event, Event::SessionsChanged {}),
                            Ok(Ok(_)) => false,
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => true,
                            Ok(Err(_)) | Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        };
                        if changed && tx.send(()).is_err() {
                            return;
                        }
                    }
                }
                if tx.is_closed() {
                    return;
                }
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
        self.subs.watch = Some(cx.spawn(async move |this, cx| {
            while rx.recv().await.is_some() {
                if this.update(cx, |this, cx| this.sessions_changed(cx)).is_err() {
                    return;
                }
            }
        }));
    }

    fn sessions_changed(&mut self, cx: &mut Context<Self>) {
        let shown = self.session_id.as_deref();
        let relevant = self.subagent_calls > 0
            || self.parent_session().is_some()
            || shown.is_some_and(|id| self.sessions.iter().any(|s| s.parent_id.as_deref() == Some(id)));
        if !relevant || self.subs.refreshing {
            return;
        }
        self.subs.refreshing = true;
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SETTLE).await;
            let _ = this.update(cx, |this, cx| {
                this.subs.refreshing = false;
                this.refresh_sessions(cx);
            });
        })
        .detach();
    }

    /// Show `session_id` in its own agent block (the one showing it, else a new one beside this).
    pub(super) fn open_session_block(&mut self, session_id: String, cx: &mut Context<Self>) {
        let title = self.sessions.iter().find(|s| s.id == session_id).map(|s| s.title.clone()).unwrap_or_default();
        cx.emit(AgentPaneEvent::OpenSession { session_id, title });
    }

    /// The Sub-agents bar: `bot · N running`, a row per child; a row opens the child.
    pub(super) fn subagent_bar(&mut self, cx: &mut Context<Self>) -> Option<Bar> {
        let rows = self.subagent_rows();
        let first = rows.first()?.title.clone();
        let running = rows.iter().filter(|r| r.state.active()).count();
        let stopped = rows.iter().filter(|r| r.state == SubState::Stopped).count();
        let note = match (running, stopped) {
            (0, 0) => "Returned".into(),
            (0, n) => format!("{n} stopped"),
            (n, _) => format!("{n} running"),
        };
        let summary = (!first.is_empty()).then(|| {
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_color(rgb(theme::foreground()))
                .child(first)
                .into_any_element()
        });
        Some(Bar {
            key: "subagents",
            icon: icon("bot", 14.0, theme::accent()).into_any_element(),
            label: "Sub-agents",
            summary,
            note: note.clone(),
            open_label: "Sub-agents".into(),
            open_note: note,
            rows: rows.into_iter().enumerate().map(|(i, row)| self.subagent_row(i, row, cx)).collect(),
        })
    }

    fn subagent_row(&self, i: usize, row: SubRow, cx: &mut Context<Self>) -> AnyElement {
        let muted = theme::muted();
        let fit = self.fit();
        let group = gpui::SharedString::from(format!("subagent-{i}"));
        let glyph = match row.state {
            SubState::Waiting => icon("lock", 13.0, theme::warning()).into_any_element(),
            SubState::Running => crate::browser_pane::spinner(&format!("sub-{i}"), 12.0, muted, cx),
            SubState::Stopped => icon("stop", 11.0, muted).into_any_element(),
            SubState::Returned => icon("circle-check", 13.0, muted).into_any_element(),
        };
        let id = row.id.clone();
        div()
            .id(gpui::ElementId::Name(group.clone()))
            .group(group.clone())
            .h(px(BAR_ROW))
            .px(px(14.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .cursor_pointer()
            .hover(|s| s.bg(theme::hover_fill()))
            .tooltip(Tooltip::text("Open sub-agent"))
            .child(div().w(px(14.0)).flex_none().flex().justify_center().child(glyph))
            .when(fit.detail, |el| el.child(div().flex_none().text_color(rgb(muted)).child(row.agent.to_uppercase())))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(rgb(if row.state.active() { theme::foreground() } else { muted }))
                    .child(if row.title.is_empty() { "Working".into() } else { row.title }),
            )
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_size(px(11.5))
                    .text_color(rgb(if row.state == SubState::Waiting { theme::warning() } else { muted }))
                    .child(row.state.note()),
            )
            .when(fit.labels, |el| {
                el.child(div().flex_none().opacity(0.0).group_hover(group, |s| s.opacity(1.0)).child(icon("arrow-up-right", 12.0, muted)))
            })
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.open_session_block(id.clone(), cx)))
            .into_any_element()
    }

    /// A pane showing a sub-agent: `bot Sub-agent · parent title`, and the way back to the parent.
    pub(super) fn render_subagent_strip(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let parent = self.parent_session()?.to_owned();
        let muted = theme::muted();
        // Narrow: the parent's title goes, then the labels (icons and tooltips stay).
        let fit = self.fit();
        let title = self.sessions.iter().find(|s| s.id == parent).map(|s| s.title.clone()).filter(|t| fit.detail && !t.trim().is_empty());
        Some(
            div()
                .flex_none()
                .h(px(32.0))
                .px(px(if fit.labels { 14.0 } else { 8.0 }))
                .flex()
                .items_center()
                .gap(px(8.0))
                .overflow_hidden()
                .border_b_1()
                .border_color(rgb(ink(0.1)))
                .text_size(px(12.0))
                .when(fit.send, |el| {
                    el.child(div().flex_none().flex().child(icon("bot", 13.0, theme::accent()))).child(
                        div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(rgb(theme::foreground())).child("Sub-agent"),
                    )
                })
                .children(
                    title.map(|t| {
                        div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().text_color(rgb(muted)).child(format!("of {t}"))
                    }),
                )
                .child(div().flex_1())
                .child(
                    div()
                        .id("subagent-parent")
                        .flex_none()
                        .h(px(24.0))
                        .px(px(if fit.labels { 8.0 } else { 6.0 }))
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .rounded(px(6.0))
                        .text_color(rgb(muted))
                        .cursor_pointer()
                        .hover(|s| s.bg(theme::hover_fill()).text_color(rgb(theme::foreground())))
                        .active(|s| s.scale(0.96))
                        .tooltip(Tooltip::text("Show the main agent"))
                        .child(icon("back", 12.0, muted))
                        .when(fit.labels, |el| el.child("Main agent"))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.open_session_block(parent.clone(), cx))),
                )
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, parent: Option<&str>, status: SessionStatus, created_at: i64) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            project_id: "proj".into(),
            parent_id: parent.map(Into::into),
            agent: "general".into(),
            approval_mode: otto_proto::ApprovalMode::default(),
            provider: "test".into(),
            model: "test".into(),
            effort: None,
            title: format!("task {id}"),
            cwd: String::new(),
            status,
            created_at,
            updated_at: created_at,
            last_seq: 0,
            usage: otto_proto::UsageTotals::default(),
            report_held: false,
        }
    }

    #[test]
    fn bar_rows_show_running_waiting_then_returned_for_a_while() {
        let now = Instant::now();
        let mut subs = Subs::default();
        let mut list = vec![
            session("a", Some("p"), SessionStatus::Busy, 2),
            session("b", Some("p"), SessionStatus::WaitingApproval, 3),
            session("c", Some("p"), SessionStatus::Idle, 1),
            session("x", Some("other"), SessionStatus::Busy, 0),
            session("p", None, SessionStatus::Busy, 0),
        ];
        assert!(!subs.track(&list, "p", now));
        let states = |subs: &Subs, list: &[SessionInfo], at| subs.rows(list, "p", at).into_iter().map(|r| (r.id, r.state)).collect::<Vec<_>>();
        // An idle child never seen running is not listed.
        assert_eq!(states(&subs, &list, now), [("a".into(), SubState::Running), ("b".into(), SubState::Waiting)]);
        list[0].status = SessionStatus::Idle;
        assert!(subs.track(&list, "p", now));
        assert_eq!(states(&subs, &list, now), [("b".into(), SubState::Waiting), ("a".into(), SubState::Returned)]);
        let later = now + RETURNED;
        assert_eq!(states(&subs, &list, later), [("b".into(), SubState::Waiting)]);
        // Started again (the user took it over): running once more.
        list[0].status = SessionStatus::Busy;
        assert!(!subs.track(&list, "p", now));
        assert_eq!(states(&subs, &list, later)[0], ("a".into(), SubState::Running));
        subs.reset();
        list[0].status = SessionStatus::Idle;
        assert!(!subs.track(&list, "p", now), "a reset forgets what ran");
    }

    #[test]
    fn a_child_the_user_stopped_reads_stopped_until_its_report_goes() {
        let now = Instant::now();
        let mut subs = Subs::default();
        let mut list = vec![session("a", Some("p"), SessionStatus::Busy, 1)];
        subs.track(&list, "p", now);
        let state = |subs: &Subs, list: &[SessionInfo], at| subs.rows(list, "p", at).first().map(|r| r.state);
        // Stopped: idle, holding its report.
        (list[0].status, list[0].report_held) = (SessionStatus::Idle, true);
        assert!(!subs.track(&list, "p", now));
        assert_eq!(state(&subs, &list, now + RETURNED * 4), Some(SubState::Stopped), "stays while held");
        // The report went to the parent: returned, for a while.
        let sent = now + RETURNED * 5;
        list[0].report_held = false;
        assert!(subs.track(&list, "p", sent));
        assert_eq!(state(&subs, &list, sent), Some(SubState::Returned));
        assert_eq!(state(&subs, &list, sent + RETURNED), None);
        // Held when the pane first sees it (a reload): stopped too.
        let mut fresh = Subs::default();
        list[0].report_held = true;
        fresh.track(&list, "p", now);
        assert_eq!(state(&fresh, &list, now), Some(SubState::Stopped));
    }
}
