//! Background jobs (`docs/AGENT_BLOCK.md` §4f, `docs/MEDIA.md` §6.5): the bar above the composer listing the
//! shown session's running jobs (a `background` command, one waiting for input, one the user detached, a
//! generation), detaching a running command or generation from its line, and stopping a job.

use std::collections::{HashMap, HashSet};

use gpui::{AnyElement, ClickEvent, Context, SharedString, div, prelude::*, px, rgb};

use super::{
    AgentPane, AgentPaneEvent,
    transcript::{JobRef, Row, Status},
    view::{BAR_ROW, Bar, icon_button, ink},
};
use crate::{assets::icon, theme, tooltip::Tooltip};

/// Room a row's note keeps for its hover actions.
const ACTIONS: f32 = 52.0;

/// What the pane tracks of jobs beyond the transcript (which says what runs: otto reports a job that never
/// ended as stopped once it restarts).
#[derive(Default)]
pub(super) struct ShellJobs {
    /// Calls the user detached, until otto answers.
    pub(super) detaching: HashSet<String>,
    /// The session's jobs as `job.list` last said, by id, while generations run (their progress and start).
    pub(super) listed: HashMap<String, otto_proto::JobInfo>,
    /// A `job.list` is out.
    polling: bool,
}

/// How often running generations' progress is read.
const POLL: std::time::Duration = std::time::Duration::from_secs(1);

/// The running jobs among `rows`' shell lines, oldest first.
pub(super) fn running(rows: &[Row]) -> Vec<JobRef> {
    rows.iter()
        .filter_map(|r| if let Row::Group(g) = r { Some(&g.lines) } else { None })
        .flatten()
        .filter(|l| matches!(l.status, Status::Running | Status::Input))
        .filter_map(|l| l.job.clone())
        .collect()
}

impl AgentPane {
    pub(super) fn reset_jobs(&mut self) {
        self.shell_jobs = ShellJobs::default();
    }

    /// While generations run in the background: read `job.list` for their progress, again a second after each answer.
    fn poll_jobs(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session_id.clone() else { return };
        if self.shell_jobs.polling || !running(&self.built).iter().any(|j| j.media.is_some()) {
            return;
        }
        self.shell_jobs.polling = true;
        let asked = session.clone();
        self.run(
            cx,
            move |c| async move {
                let listed = c.jobs(&session).await;
                tokio::time::sleep(POLL).await;
                listed
            },
            move |this, listed, _| {
                this.shell_jobs.polling = false;
                if let (Ok(listed), true) = (listed, this.session_id.as_deref() == Some(asked.as_str())) {
                    this.shell_jobs.listed = listed.into_iter().map(|j| (j.job_id.clone(), j)).collect();
                }
            },
        );
    }

    /// Detach running shell or generation call `call_id`: it returns now and its work runs on as a job.
    pub(super) fn detach_shell(&mut self, call_id: String, cx: &mut Context<Self>) {
        let Some(session) = self.session_id.clone() else { return };
        if !self.shell_jobs.detaching.insert(call_id.clone()) {
            return;
        }
        self.run(
            cx,
            {
                let call_id = call_id.clone();
                move |c| async move { c.detach_shell(&session, &call_id).await }
            },
            move |this, result, _| {
                this.shell_jobs.detaching.remove(&call_id);
                if let Err(error) = result {
                    this.notice = Some(format!("Couldn't detach it: {error}"));
                }
            },
        );
        cx.notify();
    }

    /// Stop `job` (`job.kill`); an older otto without it: close its block.
    fn kill_job(&mut self, job: JobRef, cx: &mut Context<Self>) {
        let Some(session) = self.session_id.clone() else { return };
        let id = job.id.clone();
        self.run(
            cx,
            move |c| async move { c.kill_job(&session, &id).await },
            move |this, result, cx| match (result, job.block) {
                (Ok(_), _) => {}
                (Err(_), Some(block)) => cx.emit(AgentPaneEvent::Stop(block)),
                (Err(error), None) => this.notice = Some(format!("Couldn't stop it: {error}")),
            },
        );
    }

    /// The jobs bar: `terminal · Shells · N running` (`Needs input` while one waits), `Media` with the first
    /// generation's kind icon when only generations run, `Jobs` for both; a row per running job.
    pub(super) fn jobs_bar(&mut self, cx: &mut Context<Self>) -> Option<Bar> {
        self.poll_jobs(cx);
        let jobs = running(&self.built);
        let first = jobs.first()?.command.clone();
        let waiting = jobs.iter().any(|j| j.waiting);
        let media = jobs.iter().filter(|j| j.media.is_some()).count();
        let (glyph, label, open_label) = match media {
            0 => ("terminal", "Shells", "Background shells"),
            n if n == jobs.len() => (super::media::glyph(jobs[0].media.unwrap_or(otto_proto::MediaKind::Image)), "Media", "Background generations"),
            _ => ("terminal", "Jobs", "Background jobs"),
        };
        let note = if waiting { "Needs input".to_owned() } else { format!("{} running", jobs.len()) };
        let summary = div()
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .text_color(rgb(theme::foreground()))
            .child(first)
            .into_any_element();
        Some(Bar {
            key: "jobs",
            icon: icon(glyph, 14.0, if waiting { theme::warning() } else { theme::muted() }).into_any_element(),
            label,
            summary: Some(summary),
            note: note.clone(),
            open_label: open_label.into(),
            open_note: note,
            rows: jobs.into_iter().enumerate().map(|(i, job)| self.job_row(i, job, cx)).collect(),
        })
    }

