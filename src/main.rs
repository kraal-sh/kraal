//! kraal: a terminal workspace manager for SSH fleets and coding agents.

mod accounts;
mod agent;
mod ansi;
mod buffers;
mod cli;
mod client;
mod codex_usage;
mod config;
mod context;
mod edit;
mod exec;
mod files;
mod fleet;
mod git;
mod graphics;
mod hints;
mod input;
mod integration;
mod keymap;
mod layout;
mod pane;
mod paths;
mod persist;
mod preview;
mod process;
mod protocol;
mod runbooks;
mod server;
mod servers;
mod shell;
#[cfg(test)]
mod testing;
mod text;
mod theme;
mod tools;
mod transfer;
mod watch;
mod workspace;
mod worktree_setup;

fn main() {
    cli::main();
}
