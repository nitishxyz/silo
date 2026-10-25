//! `files_show`: show the user a file in a Files block next to the agent's
//! block (docs/FILES_BLOCK.md §5), scrolled to a line, with line ranges
//! marked.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context as _, bail};
use serde_json::Value;
use silo_proto::{
    control::FilesSetParams,
    model::{BlockKind, FileHighlight, SessionView, WindowInfo},
};

/// Lines kept above the line shown, so it isn't flush with the top.
pub const CONTEXT_LINES: u64 = 5;

/// A parsed `files_show` input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Show {
    /// Absolute, lexically normalised (no `.` or `..`).
    pub path: PathBuf,
    /// 1-based.
    pub line: Option<u64>,
    pub highlights: Vec<FileHighlight>,
}

/// Read `files_show`'s input: `path` (absolute, `~/…`, or relative to
/// `cwd`), `line` and `highlight` (1-based, inclusive line ranges).
///
/// # Errors
/// A missing path, a relative one without `cwd`, line 0, a malformed or
/// too long highlight list.
pub fn parse(input: &Value, cwd: Option<&Path>, home: Option<&Path>) -> anyhow::Result<Show> {
    let raw = input["path"].as_str().map(str::trim).filter(|p| !p.is_empty()).context("path must be a non-empty string")?;
    let path = if raw == "~" || raw.starts_with("~/") {
        home.context("no home directory for ~")?.join(raw.trim_start_matches('~').trim_start_matches('/'))
    } else if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        cwd.context("path is relative and there is no working directory; give an absolute path")?.join(raw)
    };
    let line = match &input["line"] {
        Value::Null => None,
        v => Some(v.as_u64().filter(|&l| l >= 1).context("line must be a positive integer (1-based)")?),
    };
    let highlights = match &input["highlight"] {
        Value::Null => Vec::new(),
        Value::Array(items) => {
            if items.len() > FileHighlight::PER_FILE {
                bail!("at most {} highlights", FileHighlight::PER_FILE);
            }
            items.iter().map(highlight).collect::<anyhow::Result<_>>()?
        }
        _ => bail!("highlight must be an array of {{start, end, note?}}"),
    };
    Ok(Show { path: normalize(&path), line, highlights })
}

fn highlight(item: &Value) -> anyhow::Result<FileHighlight> {
    let line = |key: &str| item[key].as_u64().filter(|&l| l >= 1);
    let start = line("start").context("highlight start must be a positive integer (1-based)")?;
    let end = match &item["end"] {
        Value::Null => start,
        _ => line("end").context("highlight end must be a positive integer (1-based)")?,
    };
    let note = match &item["note"] {
        Value::Null => None,
        v => Some(v.as_str().context("highlight note must be a string")?.trim())
            .filter(|n| !n.is_empty())
            .map(|n| n.chars().take(FileHighlight::NOTE_MAX).collect()),
    };
    Ok(FileHighlight { start: start.min(end), end: start.max(end), note })
}

/// `path` without `.` and `..` components (lexically: symlinks are left
/// to the server, which checks where they lead).
pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// The root of a new Files block showing `target` (a file, or a folder
/// when `dir`): the agent's folder when `target` is inside it, so the tree
/// shows the project; otherwise the folder itself, or the file's folder.
#[must_use]
pub fn new_root(target: &Path, dir: bool, cwd: Option<&Path>) -> PathBuf {
    if let Some(cwd) = cwd.filter(|c| target.starts_with(c) && *c != Path::new("/")) {
        return cwd.to_path_buf();
    }
    if dir { target.to_path_buf() } else { target.parent().map_or_else(|| PathBuf::from("/"), Path::to_path_buf) }
}

/// `target` relative to `root`, `/`-separated; `None` outside it. The
/// root itself is the empty path.
#[must_use]
pub fn relative(root: &Path, target: &Path) -> Option<String> {
    let rest = target.strip_prefix(root).ok()?;
    Some(rest.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/"))
}

/// The window of `view` holding `block`, tiled or floating.
#[must_use]
pub fn window_of<'a>(view: &'a SessionView, block: &str) -> Option<&'a WindowInfo> {
    view.session.windows.iter().find(|w| w.layout.contains(block) || w.floats.iter().any(|f| f.id == block))
}

