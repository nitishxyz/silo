//! The `changes.*` wire types the view model reads (docs/CHANGES_BLOCK.md §2).

pub use silo_proto::{
    changes::{ChangedFile, DiffLine, FileDiff, FileStatus, Hunk, LineKind},
    model::{ReviewComment, Side},
};
