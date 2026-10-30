//! Changes blocks (docs/CHANGES_BLOCK.md): a review of code changes in a
//! git repo on the server's machine, through the `git` command line. Every
//! path is relative to the block's root ([`crate::model::ChangesState::root`],
//! the repo's top level). Feature [`crate::compat::feature::CHANGES`].

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::model::{AgentLink, BlockId, ChangeMark, ChangesScope, ReviewComment};

/// `changes.status`: the changed files of the block's scope.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ChangesStatusParams {
    pub block_id: BlockId,
}

/// `changes.diff`: one file's hunks in the block's scope.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ChangesDiffParams {
    pub block_id: BlockId,
    pub path: String,
    /// Context lines (default: the block's `context`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<u32>,
}

/// `changes.set`: change the block's view state; omitted fields stay.
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct ChangesSetParams {
    pub block_id: BlockId,
    /// What is compared: `{"kind":"working"}`, `staged`, `unstaged`,
    /// `{"kind":"tree","base":…,"head"?:…,"label"?:…}` or
    /// `{"kind":"range","base":…,"head"?:…}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<ChangesScope>,
    /// Open tabs, in order (replaces the list).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open: Option<Vec<String>>,
    /// The file shown; empty for the summary. A path not open is opened
    /// (appended).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<String>,
    /// The file list is shown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub list: Option<bool>,
    /// Side by side; false: unified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub split: Option<bool>,
    /// Context lines around each hunk.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<u32>,
    /// Unsent review comments (replaces the list).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comments: Option<Vec<ReviewComment>>,
    /// The agent the review goes back to; `null` unlinks.
    #[serde(default, skip_serializing_if = "Option::is_none", with = "double_option")]
    #[schemars(with = "Option<AgentLink>")]
    pub agent: Option<Option<AgentLink>>,
    /// Marked lines (replaces the list; empty clears them).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marks: Option<Vec<ChangeMark>>,
}

/// `Option<Option<T>>`: absent is `None`, `null` is `Some(None)`.
mod double_option {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    #[allow(clippy::ref_option, clippy::option_option)]
    pub fn serialize<T: Serialize, S: Serializer>(value: &Option<Option<T>>, serializer: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(inner) => inner.serialize(serializer),
            None => serializer.serialize_none(),
        }
    }

    #[allow(clippy::option_option)]
    pub fn deserialize<'de, T: Deserialize<'de>, D: Deserializer<'de>>(deserializer: D) -> Result<Option<Option<T>>, D::Error> {
        Option::<T>::deserialize(deserializer).map(Some)
    }
}

/// `changes.stage`, `changes.unstage` and `changes.revert`: a whole file,
/// or only the hunks named (ids from `changes.diff` in the block's scope,
/// with the block's `context`: ids depend on the context lines).
/// A hunk the file no longer has is refused with
/// [`crate::control::RpcError::CONFLICT`].
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ChangesHunksParams {
    pub block_id: BlockId,
    pub path: String,
    /// Hunk ids from `changes.diff`; omitted: the whole file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hunks: Option<Vec<String>>,
}

/// `changes.commit`: commit the index (hooks run).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ChangesCommitParams {
    pub block_id: BlockId,
    pub message: String,
    /// Amend the last commit.
    #[serde(default)]
    pub amend: bool,
}

/// `changes.commit`'s result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ChangesCommitted {
    /// The new commit (full sha).
    pub sha: String,
    /// Its subject line.
    pub summary: String,
}

/// `changes.root`: review another repo; the view state starts afresh.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ChangesRootParams {
    pub block_id: BlockId,
    /// An absolute path (`~` is the home directory), or one relative to
    /// the current root; the repo holding it is the new root.
    pub path: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    Untracked,
    Conflicted,
    TypeChanged,
    #[serde(other)]
    Unknown,
}

/// A file changed in the scope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ChangedFile {
    pub path: String,
    /// Renames and copies: where it came from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub status: FileStatus,
    /// The index differs from `HEAD` for it (scopes on the work tree).
    #[serde(default)]
    pub staged: bool,
    /// The work tree differs from the index for it (untracked included).
    #[serde(default)]
    pub unstaged: bool,
    /// Lines added and removed in the scope (0 for binaries).
    #[serde(default)]
    pub added: u32,
    #[serde(default)]
    pub removed: u32,
    #[serde(default)]
    pub binary: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ChangesTotals {
    pub files: u32,
    pub added: u32,
    pub removed: u32,
}

/// `changes.status`'s (and the writes') result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ChangesStatus {
    /// The root is in a git work tree; false: nothing else is filled.
    pub repo: bool,
    /// The checked-out branch; `None` when detached.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// `HEAD`'s short sha; `None` before the first commit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// The branch's upstream (`origin/main`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    #[serde(default)]
    pub ahead: u32,
    #[serde(default)]
    pub behind: u32,
    /// By path.
    pub files: Vec<ChangedFile>,
    /// Over every file, those left out too.
    pub totals: ChangesTotals,
    /// More than [`ChangesStatus::MAX_FILES`] files; the rest are left out.
    #[serde(default)]
    pub truncated: bool,
    /// What git is in the middle of (a merge stopped on conflicts, say).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub op: Option<RepoOp>,
    /// The remote `changes.push` goes to: the upstream's, else the only
    /// remote (or `origin`). Feature [`crate::compat::feature::CHANGES_SYNC`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote: Option<String>,
}

