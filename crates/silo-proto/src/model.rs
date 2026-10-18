//! Session tree: sessions contain windows (tabs); a window's layout is a
//! binary split tree whose leaves are blocks.

use serde::{Deserialize, Serialize};

pub type SessionId = String;
pub type WindowId = String;
pub type BlockId = String;
pub type ClientId = u64;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TermSize {
    pub columns: u16,
    pub rows: u16,
    #[serde(default)]
    pub cell_width: u16,
    #[serde(default)]
    pub cell_height: u16,
}

impl TermSize {
    #[must_use]
    pub fn new(columns: u16, rows: u16) -> Self {
        Self { columns, rows, cell_width: 0, cell_height: 0 }
    }
}

/// `Horizontal` places children side by side; `Vertical` stacks them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Axis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Layout {
    Block {
        id: BlockId,
    },
    Split {
        axis: Axis,
        /// Fraction of the space given to `first`, in (0, 1).
        ratio: f32,
        first: Box<Layout>,
        second: Box<Layout>,
    },
}

impl Layout {
    #[must_use]
    pub fn block(id: impl Into<BlockId>) -> Self {
        Self::Block { id: id.into() }
    }

    /// Block ids in reading order (left-to-right, top-to-bottom).
    #[must_use]
    pub fn blocks(&self) -> Vec<BlockId> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<BlockId>) {
        match self {
            Self::Block { id } => out.push(id.clone()),
            Self::Split { first, second, .. } => {
                first.collect(out);
                second.collect(out);
            }
        }
    }

    #[must_use]
    pub fn contains(&self, block: &str) -> bool {
        match self {
            Self::Block { id } => id == block,
            Self::Split { first, second, .. } => first.contains(block) || second.contains(block),
        }
    }

    /// Replace leaf `target` with a split of it and `new`.
    pub fn split(&mut self, target: &str, new: BlockId, axis: Axis) -> bool {
        match self {
            Self::Block { id } if id == target => {
                let old = Self::block(id.clone());
                *self = Self::Split {
                    axis,
                    ratio: 0.5,
                    first: Box::new(old),
                    second: Box::new(Self::block(new)),
                };
                true
            }
            Self::Block { .. } => false,
            Self::Split { first, second, .. } => {
                first.split(target, new.clone(), axis) || second.split(target, new, axis)
            }
        }
    }

    /// Remove leaf `target`, collapsing its parent split. Returns `None` if
    /// the tree becomes empty.
    #[must_use]
    pub fn remove(self, target: &str) -> Option<Self> {
        match self {
            Self::Block { ref id } if id == target => None,
            Self::Block { .. } => Some(self),
            Self::Split { axis, ratio, first, second } => {
                match ((*first).remove(target), (*second).remove(target)) {
                    (Some(a), Some(b)) => Some(Self::Split { axis, ratio, first: Box::new(a), second: Box::new(b) }),
                    (Some(a), None) => Some(a),
                    (None, Some(b)) => Some(b),
                    (None, None) => None,
                }
            }
        }
    }

    /// Set the ratio of the split reached by `path` (0 = first child,
    /// 1 = second child at each level; empty = this node).
    pub fn set_ratio(&mut self, path: &[usize], ratio: f32) -> bool {
        match (self, path.split_first()) {
            (Self::Split { ratio: r, .. }, None) => {
                *r = ratio.clamp(0.05, 0.95);
                true
            }
            (Self::Split { first, second, .. }, Some((&index, rest))) => {
                if index == 0 { first.set_ratio(rest, ratio) } else { second.set_ratio(rest, ratio) }
            }
            _ => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowInfo {
    pub id: WindowId,
    #[serde(default)]
    pub name: Option<String>,
    pub layout: Layout,
    pub focused: BlockId,
    #[serde(default)]
    pub zoomed: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionInfo {
    pub id: SessionId,
    pub name: String,
    pub windows: Vec<WindowInfo>,
    pub active_window: WindowId,
    pub revision: u64,
    pub created_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BlockKind {
    Terminal,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BlockInfo {
    pub id: BlockId,
    pub session_id: SessionId,
    pub kind: BlockKind,
    pub title: String,
    #[serde(default)]
    pub cwd: Option<String>,
    /// Exit code once the process has exited (`-1` when killed by a signal).
    #[serde(default)]
    pub exited: Option<i32>,
    pub size: TermSize,
    #[serde(default)]
    pub owner: Option<ClientId>,
    #[serde(default)]
    pub pid: Option<i32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionView {
    pub session: SessionInfo,
    pub blocks: Vec<BlockInfo>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: SessionId,
    pub name: String,
    pub windows: usize,
    pub blocks: usize,
    pub attached_clients: usize,
    pub created_at: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_and_remove_collapse() {
        let mut layout = Layout::block("a");
        assert!(layout.split("a", "b".into(), Axis::Horizontal));
        assert!(layout.split("b", "c".into(), Axis::Vertical));
        assert_eq!(layout.blocks(), ["a", "b", "c"]);
        let layout = layout.remove("b").unwrap();
        assert_eq!(layout.blocks(), ["a", "c"]);
        let layout = layout.remove("a").unwrap();
        assert_eq!(layout, Layout::block("c"));
        assert!(layout.remove("c").is_none());
    }
}
