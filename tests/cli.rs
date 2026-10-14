//! End-to-end tests: a real server driven through the CLI, in isolated directories.

mod common;

#[path = "cli/accounts.rs"]
mod accounts;
#[path = "cli/agents.rs"]
mod agents;
#[path = "cli/buffers.rs"]
mod buffers;
#[path = "cli/config.rs"]
mod config;
#[path = "cli/events.rs"]
mod events;
#[path = "cli/input.rs"]
mod input;
#[path = "cli/panes.rs"]
mod panes;
#[path = "cli/production.rs"]
mod production;
#[path = "cli/remote.rs"]
mod remote;
#[path = "cli/rendering.rs"]
mod rendering;
#[path = "cli/runbooks.rs"]
mod runbooks;
#[path = "cli/sessions.rs"]
mod sessions;
#[path = "cli/support.rs"]
mod support;
#[path = "cli/updates.rs"]
mod updates;
#[path = "cli/workspaces.rs"]
mod workspaces;
#[path = "cli/worktrees.rs"]
mod worktrees;
