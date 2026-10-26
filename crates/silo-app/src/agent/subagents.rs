//! Sub-agents (`docs/AGENT_BLOCK.md` §4f): the bar listing the shown
//! session's children, opening one in its own agent block, and the strip
//! that marks a pane showing a child, with the way back to its parent and,
//! while the user holds its report, the hand-back.

use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

use gpui::{AnyElement, ClickEvent, Context, Task, div, prelude::*, px, rgb};
use otto_proto::{Event, PendingApproval, PendingQuestion, ServerMessage, SessionInfo, SessionStatus};

use super::{
    AgentPane, AgentPaneEvent,
    view::{BAR_ROW, Bar, eased, ink},
};
use crate::{assets::icon, theme, tooltip::Tooltip};

/// A returned row fades out (opacity, height) over this long once the
/// parent's turn is over.
const LEAVE: Duration = crate::motion::UI;
/// `sessions.changed` bursts (a turn starting, approvals) refresh the list once.
const SETTLE: Duration = Duration::from_millis(150);
/// Room a stopped row's note keeps for its `Hand back` hover action.
const HAND_BACK: f32 = 72.0;

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
            Self::Waiting => "Needs you",
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
    /// A returned row fading out since.
    pub leaving: Option<Instant>,
}

/// A child that returned: it stays while the parent's turn runs.
#[derive(Clone, Copy, Debug, Default)]
struct Back {
    /// The parent ran a turn since (the one running, or the one its report starts).
    turn: bool,
    leaving: Option<Instant>,
}

/// The children this pane saw out (running, waiting or held), and those that returned.
#[derive(Default)]
pub(super) struct Subs {
    running: HashSet<String>,
    returned: HashMap<String, Back>,
    /// The shown (parent) session runs a turn.
    parent_running: bool,
    /// Listens for otto's `sessions.changed`.
    watch: Option<Task<()>>,
    /// A coalesced session-list refresh is due.
    refreshing: bool,
    /// What the waiting children ask, oldest child first.
    pub(super) waits: Vec<ChildWait>,
}

/// A waiting child's pending questions and approvals: their cards show in the parent's pane.
#[derive(Clone, Debug)]
pub(super) struct ChildWait {
    pub id: String,
    pub title: String,
    pub questions: Vec<PendingQuestion>,
    pub approvals: Vec<PendingApproval>,
}

/// Running, waiting, or holding its report: it has not returned yet.
fn out(s: &SessionInfo) -> bool {
    s.report_held || matches!(s.status, SessionStatus::Busy | SessionStatus::WaitingApproval)
}

/// `s` is a sub-agent of `parent`. A fork names its source as `parent_id` too, but is a session of its own
/// (`forked_from`): never a child.
pub(super) fn child_of(s: &SessionInfo, parent: &str) -> bool {
    s.is_subagent() && s.parent_id.as_deref() == Some(parent)
}

/// The parent of `s` when it is a sub-agent (a fork has none).
pub(super) fn parent_of(s: &SessionInfo) -> Option<&str> {
    s.parent_id.as_deref().filter(|_| s.is_subagent())
}

impl Subs {
    /// Note the children of `parent` in a fresh session list; `true` when one
    /// just returned (its report went to the parent).
    pub(super) fn track(&mut self, sessions: &[SessionInfo], parent: &str, now: Instant) -> bool {
        let mut returned = false;
        for s in sessions.iter().filter(|s| child_of(s, parent)) {
            if out(s) {
                self.running.insert(s.id.clone());
                self.returned.remove(&s.id);
            } else if self.running.remove(&s.id) {
                self.returned.insert(s.id.clone(), Back { turn: self.parent_running, leaving: None });
                returned = true;
            }
        }
        self.returned.retain(|_, b| b.leaving.is_none_or(|at| now.duration_since(at) < LEAVE));
        returned
    }

    /// The parent's turn started or ended. A returned row leaves when the
    /// turn it returned during ends; returned while the parent was idle, when
    /// the turn its report starts ends.
    pub(super) fn turn(&mut self, running: bool, now: Instant) {
        if running == self.parent_running {
            return;
        }
        self.parent_running = running;
        for back in self.returned.values_mut() {
            if running {
                back.turn = true;
            } else if back.turn && back.leaving.is_none() {
                back.leaving = Some(now);
            }
        }
    }

