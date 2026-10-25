//! `changes_show`: show the user what the agent changed in a Changes block
//! of its own (docs/CHANGES_BLOCK.md §4), on a scope, with lines marked.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use serde_json::Value;
use silo_proto::model::{BlockKind, ChangeMark, ChangesScope, SessionView};

/// What `scope` asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Turn,
    Session,
    Working,
    Staged,
}

/// A mark before its path is made relative to the block's root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mark {
    /// Absolute.
    pub path: PathBuf,
    pub line: u32,
    pub note: Option<String>,
}

/// A parsed `changes_show` input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Show {
    /// The file to show (absolute), if any.
    pub path: Option<PathBuf>,
    pub scope: Scope,
    pub marks: Vec<Mark>,
}

fn absolute(raw: &str, cwd: Option<&Path>, home: Option<&Path>) -> anyhow::Result<PathBuf> {
    let path = if raw == "~" || raw.starts_with("~/") {
        home.context("no home directory for ~")?.join(raw.trim_start_matches('~').trim_start_matches('/'))
    } else if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        cwd.context("path is relative and there is no working directory; give an absolute path")?.join(raw)
    };
    Ok(crate::files::normalize(&path))
}

/// Read `changes_show`'s input: `path?`, `scope?` (`turn`, the default,
/// `session`, `working`, `staged`) and `hunks?` (`[{path, line, note?}]`,
/// lines on the new side, 1-based). Paths are absolute, `~/…` or relative
/// to `cwd`.
///
/// # Errors
/// A bad scope, path, line, or too many marks.
pub fn parse(input: &Value, cwd: Option<&Path>, home: Option<&Path>) -> anyhow::Result<Show> {
    let path = match &input["path"] {
        Value::Null => None,
        v => Some(absolute(v.as_str().map(str::trim).filter(|p| !p.is_empty()).context("path must be a non-empty string")?, cwd, home)?),
    };
    let scope = match input["scope"].as_str() {
        None | Some("turn") => Scope::Turn,
        Some("session") => Scope::Session,
        Some("working") => Scope::Working,
        Some("staged") => Scope::Staged,
        Some(other) => bail!("scope must be turn, session, working or staged, not {other}"),
    };
    let marks = match &input["hunks"] {
        Value::Null => Vec::new(),
        Value::Array(items) => {
            if items.len() > ChangeMark::MAX {
                bail!("at most {} hunks", ChangeMark::MAX);
            }
            items
                .iter()
                .map(|item| {
                    let path = item["path"].as_str().map(str::trim).filter(|p| !p.is_empty()).context("each hunk needs a path")?;
                    let line = item["line"].as_u64().filter(|&l| l >= 1).and_then(|l| u32::try_from(l).ok());
                    let note = match &item["note"] {
                        Value::Null => None,
                        v => Some(v.as_str().context("a hunk's note must be a string")?.trim())
                            .filter(|n| !n.is_empty())
                            .map(|n| n.chars().take(ChangeMark::NOTE_MAX).collect()),
                    };
                    Ok(Mark { path: absolute(path, cwd, home)?, line: line.context("a hunk's line must be a positive integer (1-based)")?, note })
                })
                .collect::<anyhow::Result<_>>()?
        }
        _ => bail!("hunks must be an array of {{path, line, note?}}"),
    };
    Ok(Show { path, scope, marks })
}

/// The block's scope for `scope`: a turn or the session compares the tree
/// otto snapshotted at its start (`turn_base` / `session_base`, given by
/// the host) with the work tree now; without one, the work tree against
/// `HEAD`. The second value says whether it fell back.
#[must_use]
pub fn block_scope(scope: Scope, turn_base: Option<&str>, session_base: Option<&str>) -> (ChangesScope, bool) {
    let tree = |base: Option<&str>, label: &str| match base.filter(|b| !b.is_empty()) {
        Some(base) => (ChangesScope::Tree { base: base.to_owned(), head: None, label: label.to_owned() }, false),
        None => (ChangesScope::Working, true),
    };
    match scope {
        Scope::Turn => tree(turn_base, "This turn"),
        Scope::Session => tree(session_base, "This session"),
        Scope::Working => (ChangesScope::Working, false),
        Scope::Staged => (ChangesScope::Staged, false),
    }
}

/// The agent's own Changes block in `view`: one linked to block `agent`;
/// its id, root and window.
#[must_use]
pub fn own_block(view: &SessionView, agent: &str) -> Option<(String, PathBuf, Option<String>)> {
    view.blocks.iter().filter(|b| b.kind == BlockKind::Changes).find_map(|b| {
        let state = b.changes.as_deref()?;
        let linked = state.agent.as_ref().is_some_and(|a| a.block_id.as_deref() == Some(agent));
        let window = crate::files::window_of(view, &b.id).map(|w| w.id.clone());
        linked.then(|| (b.id.clone(), PathBuf::from(&state.root), window))
    })
}