/// The Files block to reuse for `target`: one in `anchor`'s window whose
/// root holds `target`, the deepest such root first (it shows the least
/// unrelated tree); its id and root.
#[must_use]
pub fn reusable(view: &SessionView, anchor: &str, target: &Path) -> Option<(String, PathBuf)> {
    let window = window_of(view, anchor)?;
    let shown: Vec<String> = window.layout.blocks().into_iter().chain(window.floats.iter().map(|f| f.id.clone())).collect();
    view.blocks
        .iter()
        .filter(|b| b.kind == BlockKind::Files && shown.contains(&b.id))
        .filter_map(|b| Some((b.id.clone(), PathBuf::from(&b.files.as_ref()?.root))))
        .filter(|(_, root)| target.starts_with(root))
        .max_by_key(|(_, root)| root.components().count())
}

/// The top line (0-based) that shows 1-based `line` with
/// [`CONTEXT_LINES`] above it.
#[must_use]
pub fn top_for(line: u64) -> u64 {
    line.saturating_sub(1 + CONTEXT_LINES)
}

/// The line `show` scrolls to: `line`, else the first highlight's start.
#[must_use]
pub fn focus_line(show: &Show) -> Option<u64> {
    show.line.or_else(|| show.highlights.iter().map(|h| h.start).min())
}

