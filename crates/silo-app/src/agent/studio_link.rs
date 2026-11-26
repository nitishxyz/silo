//! What a Studio block (`crate::studio`, docs/STUDIO.md §4.2) asks of an agent pane: the `studio` agent as
//! the default of a pane split off it, its selection put in the composer as mentions, and a first message
//! sent once the pane can send (Start with the agent).

use std::time::Duration;

use gpui::{Context, Window};
use otto_proto::MentionKind;

use super::AgentPane;

/// How long a fresh pane gets to connect before a queued first message is dropped.
const READY_WAIT: Duration = Duration::from_secs(20);
const READY_POLL: Duration = Duration::from_millis(250);

impl AgentPane {
    /// Use `agent` for the session this pane starts (one it already shows keeps its own).
    pub(crate) fn prefer_agent(&mut self, agent: &str, cx: &mut Context<Self>) {
        if self.session_id.is_none() {
            self.choice.agent = Some(agent.to_owned());
            cx.notify();
        }
    }

    /// Append `mentions` to the composer as chips (each after a space, a whole canvas's too), the caret after them,
    /// and focus it. Nodes' chips read their titles once otto names them (`studio.mentions` for their canvas).
    pub(crate) fn insert_mentions(&mut self, mentions: Vec<(MentionKind, String)>, window: &mut Window, cx: &mut Context<Self>) {
        let mut canvases: Vec<String> = Vec::new();
        for (kind, target) in &mentions {
            if let (MentionKind::Canvas, Some((canvas, _))) = (kind, target.split_once('#'))
                && !canvases.iter().any(|c| c == canvas)
            {
                canvases.push(canvas.to_owned());
            }
        }
        self.editor.update(cx, |e, cx| {
            let last = mentions.len().saturating_sub(1);
            for (i, (kind, target)) in mentions.into_iter().enumerate() {
                if !e.text().is_empty() && !e.text().ends_with(char::is_whitespace) {
                    let (text, chips) = (format!("{} ", e.text()), e.chips().to_vec());
                    e.set_content(text, chips, cx);
                }
                let end = e.text().len();
                e.insert_chip(end..end, kind, target, cx);
                // A whole canvas gets no space when picked (`#` drills in); here the user types on after it.
                if i == last && !e.text().ends_with(char::is_whitespace) {
                    let (text, chips) = (format!("{} ", e.text()), e.chips().to_vec());
                    e.set_content(text, chips, cx);
                }
            }
        });
        self.name_nodes(canvases, cx);
        self.focus_composer(window, cx);
    }

    /// Ask otto for the nodes of `canvases` so their chips read the nodes' titles ([`super::mention::label`]).
    fn name_nodes(&mut self, canvases: Vec<String>, cx: &mut Context<Self>) {
        let Some(cwd) = self.files_cwd() else { return };
        for canvas in canvases {
            let cwd = cwd.clone();
            self.run(
                cx,
                move |c| async move { c.studio_mentions(&cwd, &format!("{canvas}#"), 1000).await },
                |_, result, cx| {
                    if let Ok(found) = result {
                        cx.default_global::<super::mention::StudioNames>().add(&found);
                    }
                },
            );
        }
    }

    /// Send `text` as soon as the pane can (connected, signed in, its project known): the session it went
    /// to, `None` if it couldn't in time.
    pub(crate) fn send_when_ready(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) -> gpui::Task<Option<String>> {
        cx.spawn_in(window, async move |this, cx| {
            let start = std::time::Instant::now();
            let mut sent = false;
            while start.elapsed() < READY_WAIT {
                let step = this
                    .update_in(cx, |this, window, cx| {
                        if sent {
                            return this.session_id.clone().map(Some);
                        }
                        let ready = this.catalog.as_ref().is_some_and(super::Catalog::any_signed_in)
                            && (this.session_id.is_some() || this.project.is_some())
                            && !this.reconnecting();
                        if ready {
                            this.editor.update(cx, |e, cx| e.set_text(text.clone(), cx));
                            this.submit(window, cx);
                            sent = true;
                        }
                        None
                    })
                    .ok()?;
                if let Some(session) = step {
                    return session;
                }
                cx.background_executor().timer(READY_POLL).await;
            }
            None
        })
    }
}
