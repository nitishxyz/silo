//! The tool model (docs/OTTO.md §5): the [`Tool`] trait, specs with effects
//! and presentations, outputs, and [`ToolSource`]s (builtin, silo, MCP, ext).

pub mod builtin;
pub mod native;
pub mod output;
pub mod policy;
pub mod registry;
pub mod spec;

pub use spec::{
    Approver, CallMode, Effects, Loading, Mailbox, NativeTool, OutputPart, Presentation, Progress, ProjectSources, Source, Tool, ToolCx, ToolError,
    ToolErrorKind, ToolOutput, ToolSource, ToolSpec, Truncation,
};