    /// Another session is shown: nothing of the last one carries over.
    pub(super) fn reset(&mut self) {
        self.running.clear();
        self.returned.clear();
        self.waits.clear();
        self.parent_running = false;
    }

    /// The bar's rows: children running, waiting or stopped, then those returned (until they have left), oldest first.
    pub(super) fn rows(&self, sessions: &[SessionInfo], parent: &str, now: Instant) -> Vec<SubRow> {
        let mut rows: Vec<(&SessionInfo, SubState, Option<Instant>)> = sessions
            .iter()
            .filter(|s| child_of(s, parent))
            .filter_map(|s| {
                let back = self.returned.get(&s.id).filter(|b| b.leaving.is_none_or(|at| now.duration_since(at) < LEAVE));
                let (state, leaving) = match s.status {
                    SessionStatus::WaitingApproval => (SubState::Waiting, None),
                    SessionStatus::Busy => (SubState::Running, None),
                    _ if s.report_held => (SubState::Stopped, None),
                    _ if back.is_some() => (SubState::Returned, back.and_then(|b| b.leaving)),
                    _ => return None,
                };
                Some((s, state, leaving))
            })
            .collect();
        rows.sort_by_key(|(s, state, _)| (*state == SubState::Returned, s.created_at));
        rows.into_iter()
            .map(|(s, state, leaving)| SubRow { id: s.id.clone(), agent: s.agent.clone(), title: s.title.clone(), state, leaving })
            .collect()
    }
}

impl AgentPane {
    pub(super) fn subagent_rows(&self) -> Vec<SubRow> {
        let Some(id) = self.session_id.as_deref() else { return Vec::new() };
        self.subs.rows(&self.sessions, id, Instant::now())
    }

    /// The shown session's parent, when it is a sub-agent (not a fork).
    fn parent_session(&self) -> Option<&str> {
        self.mirror().and_then(|m| parent_of(&m.info)).or_else(|| {
            let id = self.session_id.as_deref()?;
            self.sessions.iter().find(|s| s.id == id).and_then(parent_of)
        })
    }

    /// The pane shows a sub-agent: the header drops History / New Session.
    pub fn is_subagent(&self) -> bool {
        self.parent_session().is_some()
    }

    /// A fresh session list arrived: note children that started or returned,
    /// and fetch what the waiting ones ask (their cards show in this pane).
    pub(super) fn track_subagents(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.session_id.clone() else { return };
        let now = Instant::now();
        self.subs.track(&self.sessions, &id, now);
        let waiting: Vec<(String, String)> =
            self.subs.rows(&self.sessions, &id, now).into_iter().filter(|r| r.state == SubState::Waiting).map(|r| (r.id, r.title)).collect();
        if waiting.is_empty() {
            self.subs.waits.clear();
            return;
        }
        self.run(
            cx,
            move |c| async move {
                let mut waits = Vec::new();
                for (id, title) in waiting {
                    let view = c.session_view(&id).await?;
                    waits.push(ChildWait { id, title, questions: view.pending_questions, approvals: view.pending_approvals });
                }
                Ok(waits)
            },
            move |this, result, _| {
                if let Ok(waits) = result
                    && this.session_id.as_deref() == Some(id.as_str())
                {
                    this.subs.waits = waits;
                }
            },
        );
    }

    /// The pending question or approval to show: the shown session's own, else a waiting child's (oldest child
    /// first), with the session it belongs to and, for a child, its title.
    pub(super) fn live_card(&self) -> Option<(String, Option<String>, super::Card)> {
        use super::Card;
        let own = self.mirror().and_then(|m| {
            m.pending_questions
                .first()
                .cloned()
                .map(Card::Question)
                .or_else(|| m.pending_approvals.first().cloned().map(|a| Card::Approval(a, m.pending_approvals.len() - 1)))
        });
        if let Some(card) = own {
            return Some((self.session_id.clone()?, None, card));
        }
        self.subs.waits.iter().find_map(|w| {
            let card = w.questions.first().cloned().map(Card::Question);
            let card = card.or_else(|| w.approvals.first().cloned().map(|a| Card::Approval(a, w.approvals.len() - 1)))?;
            Some((w.id.clone(), Some(if w.title.is_empty() { "Sub-agent".into() } else { w.title.clone() }), card))
        })
    }