/// `files.set` for showing file `rel` of `block`: its tab active, scrolled
/// to [`focus_line`], its highlights replaced (cleared when there are
/// none, so a second call doesn't leave stale marks).
#[must_use]
pub fn set_params(block: &str, rel: &str, show: &Show) -> FilesSetParams {
    FilesSetParams {
        block_id: block.to_owned(),
        active: Some(rel.to_owned()),
        scroll: focus_line(show).map(|line| [(rel.to_owned(), top_for(line))].into()),
        highlights: Some([(rel.to_owned(), show.highlights.clone())].into()),
        ..FilesSetParams::default()
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use silo_proto::model::{BlockInfo, FilesState, Layout};

    use super::*;

    fn mark(start: u64, end: u64, note: Option<&str>) -> FileHighlight {
        FileHighlight { start, end, note: note.map(str::to_owned) }
    }

    #[test]
    fn paths_resolve_against_cwd_and_home() {
        let (cwd, home) = (Path::new("/work/app"), Path::new("/Users/me"));
        let parse = |path: &str| parse(&json!({"path": path}), Some(cwd), Some(home)).map(|s| s.path);
        assert_eq!(parse("src/main.rs").unwrap(), Path::new("/work/app/src/main.rs"));
        assert_eq!(parse("./src/../Cargo.toml").unwrap(), Path::new("/work/app/Cargo.toml"));
        assert_eq!(parse("/etc/hosts").unwrap(), Path::new("/etc/hosts"));
        assert_eq!(parse("~/notes.md").unwrap(), Path::new("/Users/me/notes.md"));
        assert!(parse("  ").is_err());
        assert!(super::parse(&json!({"path": "a.rs"}), None, None).unwrap_err().to_string().contains("absolute"));
    }

    #[test]
    fn lines_and_highlights_are_checked() {
        let parse = |input: Value| parse(&input, Some(Path::new("/w")), None);
        let show =
            parse(json!({"path":"a.rs","line":12,"highlight":[{"start":9,"end":3,"note":"  why "},{"start":20},{"start":4,"end":4,"note":""}]}))
                .unwrap();
        assert_eq!(show.line, Some(12));
        assert_eq!(show.highlights, [mark(3, 9, Some("why")), mark(20, 20, None), mark(4, 4, None)]);
        assert!(parse(json!({"path":"a.rs","line":0})).is_err());
        assert!(parse(json!({"path":"a.rs","highlight":[{"start":0,"end":2}]})).is_err());
        assert!(parse(json!({"path":"a.rs","highlight":{"start":1}})).is_err());
        assert!(parse(json!({"path":"a.rs","highlight":vec![json!({"start":1}); FileHighlight::PER_FILE + 1]})).is_err());
        let long = parse(json!({"path":"a.rs","highlight":[{"start":1,"note":"x".repeat(1000)}]})).unwrap();
        assert_eq!(long.highlights[0].note.as_ref().unwrap().len(), FileHighlight::NOTE_MAX);
    }

    #[test]
    fn roots_prefer_the_agents_folder() {
        let cwd = Some(Path::new("/work/app"));
        assert_eq!(new_root(Path::new("/work/app/src/main.rs"), false, cwd), Path::new("/work/app"));
        assert_eq!(new_root(Path::new("/etc/hosts"), false, cwd), Path::new("/etc"));
        assert_eq!(new_root(Path::new("/work/other"), true, cwd), Path::new("/work/other"));
        assert_eq!(new_root(Path::new("/etc/hosts"), false, Some(Path::new("/"))), Path::new("/etc"), "never root at /");
        assert_eq!(relative(Path::new("/work/app"), Path::new("/work/app/src/main.rs")).as_deref(), Some("src/main.rs"));
        assert_eq!(relative(Path::new("/work/app"), Path::new("/work/app")).as_deref(), Some(""));
        assert_eq!(relative(Path::new("/work/app"), Path::new("/work/apple/x")), None);
    }

    #[test]
    fn scroll_shows_context_above_the_line() {
        let show = |line, highlights| Show { path: "/a".into(), line, highlights };
        assert_eq!(focus_line(&show(None, vec![mark(40, 44, None), mark(12, 13, None)])), Some(12));
        assert_eq!(focus_line(&show(Some(3), vec![mark(40, 44, None)])), Some(3), "line wins");
        assert_eq!(focus_line(&show(None, vec![])), None);
        assert_eq!((top_for(1), top_for(6), top_for(7), top_for(100)), (0, 0, 1, 94));
        let params = set_params("b1", "src/a.rs", &show(None, vec![mark(30, 31, Some("here"))]));
        assert_eq!(params.active.as_deref(), Some("src/a.rs"));
        assert_eq!(params.scroll, Some([("src/a.rs".to_owned(), 24)].into()));
        assert_eq!(params.highlights, Some([("src/a.rs".to_owned(), vec![mark(30, 31, Some("here"))])].into()));
        let plain = set_params("b1", "a.rs", &show(None, vec![]));
        assert_eq!((plain.scroll, plain.highlights), (None, Some([("a.rs".to_owned(), vec![])].into())), "old marks are cleared");
    }

    fn block(id: &str, kind: BlockKind, root: Option<&str>) -> BlockInfo {
        let mut info: BlockInfo =
            serde_json::from_value(json!({"id": id, "session_id": "s", "kind": "terminal", "title": "", "size": {"columns": 80, "rows": 24}}))
                .unwrap();
        info.kind = kind;
        info.files = root.map(|r| Box::new(FilesState::new(r.into())));
        info
    }

    #[test]
    fn reuses_a_files_block_in_the_agents_window() {
        let window = |id: &str, layout: Layout| -> WindowInfo {
            WindowInfo { id: id.into(), name: None, focused: String::new(), zoomed: false, floats: vec![], layout }
        };
        let split = |a: &str, b: &str, c: &str| Layout::Split {
            axis: silo_proto::model::Axis::Horizontal,
            ratio: 0.5,
            first: Box::new(Layout::block(a)),
            second: Box::new(Layout::Split {
                axis: silo_proto::model::Axis::Vertical,
                ratio: 0.5,
                first: Box::new(Layout::block(b)),
                second: Box::new(Layout::block(c)),
            }),
        };
        let mut view: SessionView = serde_json::from_value(
            json!({"session": {"id": "s", "name": "s", "windows": [], "active_window": "w1", "revision": 1, "created_at": 0}, "blocks": []}),
        )
        .unwrap();
        view.session.windows = vec![window("w1", split("agent", "f1", "f2")), window("w2", Layout::block("f3"))];
        view.blocks = vec![
            block("agent", BlockKind::Agent, None),
            block("f1", BlockKind::Files, Some("/work")),
            block("f2", BlockKind::Files, Some("/work/app")),
            block("f3", BlockKind::Files, Some("/work/app/src")),
        ];
        let pick = |path: &str| reusable(&view, "agent", Path::new(path)).map(|(id, _)| id);
        assert_eq!(pick("/work/app/src/main.rs").as_deref(), Some("f2"), "deepest root in the agent's window");
        assert_eq!(pick("/work/README.md").as_deref(), Some("f1"));
        assert_eq!(pick("/etc/hosts"), None);
        assert_eq!(reusable(&view, "nobody", Path::new("/work/a")), None);
    }
}
