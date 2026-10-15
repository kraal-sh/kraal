//! Workspace definitions loaded from KDL files.
//!
//! A workspace file is a Zellij layout, so existing `~/.config/zellij/layouts/*.kdl`
//! files work unchanged. Zellij writes KDL v1; KDL v2 is accepted too. Only the
//! subset kraal needs is interpreted: anything else is skipped and reported in
//! [`WorkspaceDef::warnings`] rather than failing the load.
//!
//! kraal extends panes with two properties: `preview="notes.md"` (show a rendered
//! file instead of a terminal) and `agent="claude"` (the pane runs a coding agent).
//! Zellij rejects unknown properties, so files using them are kraal-only. Stacks are
//! Zellij's: `pane stacked=true { ... }`, with `expanded=true` on the pane open in it.

pub mod catalog;
mod files;
mod model;
mod read;
mod snapshot;
mod write;

pub use files::*;
pub use model::*;
pub use read::*;
pub use snapshot::from_snapshot;
pub use write::*;

#[cfg(test)]
mod tests;