    /// A running job: its command (a generation: its kind's icon and prompt) and `Running` / `Needs input` / the
    /// generation's progress; on hover that note gives way to open (its block) and stop. A click opens its block.
    fn job_row(&self, i: usize, job: JobRef, cx: &mut Context<Self>) -> AnyElement {
        let muted = theme::muted();
        let group = SharedString::from(format!("job-{i}"));
        let progress = job.media.and(self.shell_jobs.listed.get(&job.id)).and_then(|j| j.progress.as_deref()).map(super::media::progress);
        let glyph = if let Some(kind) = job.media {
            icon(super::media::glyph(kind), 13.0, muted).into_any_element()
        } else if job.waiting {
            icon("terminal", 13.0, theme::warning()).into_any_element()
        } else {
            crate::browser_pane::spinner(&format!("job-spin-{i}"), 12.0, muted, cx)
        };
        let note = div()
            .flex_none()
            .max_w(px(220.0))
            .overflow_hidden()
            .whitespace_nowrap()
            .text_ellipsis()
            .min_w(px(ACTIONS))
            .flex()
            .justify_end()
            .text_size(px(11.5))
            .text_color(rgb(if job.waiting { theme::warning() } else { muted }))
            .child(if job.waiting { "Needs input".to_owned() } else { progress.unwrap_or_else(|| "Running".into()) });
        let open = job.block.clone().map(|block| {
            icon_button(gpui::ElementId::Name(format!("job-open-{i}").into()), "arrow-up-right", 12.0, 22.0, muted)
                .tooltip(Tooltip::text("Open its terminal"))
                .active(|s| s.scale(0.92))
                .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                    cx.stop_propagation();
                    cx.emit(AgentPaneEvent::Reveal(block.clone()));
                }))
        });
        let stop = {
            let job = job.clone();
            icon_button(gpui::ElementId::Name(format!("job-stop-{i}").into()), "stop", 11.0, 22.0, muted)
                .tooltip(Tooltip::text("Stop"))
                .active(|s| s.scale(0.92))
                .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                    cx.stop_propagation();
                    this.kill_job(job.clone(), cx);
                }))
        };
        let actions = div()
            .absolute()
            .right(px(8.0))
            .top_0()
            .h_full()
            .flex()
            .items_center()
            .gap(px(2.0))
            .opacity(0.0)
            .group_hover(group.clone(), |s| s.opacity(1.0))
            .children(open)
            .child(stop);
        let block = job.block.clone();
        div()
            .id(gpui::ElementId::Name(group.clone()))
            .group(group.clone())
            .relative()
            .h(px(BAR_ROW))
            .px(px(14.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .hover(|s| s.bg(theme::hover_fill()))
            .when(block.is_some(), |el| el.cursor_pointer())
            .child(div().w(px(14.0)).flex_none().flex().justify_center().child(glyph))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(rgb(theme::foreground()))
                    .child(job.command),
            )
            .child(note.group_hover(group, |s| s.opacity(0.0)))
            .child(actions)
            .on_click(cx.listener(move |_, _: &ClickEvent, _, cx| {
                if let Some(block) = &block {
                    cx.emit(AgentPaneEvent::Reveal(block.clone()));
                }
            }))
            .into_any_element()
    }
}

/// The `Detach` control on a running shell line.
pub(super) fn detach_button(key: &str, call_id: String, pending: bool, cx: &mut Context<AgentPane>) -> AnyElement {
    let muted = theme::muted();
    div()
        .id(gpui::ElementId::Name(format!("detach-{key}").into()))
        .flex_none()
        .h(px(20.0))
        .px(px(6.0))
        .flex()
        .items_center()
        .gap(px(4.0))
        .rounded(px(6.0))
        .text_size(px(11.5))
        .text_color(rgb(muted))
        .when(pending, |el| el.opacity(0.5))
        .when(!pending, |el| el.cursor_pointer().hover(|s| s.bg(theme::hover_fill()).text_color(rgb(theme::foreground()))).active(|s| s.scale(0.96)))
        .border_1()
        .border_color(rgb(ink(0.12)))
        .tooltip(Tooltip::text("Run it in the background: the agent goes on, and gets its result when it ends"))
        .child("Detach")
        .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
            cx.stop_propagation();
            this.detach_shell(call_id.clone(), cx);
        }))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::transcript::{Group, Kind, Line};

    fn line(key: &str, status: Status, job: Option<(&str, bool)>) -> Line {
        let mut line = Line::new(key, Kind::Shell);
        line.status = status;
        line.job = job.map(|(id, waiting)| JobRef { id: id.into(), command: format!("cmd {id}"), block: None, waiting, media: None });
        line
    }

    #[test]
    fn the_bar_lists_running_and_waiting_jobs_only() {
        let lines = vec![
            line("a", Status::Running, Some(("j1", false))),
            line("b", Status::Done, Some(("j2", false))),
            line("c", Status::Running, None),
            line("d", Status::Input, Some(("j3", true))),
        ];
        let rows = vec![Row::Group(Group { key: "g".into(), lines, live: false, summary: String::new(), failed: false })];
        let jobs: Vec<(String, bool)> = running(&rows).into_iter().map(|j| (j.id, j.waiting)).collect();
        assert_eq!(jobs, [("j1".into(), false), ("j3".into(), true)]);
    }
}
