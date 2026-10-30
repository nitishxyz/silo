//! The Changes block's branch actions: Push (setting the upstream the
//! first time), Pull and Fetch with git's progress, the branch's pull
//! request through `gh` (shown, or a Create PR form), and conflicts: the
//! active conflicted file's regions with a side to take per region or for
//! the file, Mark resolved, and Continue / Abort of the stopped op.

use gpui::{AppContext as _, Context, Entity, Subscription, Window};
use silo_proto::{
    changes::{
        ChangesConflictParams, ChangesPr, ChangesPrCreateParams, ChangesPushParams, ChangesPushed, ChangesResolveParams, ChangesStatus,
        ChangesStatusParams, ConflictFile, FileStatus, GhState, PullRequest, RepoOp, Take,
    },
    compat::feature,
    control::Method,
};

use super::{ChangesPane, ChangesPaneEvent};
use crate::{
    agent::editor::{Editor, EditorEvent},
    text_field::{FieldEvent, TextField},
};

/// What the sync row and the conflict view hold.
#[derive(Default)]
pub(crate) struct SyncState {
    /// The branch's pull request, or what a new one starts with.
    pub pr: Option<ChangesPr>,
    /// The branch `pr` was read for.
    pub pr_branch: Option<String>,
    pub form: Option<PrForm>,
    /// git's last progress line while a push, pull or fetch runs.
    pub progress: Option<String>,
    /// The active conflicted file's regions.
    pub conflict: Option<ConflictFile>,
}

/// The Create PR card.
pub(crate) struct PrForm {
    pub title: Entity<TextField>,
    pub body: Entity<Editor>,
    pub base: Option<String>,
    pub bases_open: bool,
    pub draft: bool,
    _subs: Vec<Subscription>,
}

/// The notice after a push.
pub(crate) fn pushed_text(p: &ChangesPushed) -> String {
    match (p.up_to_date, p.set_upstream) {
        (true, _) => format!("{} is up to date on {}", p.branch, p.remote),
        (false, true) => format!("Pushed {} to {} and tracking it", p.branch, p.remote),
        (false, false) => format!("Pushed {} to {}", p.branch, p.remote),
    }
}

/// The banner over a stopped op.
pub(crate) fn op_title(op: RepoOp, conflicts: usize) -> String {
    let what = match op {
        RepoOp::Merge => "Merging",
        RepoOp::Rebase => "Rebasing",
        RepoOp::CherryPick => "Cherry-picking",
        RepoOp::Revert => "Reverting",
        RepoOp::Unknown => "Stopped",
    };
    match conflicts {
        0 => format!("{what} · conflicts resolved"),
        1 => format!("{what} · 1 conflicted file"),
        n => format!("{what} · {n} conflicted files"),
    }
}

/// `↑2 ↓1`-style counts, empty when even.
pub(crate) fn counts(status: &ChangesStatus) -> String {
    let mut out = Vec::new();
    if status.ahead > 0 {
        out.push(format!("\u{2191}{}", status.ahead));
    }
    if status.behind > 0 {
        out.push(format!("\u{2193}{}", status.behind));
    }
    out.join(" ")
}

/// What the PR button does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PrButton {
    /// Open this one.
    Open(PullRequest),
    Create,
    /// `gh` is signed out: say how to sign in.
    SignIn,
}

/// The PR button for `pr` on `branch`; none without `gh`, off GitHub, or
/// on the default branch with no pull request.
pub(crate) fn pr_button(pr: &ChangesPr, branch: &str) -> Option<PrButton> {
    match pr.gh {
        GhState::Ready => {}
        GhState::SignedOut => return Some(PrButton::SignIn),
        GhState::Missing | GhState::NotGithub | GhState::Unknown => return None,
    }
    match &pr.pr {
        Some(p) if p.state == "open" => Some(PrButton::Open(p.clone())),
        _ if pr.bases.first().is_none_or(|b| b == branch) => None,
        _ => Some(PrButton::Create),
    }
}

impl ChangesPane {
    /// The server pushes, pulls, opens pull requests and resolves conflicts.
    pub(crate) fn syncs(&self) -> bool {
        self.control.as_ref().is_some_and(|c| c.peer_supports(feature::CHANGES_SYNC))
    }

    fn status_params(&self) -> ChangesStatusParams {
        ChangesStatusParams { block_id: self.block_id.clone() }
    }

