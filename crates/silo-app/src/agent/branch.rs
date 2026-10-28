//! Branching a thread from one of its messages (`session.fork`): the Branch
//! action under a user message or a reply, the branch opening in the same
//! block, and the strip that leads back to the original.

use std::collections::{HashMap, HashSet};

use gpui::{AnyElement, ClickEvent, Context, SharedString, div, prelude::*, px, rgb};
use otto_proto::{Item, Role};

use super::{AgentPane, AgentPaneEvent, OPTIMISTIC, pill_input, view::icon_button};
use crate::{
    assets::icon,
    theme::{self, Continuous as _},
    tooltip::Tooltip,
};

/// The branch the pane just opened: the original it leads back to.
#[derive(Clone, Debug)]
pub(super) struct Branched {
    /// The branch (the strip shows while the block shows it).
    pub session: String,
    pub from: String,
    pub from_title: String,
}

/// Whether a message can be branched from now, and if not why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum BranchState {
    /// No action: no session, a sub-agent, or a bubble not sent yet.
    Hidden,
    Disabled(&'static str),
    Ready(i64),
}

/// The sequence to fork at so the branch holds the history through item `item_id`: through it for a user
/// message; for a reply, through it and the tool results right after it (its calls never end the branch
/// unanswered). The latest of the kept items' stored sequences; `None` when the item isn't stored yet (or the
/// server sends no sequences).
pub(super) fn fork_seq(items: &[Item], seqs: &HashMap<String, i64>, item_id: &str) -> Option<i64> {
    let at = items.iter().position(|i| i.id == item_id)?;
    seqs.get(item_id)?;
    let mut end = at;
    if items[at].role == Role::Assistant {
        while items.get(end + 1).is_some_and(|i| i.role == Role::Tool) {
            end += 1;
        }
    }
    items[..=end].iter().filter_map(|i| seqs.get(&i.id)).max().copied()
}

/// `title` without a branch suffix (`… (branch)`, `… (branch 3)`).
fn branch_base(title: &str) -> &str {
    let title = title.trim();
    if let Some(base) = title.strip_suffix(" (branch)") {
        return base.trim_end();
    }
    if let Some(open) = title.strip_suffix(')').and_then(|t| t.rsplit_once(" (branch "))
        && !open.1.is_empty()
        && open.1.bytes().all(|b| b.is_ascii_digit())
    {
        return open.0.trim_end();
    }
    title
}

/// A branch's title: `<title> (branch)`, numbered (`(branch 2)`, …) past the titles `taken`; a branch of a
/// branch numbers its original's.
pub(super) fn branch_title<'a>(source: &str, taken: impl IntoIterator<Item = &'a str>) -> String {
    let base = branch_base(source);
    let base = if base.is_empty() { "Untitled" } else { base };
    let taken: HashSet<&str> = taken.into_iter().map(str::trim).collect();
    let first = format!("{base} (branch)");
    if !taken.contains(first.as_str()) {
        return first;
    }
    (2..).map(|n| format!("{base} (branch {n})")).find(|t| !taken.contains(t.as_str())).unwrap_or(first)
}

impl AgentPane {
    /// Branch from item `item_id`, now: its fork sequence, or why not.
    pub(super) fn branch_state(&self, item_id: &str) -> BranchState {
        if self.session_id.is_none() || item_id == OPTIMISTIC || self.is_subagent() {
            return BranchState::Hidden;
        }
        let Some(mirror) = self.mirror() else { return BranchState::Hidden };
        if self.running {
            return BranchState::Disabled("Stop the turn to branch");
        }
        if self.branching {
            return BranchState::Disabled("Branching…");
        }
        match fork_seq(&mirror.items, &mirror.item_seqs, item_id) {
            Some(seq) => BranchState::Ready(seq),
            None if mirror.item_seqs.is_empty() => BranchState::Disabled("Update otto to branch"),
            None => BranchState::Disabled("Not saved yet"),
        }
    }

    /// The Branch action of a message (`item_id`; `key` its row's), among its hover actions.
    pub(super) fn branch_button(&self, item_id: &str, key: &str, cx: &mut Context<Self>) -> Option<AnyElement> {
        let (tip, ready) = match self.branch_state(item_id) {
            BranchState::Hidden => return None,
            BranchState::Disabled(why) => (why, false),
            BranchState::Ready(_) => ("Branch from here", true),
        };
        let id = item_id.to_owned();
        let button = icon_button(gpui::ElementId::Name(format!("branch-{key}").into()), "git-branch", 13.0, 24.0, theme::muted())
            .tooltip(Tooltip::text(tip))
            .when(!ready, |el| el.opacity(0.45).cursor_default())
            .when(ready, |el| el.on_click(cx.listener(move |this, _: &ClickEvent, _, cx| this.branch(&id, cx))));
        Some(button.into_any_element())
    }