/// An operation stopped part way, for `changes.continue` / `changes.abort`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RepoOp {
    Merge,
    Rebase,
    CherryPick,
    Revert,
    #[serde(other)]
    Unknown,
}

impl ChangesStatus {
    pub const MAX_FILES: usize = 2000;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LineKind {
    Context,
    Added,
    Removed,
    /// `\ No newline at end of file`, after the line it is about.
    NoNewline,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DiffLine {
    pub kind: LineKind,
    /// 1-based line on the old side (context and removed lines).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old: Option<u32>,
    /// 1-based line on the new side (context and added lines).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new: Option<u32>,
    /// The line without its `+`/`-`/` ` prefix and line break.
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Hunk {
    /// Stable while the hunk's lines are: stage, unstage and revert name
    /// hunks by it.
    pub id: String,
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    /// What follows the `@@ … @@` (often the enclosing function); may be
    /// empty.
    #[serde(default)]
    pub header: String,
    pub lines: Vec<DiffLine>,
}

/// `changes.diff`'s result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FileDiff {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub status: FileStatus,
    /// Binary: no hunks.
    #[serde(default)]
    pub binary: bool,
    pub hunks: Vec<Hunk>,
    /// Over [`FileDiff::MAX_BYTES`] of diff: the hunks so far.
    #[serde(default)]
    pub truncated: bool,
}

impl FileDiff {
    pub const MAX_BYTES: usize = 1024 * 1024;
}

/// `changes.push`: push the checked-out branch to its upstream; a branch
/// without one goes to [`ChangesStatus::remote`] under its own name and
/// gets it as its upstream. Progress comes as `changes.progress`.
/// Feature [`crate::compat::feature::CHANGES_SYNC`].
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ChangesPushParams {
    pub block_id: BlockId,
}

/// `changes.push`'s result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ChangesPushed {
    pub remote: String,
    pub branch: String,
    /// The branch had no upstream; it has now.
    #[serde(default)]
    pub set_upstream: bool,
    /// Nothing was new on the remote's side.
    #[serde(default)]
    pub up_to_date: bool,
}

/// `changes.progress`: what a running push, pull or fetch says.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SyncOp {
    Push,
    Pull,
    Fetch,
    #[serde(other)]
    Unknown,
}

/// Whether `gh` can open pull requests here.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GhState {
    Ready,
    /// `gh` isn't installed.
    Missing,
    /// `gh auth login` is needed.
    SignedOut,
    /// No remote of the repo is on GitHub.
    NotGithub,
    #[serde(other)]
    Unknown,
}

/// A pull request, as `gh pr view` has it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
    pub title: String,
    /// `open`, `closed` or `merged`.
    pub state: String,
    #[serde(default)]
    pub draft: bool,
    /// The branch it merges into.
    pub base: String,
}

/// `changes.pr`: the checked-out branch's pull request, or what a new one
/// would start with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ChangesPr {
    pub gh: GhState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr: Option<PullRequest>,
    /// Branches it may merge into, the default first.
    #[serde(default)]
    pub bases: Vec<String>,
    /// From the branch's commits since the default base.
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: String,
}

/// `changes.pr_create`: open a pull request of the checked-out branch with
/// `gh pr create`, pushing it first when the remote lacks commits.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ChangesPrCreateParams {
    pub block_id: BlockId,
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// The branch it merges into; omitted: the repo's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(default)]
    pub draft: bool,
}

/// `changes.conflict`: a conflicted file's conflicts, from its markers.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ChangesConflictParams {
    pub block_id: BlockId,
    pub path: String,
}

/// One `<<<<<<< … >>>>>>>` region.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ConflictRegion {
    /// Stable while the region's lines are: `changes.resolve` names it.
    pub id: String,
    /// 1-based line of its `<<<<<<<` in the work tree file.
    pub line: u32,
    /// Ours: the checked-out side (`HEAD`).
    pub ours: Vec<String>,
    /// The common ancestor's lines (`merge.conflictStyle diff3`/`zdiff3`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<Vec<String>>,
    /// Theirs: the side coming in.
    pub theirs: Vec<String>,
    /// What git named each side (`HEAD`, a branch, a commit).
    #[serde(default)]
    pub ours_label: String,
    #[serde(default)]
    pub theirs_label: String,
}

/// `changes.conflict`'s result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ConflictFile {
    pub path: String,
    /// In the file's order; empty once every region is resolved.
    pub regions: Vec<ConflictRegion>,
    /// Binary, or deleted on one side: only a whole side can be taken.
    #[serde(default)]
    pub whole: bool,
}

/// Which side a resolution takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Take {
    Ours,
    Theirs,
    /// Ours, then theirs.
    Both,
    #[serde(other)]
    Unknown,
}

/// `changes.resolve`: with `take` and `regions`, rewrite those regions of
/// the file as that side (a region gone is
/// [`crate::control::RpcError::CONFLICT`]); with `take` alone, the whole
/// file becomes that side and is marked resolved; neither: mark it
/// resolved as it is (`git add`).
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct ChangesResolveParams {
    pub block_id: BlockId,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub take: Option<Take>,
    /// Region ids from `changes.conflict`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub regions: Option<Vec<String>>,
}