    /// A push, pull, fetch or PR is running: git's progress shows.
    pub fn progress(&mut self, text: String, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            self.sync.progress = Some(text);
            cx.notify();
        }
    }

    pub(crate) fn push_now(&mut self, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        (self.busy, self.notice, self.sync.progress) = (Some("Pushing…"), None, None);
        let method = Method::ChangesPush(ChangesPushParams { block_id: self.block_id.clone() });
        self.request(method, cx, |this, result: anyhow::Result<ChangesPushed>, cx| {
            (this.busy, this.sync.progress) = (None, None);
            this.notice = Some(match result {
                Ok(p) => (pushed_text(&p), false),
                Err(error) => (crate::files::describe(&error), true),
            });
            this.refresh(cx);
            this.load_pr(true, cx);
        });
        cx.notify();
    }

    /// Pull (`pull`) or fetch, then the status they leave.
    pub(crate) fn pull_now(&mut self, pull: bool, cx: &mut Context<Self>) {
        if self.busy.is_some() {
            return;
        }
        (self.busy, self.notice, self.sync.progress) = (Some(if pull { "Pulling…" } else { "Fetching…" }), None, None);
        let params = self.status_params();
        let method = if pull { Method::ChangesPull(params) } else { Method::ChangesFetch(params) };
        self.request(method, cx, move |this, result: anyhow::Result<ChangesStatus>, cx| {
            (this.busy, this.sync.progress) = (None, None);
            match result {
                Ok(status) => {
                    this.notice = match (pull, status.op, status.behind) {
                        (true, Some(_), _) => Some(("The pull stopped on conflicts: resolve them, then Continue".into(), true)),
                        (false, _, 0) => Some(("Up to date".into(), false)),
                        (false, _, n) => Some((if n == 1 { "1 new commit to pull".into() } else { format!("{n} new commits to pull") }, false)),
                        (true, None, _) => None,
                    };
                    this.took_status(Ok(status), cx);
                }
                Err(error) => {
                    this.notice = Some((crate::files::describe(&error), true));
                    this.refresh(cx);
                }
            }
        });
        cx.notify();
    }

    /// Read the branch's pull request (once per branch unless `force`).
    pub(crate) fn load_pr(&mut self, force: bool, cx: &mut Context<Self>) {
        let branch = self.status.as_ref().filter(|s| s.repo).and_then(|s| s.branch.clone());
        if !self.syncs() || branch.is_none() {
            (self.sync.pr, self.sync.pr_branch) = (None, None);
            return;
        }
        if !force && branch == self.sync.pr_branch {
            return;
        }
        if branch != self.sync.pr_branch {
            self.sync.pr = None;
        }
        self.sync.pr_branch.clone_from(&branch);
        self.request(Method::ChangesPr(self.status_params()), cx, move |this, result: anyhow::Result<ChangesPr>, _| {
            if this.sync.pr_branch != branch {
                return;
            }
            match result {
                Ok(pr) => this.sync.pr = Some(pr),
                Err(error) => tracing::debug!("changes.pr: {error:#}"),
            }
        });
    }

    /// The PR button pressed.
    pub(crate) fn pr_action(&mut self, button: PrButton, window: &mut Window, cx: &mut Context<Self>) {
        match button {
            PrButton::Open(pr) => cx.emit(ChangesPaneEvent::OpenUrl(pr.url)),
            PrButton::SignIn => self.notice = Some(("Sign in to GitHub first: run gh auth login in a terminal".into(), true)),
            PrButton::Create => self.open_pr_form(window, cx),
        }
        cx.notify();
    }
}