/// Marks relative to `root`; marks outside it are left out.
#[must_use]
pub fn marks_under(root: &Path, marks: &[Mark]) -> Vec<ChangeMark> {
    marks
        .iter()
        .filter_map(|m| {
            Some(ChangeMark { path: crate::files::relative(root, &m.path).filter(|p| !p.is_empty())?, line: m.line, note: m.note.clone() })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use silo_proto::model::{AgentLink, BlockInfo, ChangesState, Layout, WindowInfo};

    use super::*;

    #[test]
    fn input_is_checked() {
        let (cwd, home) = (Some(Path::new("/work/app")), Some(Path::new("/Users/me")));
        let show = parse(&json!({}), cwd, home).unwrap();
        assert_eq!((show.path, show.scope, show.marks.len()), (None, Scope::Turn, 0), "the current turn by default");
        let show = parse(
            &json!({"path": "src/a.rs", "scope": "staged", "hunks": [{"path": "./src/a.rs", "line": 12, "note": "  the risky part "}, {"path": "~/x.rs", "line": 1, "note": ""}]}),
            cwd,
            home,
        )
        .unwrap();
        assert_eq!((show.path.as_deref(), show.scope), (Some(Path::new("/work/app/src/a.rs")), Scope::Staged));
        assert_eq!(show.marks[0], Mark { path: "/work/app/src/a.rs".into(), line: 12, note: Some("the risky part".into()) });
        assert_eq!((show.marks[1].path.as_path(), show.marks[1].note.as_deref()), (Path::new("/Users/me/x.rs"), None));
        for bad in [
            json!({"scope": "yesterday"}),
            json!({"path": ""}),
            json!({"hunks": [{"path": "a.rs", "line": 0}]}),
            json!({"hunks": [{"line": 3}]}),
            json!({"hunks": {"path": "a.rs", "line": 1}}),
            json!({"hunks": vec![json!({"path": "a.rs", "line": 1}); ChangeMark::MAX + 1]}),
        ] {
            assert!(parse(&bad, cwd, home).is_err(), "{bad}");
        }
        assert!(parse(&json!({"path": "a.rs"}), None, None).unwrap_err().to_string().contains("absolute"));
        let long = parse(&json!({"hunks": [{"path": "/a", "line": 1, "note": "x".repeat(1000)}]}), None, None).unwrap();
        assert_eq!(long.marks[0].note.as_ref().unwrap().len(), ChangeMark::NOTE_MAX);
    }

    #[test]
    fn scopes_use_otto_s_snapshots_when_there_are_some() {
        let tree = |base: &str, label: &str| ChangesScope::Tree { base: base.into(), head: None, label: label.into() };
        assert_eq!(block_scope(Scope::Turn, Some("t1"), Some("s1")), (tree("t1", "This turn"), false));
        assert_eq!(block_scope(Scope::Session, Some("t1"), Some("s1")), (tree("s1", "This session"), false));
        assert_eq!(block_scope(Scope::Turn, None, None), (ChangesScope::Working, true), "no snapshot: the work tree");
        assert_eq!(block_scope(Scope::Turn, Some(""), None), (ChangesScope::Working, true));
        assert_eq!(block_scope(Scope::Staged, None, None), (ChangesScope::Staged, false));
    }

    #[test]
    fn finds_the_agent_s_own_block_and_marks_under_its_root() {
        let block = |id: &str, kind: BlockKind, linked: Option<&str>| {
            let mut info: BlockInfo =
                serde_json::from_value(json!({"id": id, "session_id": "s", "kind": "terminal", "title": "", "size": {"columns": 80, "rows": 24}}))
                    .unwrap();
            info.kind = kind;
            if kind == BlockKind::Changes {
                let mut state = ChangesState::new(format!("/work/{id}"));
                state.agent = linked.map(|b| AgentLink { otto_session: "ses".into(), block_id: Some(b.into()) });
                info.changes = Some(Box::new(state));
            }
            info
        };
        let mut view: SessionView = serde_json::from_value(
            json!({"session": {"id": "s", "name": "s", "windows": [], "active_window": "w1", "revision": 1, "created_at": 0}, "blocks": []}),
        )
        .unwrap();
        let window = |id: &str, block: &str| WindowInfo {
            id: id.into(),
            name: None,
            focused: String::new(),
            zoomed: false,
            floats: vec![],
            layout: Layout::block(block),
        };
        view.session.windows = vec![window("w1", "agent"), window("w2", "c2"), window("w3", "c1")];
        view.blocks = vec![
            block("agent", BlockKind::Agent, None),
            block("c1", BlockKind::Changes, Some("other-agent")),
            block("c2", BlockKind::Changes, Some("agent")),
        ];
        assert_eq!(own_block(&view, "agent"), Some(("c2".into(), "/work/c2".into(), Some("w2".into()))), "linked to it, in any window");
        assert_eq!(own_block(&view, "nobody"), None);
        let marks = [Mark { path: "/work/c2/src/a.rs".into(), line: 3, note: None }, Mark { path: "/elsewhere/b.rs".into(), line: 1, note: None }];
        assert_eq!(marks_under(Path::new("/work/c2"), &marks), [ChangeMark { path: "src/a.rs".into(), line: 3, note: None }]);
    }
}