    pub(super) fn reset_subagents(&mut self) {
        self.subs.reset();
    }

    /// Send stopped sub-agent `session_id`'s held report to its parent now
    /// (`session.hand_back`). Its row reads `Returned` at once; otto's
    /// `sessions.changed` confirms it.
    pub(super) fn hand_back(&mut self, session_id: String, cx: &mut Context<Self>) {
        if let Some(s) = self.sessions.iter_mut().find(|s| s.id == session_id) {
            s.report_held = false;
        }
        self.track_subagents(cx);
        self.run(
            cx,
            move |c| async move { c.hand_back(&session_id).await },
            |this, result, cx| {
                if let Err(error) = result {
                    this.notice = Some(error);
                }
                this.refresh_sessions(cx);
            },
        );
        cx.notify();
    }

    /// The shown sub-agent holds its report (the user stopped it): `Hand back` shows.
    fn holds_report(&self) -> bool {
        self.mirror().is_some_and(|m| m.info.report_held) && !self.running
    }

    /// Refresh the session list when otto says sessions changed (children
    /// start, wait, finish), while sub-agents matter to this pane.
    pub(super) fn watch_sessions(&mut self, cx: &mut Context<Self>) {
        if self.subs.watch.is_some() {
            return;
        }
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let target = self.target.clone();
        self.rt.spawn(async move {
            loop {
                if let Ok(connection) = super::client::connection(&target).await {
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
        let relevant =
            self.subagent_calls > 0 || self.parent_session().is_some() || shown.is_some_and(|id| self.sessions.iter().any(|s| child_of(s, id)));
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

    /// The Sub-agents bar: `bot · N running` (`Needs you` while a child waits), a row per child; a row opens the child.
    pub(super) fn subagent_bar(&mut self, cx: &mut Context<Self>) -> Option<Bar> {
        let rows = self.subagent_rows();
        let first = rows.first()?.title.clone();
        let running = rows.iter().filter(|r| r.state.active()).count();
        let stopped = rows.iter().filter(|r| r.state == SubState::Stopped).count();
        let waiting = rows.iter().any(|r| r.state == SubState::Waiting);
        let note = match (running, stopped) {
            _ if waiting => SubState::Waiting.note().into(),
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
            // A question, else an approval (lock).
            SubState::Waiting => {
                let asks = self.subs.waits.iter().any(|w| w.id == row.id && !w.questions.is_empty());
                icon(if asks { "question" } else { "lock" }, 13.0, theme::warning()).into_any_element()
            }
            SubState::Running => crate::browser_pane::spinner(&format!("sub-{i}"), 12.0, muted, cx),
            SubState::Stopped => icon("stop", 11.0, muted).into_any_element(),
            SubState::Returned => icon("circle-check", 13.0, muted).into_any_element(),
        };
        // Leaving: the row fades and closes up.
        let keep = row.leaving.map_or(1.0, |at| 1.0 - eased(at, LEAVE, Instant::now(), cx).unwrap_or(1.0));
        let id = row.id.clone();
        let note = div()
            .min_w_0()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .text_size(px(11.5))
            .text_color(rgb(if row.state == SubState::Waiting { theme::warning() } else { muted }))
            .child(row.state.note());
        // Stopped: on hover the note gives way to `Hand back` (no layout shift: the note keeps its width).
        let note = if row.state == SubState::Stopped {
            let back = row.id.clone();
            div()
                .relative()
                .flex_none()
                .min_w(px(HAND_BACK))
                .h_full()
                .flex()
                .items_center()
                .justify_end()
                .child(note.group_hover(group.clone(), |s| s.opacity(0.0)))
                .child(
                    div()
                        .id(gpui::ElementId::Name(format!("subagent-hand-back-{i}").into()))
                        .absolute()
                        .right(px(-6.0))
                        .top(px((BAR_ROW - 22.0) / 2.0))
                        .h(px(22.0))
                        .px(px(6.0))
                        .flex()
                        .items_center()
                        .rounded(px(6.0))
                        .whitespace_nowrap()
                        .text_size(px(11.5))
                        .text_color(rgb(muted))
                        .opacity(0.0)
                        .group_hover(group.clone(), |s| s.opacity(1.0))
                        .cursor_pointer()
                        .hover(|s| s.bg(theme::hover_fill()).text_color(rgb(theme::foreground())))
                        .active(|s| s.scale(0.96))
                        .tooltip(Tooltip::text("Send its report to the main agent"))
                        .child("Hand back")
                        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                            cx.stop_propagation();
                            this.hand_back(back.clone(), cx);
                        })),
                )
                .into_any_element()
        } else {
            note.into_any_element()
        };
        div()
            .id(gpui::ElementId::Name(group.clone()))
            .group(group.clone())
            .h(px(BAR_ROW * keep))
            .opacity(keep)
            .overflow_hidden()
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
            .child(note)
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
                .when(self.holds_report(), |el| {
                    let id = self.session_id.clone().unwrap_or_default();
                    el.child(
                        div()
                            .id("subagent-hand-back")
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
                            .tooltip(Tooltip::text("Send its report to the main agent"))
                            .child(icon("arrow-up", 12.0, muted))
                            .when(fit.labels, |el| el.child("Hand back"))
                            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.hand_back(id.clone(), cx))),
                    )
                })
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
            forked_from: None,
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
            context: otto_proto::ContextUse::default(),
            report_held: false,
            plan: None,
        }
    }

    #[test]
    fn a_fork_is_never_a_sub_agent() {
        let now = Instant::now();
        let mut fork = session("f", Some("p"), SessionStatus::Busy, 2);
        fork.forked_from = Some(otto_proto::ForkOrigin { session_id: "p".into(), at_seq: 7 });
        let list = vec![session("p", None, SessionStatus::Idle, 0), session("a", Some("p"), SessionStatus::Busy, 1), fork.clone()];
        // Not a child of its source: the sub-agent strip and its tracking skip it.
        assert!(child_of(&list[1], "p") && !child_of(&fork, "p"));
        let mut subs = Subs::default();
        subs.track(&list, "p", now);
        assert_eq!(subs.rows(&list, "p", now).into_iter().map(|r| r.id).collect::<Vec<_>>(), ["a"]);
        // No parent to lead back to (the header keeps History / New Session).
        assert_eq!(parent_of(&list[1]), Some("p"));
        assert_eq!(parent_of(&fork), None);
        // Listed in the switcher with the top-level sessions; the sub-agent isn't.
        let listed: Vec<&str> = super::super::switcher_sessions(&list, "").into_iter().map(|s| s.id.as_str()).collect();
        assert_eq!(listed, ["f", "p"]);
        // From an older otto (no `forked_from`) a fork still reads as a sub-agent.
        let mut old = fork;
        old.forked_from = None;
        assert!(child_of(&old, "p"));
    }

    #[test]
    fn bar_rows_show_running_waiting_then_returned_until_the_parent_turn_ends() {
        let now = Instant::now();
        let mut subs = Subs::default();
        let mut list = vec![
            session("a", Some("p"), SessionStatus::Busy, 2),
            session("b", Some("p"), SessionStatus::WaitingApproval, 3),
            session("c", Some("p"), SessionStatus::Idle, 1),
            session("x", Some("other"), SessionStatus::Busy, 0),
            session("p", None, SessionStatus::Busy, 0),
        ];
        subs.turn(true, now);
        assert!(!subs.track(&list, "p", now));
        let states = |subs: &Subs, list: &[SessionInfo], at| subs.rows(list, "p", at).into_iter().map(|r| (r.id, r.state)).collect::<Vec<_>>();
        // An idle child never seen running is not listed.
        assert_eq!(states(&subs, &list, now), [("a".into(), SubState::Running), ("b".into(), SubState::Waiting)]);
        list[0].status = SessionStatus::Idle;
        assert!(subs.track(&list, "p", now));
        assert_eq!(states(&subs, &list, now), [("b".into(), SubState::Waiting), ("a".into(), SubState::Returned)]);
        // No timer: it stays while the parent's turn runs.
        let later = now + Duration::from_secs(600);
        subs.track(&list, "p", later);
        assert_eq!(states(&subs, &list, later)[1], ("a".into(), SubState::Returned));
        // Started again (the user took it over): running once more.
        list[0].status = SessionStatus::Busy;
        assert!(!subs.track(&list, "p", later));
        assert_eq!(states(&subs, &list, later)[0], ("a".into(), SubState::Running));
        subs.reset();
        list[0].status = SessionStatus::Idle;
        assert!(!subs.track(&list, "p", later), "a reset forgets what ran");
    }

    #[test]
    fn a_returned_row_fades_out_when_the_parent_turn_ends() {
        let now = Instant::now();
        let mut subs = Subs::default();
        let mut list = vec![session("a", Some("p"), SessionStatus::Busy, 1)];
        let rows = |subs: &Subs, list: &[SessionInfo], at| subs.rows(list, "p", at);
        subs.turn(true, now);
        subs.track(&list, "p", now);
        list[0].status = SessionStatus::Idle;
        subs.track(&list, "p", now);
        let end = now + Duration::from_secs(30);
        subs.turn(false, end);
        let row = rows(&subs, &list, end).pop().unwrap();
        assert_eq!((row.state, row.leaving), (SubState::Returned, Some(end)), "leaving with the turn's end");
        assert!(rows(&subs, &list, end + LEAVE).is_empty(), "gone once faded");
        subs.track(&list, "p", end + LEAVE);
        assert!(rows(&subs, &list, end).is_empty(), "and forgotten");
    }

    #[test]
    fn a_report_to_an_idle_parent_stays_through_the_turn_it_starts() {
        let now = Instant::now();
        let mut subs = Subs::default();
        let mut list = vec![session("a", Some("p"), SessionStatus::Busy, 1)];
        let state = |subs: &Subs, list: &[SessionInfo], at| subs.rows(list, "p", at).first().map(|r| (r.state, r.leaving));
        subs.track(&list, "p", now);
        list[0].status = SessionStatus::Idle;
        subs.track(&list, "p", now);
        // The parent is idle: the report wakes it.
        subs.turn(false, now);
        assert_eq!(state(&subs, &list, now + LEAVE * 4), Some((SubState::Returned, None)));
        let woke = now + Duration::from_secs(1);
        subs.turn(true, woke);
        assert_eq!(state(&subs, &list, woke), Some((SubState::Returned, None)));
        let end = woke + Duration::from_secs(20);
        subs.turn(false, end);
        assert_eq!(state(&subs, &list, end), Some((SubState::Returned, Some(end))));
        assert_eq!(state(&subs, &list, end + LEAVE), None);
    }

    #[test]
    fn a_child_the_user_stopped_reads_stopped_until_it_is_handed_back() {
        let now = Instant::now();
        let mut subs = Subs::default();
        let mut list = vec![session("a", Some("p"), SessionStatus::Busy, 1)];
        subs.turn(true, now);
        subs.track(&list, "p", now);
        let state = |subs: &Subs, list: &[SessionInfo], at| subs.rows(list, "p", at).first().map(|r| r.state);
        // Stopped: idle, holding its report, through the parent's turn ending and for as long as the user keeps it.
        (list[0].status, list[0].report_held) = (SessionStatus::Idle, true);
        assert!(!subs.track(&list, "p", now));
        subs.turn(false, now);
        let kept = now + Duration::from_secs(3600);
        subs.track(&list, "p", kept);
        assert_eq!(state(&subs, &list, kept), Some(SubState::Stopped), "stays while held");
        // Handed back: returned, until the turn its report starts ends.
        list[0].report_held = false;
        assert!(subs.track(&list, "p", kept));
        assert_eq!(state(&subs, &list, kept), Some(SubState::Returned));
        subs.turn(true, kept);
        subs.turn(false, kept + LEAVE);
        assert_eq!(state(&subs, &list, kept + LEAVE * 2), None);
        // Held when the pane first sees it (a reload): stopped too.
        let mut fresh = Subs::default();
        list[0].report_held = true;
        fresh.track(&list, "p", now);
        assert_eq!(state(&fresh, &list, now), Some(SubState::Stopped));
        // A child that returned in an earlier turn is not listed after a reload.
        list[0].report_held = false;
        let mut reloaded = Subs::default();
        reloaded.track(&list, "p", now);
        assert_eq!(state(&reloaded, &list, now), None);
    }
}
