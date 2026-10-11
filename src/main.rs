//! kraal — a terminal workspace manager for SSH fleets and coding agents.

mod accounts;
mod agent;
mod ansi;
mod cli;
mod client;
mod codex_usage;
mod config;
mod context;
mod exec;
mod files;
mod fleet;
mod git;
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
mod workspace;

fn main() {
    cli::main();
}