impl ChangesPane {
    fn open_pr_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pr) = &self.sync.pr else { return };
        let (title_text, body_text, base) = (pr.title.clone(), pr.body.clone(), pr.bases.first().cloned());
        let title = cx.new(|cx| {
            let mut f = TextField::new("Title", cx).on_glass();
            f.set_text(title_text, cx);
            f
        });
        let body = cx.new(|cx| {
            let mut e = Editor::new("Description…", cx);
            e.multiline = true;
            e.set_text(body_text, cx);
            e
        });
        let subs = vec![
            cx.subscribe_in(&title, window, |this, _, event: &FieldEvent, window, cx| match event {
                FieldEvent::Submit => this.create_pr(cx),
                FieldEvent::Cancel => this.close_pr_form(window, cx),
                _ => {}
            }),
            cx.subscribe_in(&body, window, |this, _, event: &EditorEvent, window, cx| match event {
                EditorEvent::Submit => this.create_pr(cx),
                EditorEvent::Escape => this.close_pr_form(window, cx),
                _ => {}
            }),
        ];
        let focus = title.read(cx).focus.clone();
        self.sync.form = Some(PrForm { title, body, base, bases_open: false, draft: false, _subs: subs });
        window.focus(&focus, cx);
        cx.notify();
    }

    pub(crate) fn close_pr_form(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.sync.form = None;
        window.focus(&self.focus, cx);
        cx.notify();
    }

    pub(crate) fn create_pr(&mut self, cx: &mut Context<Self>) {
        let Some(form) = &self.sync.form else { return };
        if self.busy.is_some() {
            return;
        }
        let title = form.title.read(cx).text.trim().to_owned();
        if title.is_empty() {
            self.notice = Some(("A pull request needs a title".into(), true));
            return cx.notify();
        }
        let body = form.body.read(cx).text().trim().to_owned();
        let params = ChangesPrCreateParams { block_id: self.block_id.clone(), title, body, base: form.base.clone(), draft: form.draft };
        (self.busy, self.notice, self.sync.progress) = (Some("Opening…"), None, None);
        self.request(Method::ChangesPrCreate(params), cx, |this, result: anyhow::Result<PullRequest>, cx| {
            (this.busy, this.sync.progress) = (None, None);
            match result {
                Ok(pr) => {
                    this.notice = Some((format!("Opened pull request #{}", pr.number), false));
                    this.sync.form = None;
                    if let Some(state) = this.sync.pr.as_mut() {
                        state.pr = Some(pr.clone());
                    }
                    cx.emit(ChangesPaneEvent::OpenUrl(pr.url));
                    this.refresh(cx);
                }
                Err(error) => this.notice = Some((crate::files::describe(&error), true)),
            }
        });
        cx.notify();
    }

    // ------------------------------------------------------------ conflicts

    /// The active file, when it is conflicted and can be resolved here.
    pub(crate) fn active_conflicted(&self) -> Option<&str> {
        let active = self.state.active.as_deref()?;
        let file = self.status.as_ref()?.files.iter().find(|f| f.path == active)?;
        (file.status == FileStatus::Conflicted && self.syncs() && self.mode() != super::model::ListMode::ReadOnly).then_some(active)
    }

    /// Read the active conflicted file's regions (else forget them).
    pub(crate) fn load_conflict(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.active_conflicted().map(str::to_owned) else {
            self.sync.conflict = None;
            return;
        };
        let method = Method::ChangesConflict(ChangesConflictParams { block_id: self.block_id.clone(), path: path.clone() });
        self.request(method, cx, move |this, result: anyhow::Result<ConflictFile>, _| {
            if this.active_conflicted() != Some(path.as_str()) {
                return;
            }
            match result {
                Ok(file) => this.sync.conflict = Some(file),
                Err(error) => this.notice = Some((crate::files::describe(&error), true)),
            }
        });
    }

    /// Take `take` for `regions` (none: the whole file), or with no
    /// `take`, mark the file resolved.
    pub(crate) fn resolve(&mut self, path: &str, take: Option<Take>, regions: Option<Vec<String>>, cx: &mut Context<Self>) {
        let method = Method::ChangesResolve(ChangesResolveParams { block_id: self.block_id.clone(), path: path.to_owned(), take, regions });
        self.write(method, if take.is_some() { "Resolving…" } else { "Marking…" }, cx);
    }

    /// Continue (or with `abort`, abort) the stopped merge or rebase.
    pub(crate) fn finish_op(&mut self, abort: bool, cx: &mut Context<Self>) {
        let params = self.status_params();
        let method = if abort { Method::ChangesAbort(params) } else { Method::ChangesContinue(params) };
        self.write(method, if abort { "Aborting…" } else { "Continuing…" }, cx);
    }

    /// Files still conflicted.
    pub(crate) fn conflicted(&self) -> usize {
        self.status.as_ref().map_or(0, |s| s.files.iter().filter(|f| f.status == FileStatus::Conflicted).count())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(gh: GhState, open: Option<&str>, bases: &[&str]) -> ChangesPr {
        let pr = open.map(|state| PullRequest {
            number: 12,
            url: "https://github.com/o/r/pull/12".into(),
            title: "T".into(),
            state: state.into(),
            draft: false,
            base: "main".into(),
        });
        ChangesPr { gh, pr, bases: bases.iter().map(|b| (*b).to_owned()).collect(), title: String::new(), body: String::new() }
    }

    #[test]
    fn the_pr_button_follows_gh_and_the_branch() {
        assert!(matches!(pr_button(&pr(GhState::Ready, Some("open"), &["main"]), "fix"), Some(PrButton::Open(p)) if p.number == 12));
        assert_eq!(pr_button(&pr(GhState::Ready, Some("merged"), &["main"]), "fix"), Some(PrButton::Create), "a merged one: open another");
        assert_eq!(pr_button(&pr(GhState::Ready, None, &["main"]), "main"), None, "the default branch");
        assert_eq!(pr_button(&pr(GhState::Missing, None, &["main"]), "fix"), None);
        assert_eq!(pr_button(&pr(GhState::NotGithub, None, &["main"]), "fix"), None);
        assert_eq!(pr_button(&pr(GhState::SignedOut, None, &[]), "fix"), Some(PrButton::SignIn));
    }

    #[test]
    fn sync_texts() {
        let p = |up_to_date, set_upstream| ChangesPushed { remote: "origin".into(), branch: "fix".into(), set_upstream, up_to_date };
        assert_eq!(pushed_text(&p(false, true)), "Pushed fix to origin and tracking it");
        assert_eq!(pushed_text(&p(false, false)), "Pushed fix to origin");
        assert_eq!(pushed_text(&p(true, false)), "fix is up to date on origin");
        assert_eq!(op_title(RepoOp::Merge, 2), "Merging · 2 conflicted files");
        assert_eq!(op_title(RepoOp::Rebase, 0), "Rebasing · conflicts resolved");
        assert_eq!(super::super::sync_view::side_label("HEAD:src/a.rs", "ours"), "HEAD · ours");
        assert_eq!(super::super::sync_view::side_label("a1b2c3d (Fix: it)", "theirs"), "a1b2c3d (Fix: it) · theirs");
        assert_eq!(super::super::sync_view::side_label("", "ours"), "ours");
    }
}