    /// Fork the shown session at item `item_id` and show the branch in this block, titled
    /// `<title> (branch)`; the strip above the composer leads back.
    pub(super) fn branch(&mut self, item_id: &str, cx: &mut Context<Self>) {
        let BranchState::Ready(at) = self.branch_state(item_id) else { return };
        let Some(source) = self.session_id.clone() else { return };
        let from_title = self.header_title().unwrap_or_default();
        let title = branch_title(&from_title, self.sessions.iter().map(|s| s.title.as_str()));
        self.branching = true;
        self.notice = None;
        cx.notify();
        let from = source.clone();
        self.run(
            cx,
            move |c| async move {
                let info = c.session_fork(&source, Some(at)).await?;
                // Named after its original; a failed rename leaves otto's copy of the title.
                let renamed = c.session_rename(&info.id, &title).await.map_err(|e| super::describe(&e)).err();
                Ok((info, title, renamed))
            },
            move |this, result, cx| {
                this.branching = false;
                let (info, title, renamed) = match result {
                    Ok(done) => done,
                    Err(error) => {
                        this.notice = Some(format!("Couldn't branch: {error}"));
                        return;
                    }
                };
                // Still on the original (another session picked meanwhile keeps the block).
                if this.session_id.as_deref() != Some(from.as_str()) {
                    return this.refresh_sessions(cx);
                }
                this.forget_sends();
                this.left = None;
                this.session_id = Some(info.id.clone());
                this.follow(cx);
                this.title.clone_from(&title);
                this.branched = Some(Branched { session: info.id.clone(), from, from_title });
                if let Some(error) = renamed {
                    this.notice = Some(format!("Couldn't name the branch: {error}"));
                }
                cx.emit(AgentPaneEvent::Session { session_id: info.id, title });
                this.refresh_sessions(cx);
            },
        );
    }

    /// `Branched from <title> · Back to it ×` above the composer while the block shows the branch it opened.
    pub(super) fn render_branched(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let b = self.branched.as_ref().filter(|b| self.session_id.as_deref() == Some(b.session.as_str()))?;
        let muted = theme::muted();
        let title = if b.from_title.trim().is_empty() { "Untitled".to_owned() } else { b.from_title.clone() };
        let back = b.from.clone();
        let link = div()
            .id("agent-branch-back")
            .flex_none()
            .px(px(6.0))
            .h(px(20.0))
            .flex()
            .items_center()
            .continuous(5.0)
            .text_color(rgb(theme::accent()))
            .cursor_pointer()
            .hover(|s| s.bg(theme::hover_fill()))
            .child("Back to original")
            .tooltip(Tooltip::text(SharedString::from(format!("Open {title} again"))))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.pick_session(back.clone(), window, cx)));
        let close = icon_button("agent-branch-dismiss", "close", 11.0, 20.0, muted).tooltip(Tooltip::text("Dismiss")).on_click(cx.listener(
            |this, _: &ClickEvent, _, cx| {
                this.branched = None;
                cx.notify();
            },
        ));
        Some(
            div()
                .pb(px(8.0))
                .px(px(pill_input::HALO + pill_input::TEXT_X))
                .flex()
                .items_center()
                .gap(px(6.0))
                .text_size(px(12.0))
                .text_color(rgb(muted))
                .child(icon("git-branch", 12.0, muted))
                .child(div().min_w_0().overflow_hidden().whitespace_nowrap().text_ellipsis().child(format!("Branched from {title}")))
                .child(link)
                .child(div().flex_1())
                .child(close)
                .into_any_element(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otto_proto::Part;

    fn item(id: &str, role: Role) -> Item {
        Item::new(id, role, vec![Part::text(id)])
    }

    #[test]
    fn a_user_message_forks_through_itself_and_a_reply_through_its_tool_results() {
        let items = vec![
            item("u1", Role::User),
            item("a1", Role::Assistant),
            item("t1", Role::Tool),
            item("t2", Role::Tool),
            item("a2", Role::Assistant),
            item("u2", Role::User),
            item("a3", Role::Assistant),
            item("live", Role::Assistant),
        ];
        let seqs: HashMap<String, i64> = [("u1", 3), ("a1", 9), ("t1", 11), ("t2", 12), ("a2", 15), ("u2", 18), ("a3", 22)]
            .into_iter()
            .map(|(id, seq)| (id.to_owned(), seq))
            .collect();
        // A user message: through it, so the branch continues from there.
        assert_eq!(fork_seq(&items, &seqs, "u1"), Some(3));
        assert_eq!(fork_seq(&items, &seqs, "u2"), Some(18));
        // A reply with calls: through their results too.
        assert_eq!(fork_seq(&items, &seqs, "a1"), Some(12));
        assert_eq!(fork_seq(&items, &seqs, "a2"), Some(15));
        assert_eq!(fork_seq(&items, &seqs, "a3"), Some(22));
        // An item rewritten later than one after it (its latest version) still gets the latest of those kept.
        let mut later = seqs.clone();
        later.insert("u1".into(), 13);
        assert_eq!(fork_seq(&items, &later, "a1"), Some(13));
        // Not stored yet, unknown, or a server without sequences: no fork.
        assert_eq!(fork_seq(&items, &seqs, "live"), None);
        assert_eq!(fork_seq(&items, &seqs, "nope"), None);
        assert_eq!(fork_seq(&items, &HashMap::new(), "u1"), None);
    }

    #[test]
    fn branch_titles_number_past_taken_ones() {
        assert!(crate::assets::has_icon("git-branch"));
        assert_eq!(branch_title("Fix login", ["Fix login"]), "Fix login (branch)");
        assert_eq!(branch_title("Fix login", ["Fix login", "Fix login (branch)"]), "Fix login (branch 2)");
        assert_eq!(branch_title("Fix login", ["Fix login (branch)", "Fix login (branch 2)", "Fix login (branch 4)"]), "Fix login (branch 3)");
        // A branch of a branch numbers its original's.
        assert_eq!(branch_title("Fix login (branch)", ["Fix login", "Fix login (branch)"]), "Fix login (branch 2)");
        assert_eq!(branch_title("Fix login (branch 2)", ["Fix login (branch)", "Fix login (branch 2)"]), "Fix login (branch 3)");
        // Not a branch suffix: kept.
        assert_eq!(branch_title("Notes (branch x)", []), "Notes (branch x) (branch)");
        assert_eq!(branch_title("  ", []), "Untitled (branch)");
    }
}
