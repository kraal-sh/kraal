//! Agent accounts: several subscriptions per coding agent, each in its own config directory.
//!
//! An account is a config home for one agent (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`), logged in with
//! the agent's own login command. kraal never reads credentials: it only chooses which home a pane
//! starts with, and reads the usage the agents report (Claude's status line, Codex's app-server).
//! `~/.config/kraal/accounts.toml` lists accounts, context rules and usage thresholds.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

mod conversations;
mod home;
mod identity;
mod launch;
mod selection;
mod store;
mod usage;

pub use conversations::{carry_conversation, check_carry, check_resume, conversation_exists};
pub use home::{absolute_home, default_home_for, lock_login, prepare_home};
pub use identity::{agent_names, existing_label, key, label, missing_account, rename, review_question};
pub use launch::{Launch, duplicate_args, launch, launch_args, login, login_status, resumable_args, resume_argv};
pub use usage::{
    Usage, UsageMap, Window, describe_reset, limit_from_screen, now, save_usage, usage_from_statusline, usage_text,
};

/// How kraal runs one kind of agent.
#[derive(Debug, Clone, Copy)]
pub struct AgentKind {
    /// The agent's name in accounts.toml, in account keys ([`key`]) and on the command line.
    pub name: &'static str,
    /// The program that runs it.
    pub command: &'static str,
    /// Environment variable selecting the config home.
    pub home_env: &'static str,
    /// The home the agent uses when that variable is unset: the account the user had before
    /// kraal, which needs no variable.
    pub(crate) default_home: &'static str,
    /// Arguments of the agent's own login command.
    pub login: &'static [&'static str],
    /// Arguments of its command that says whether an account is logged in (exit status).
    pub login_status: &'static [&'static str],
    /// Arguments that resume a conversation; `{session}` is replaced by its id.
    pub(crate) resume: &'static [&'static str],
    /// Flag that sets the id of a new conversation, so kraal knows it without reading files.
    pub session_flag: Option<&'static str>,
    /// Entries of the default home shared with every other account (symlinked).
    pub shared: &'static [&'static str],
    /// Entries copied once into a new account home.
    pub(crate) copied: &'static [&'static str],
    /// Variables that would override the account's login; cleared in the pane.
    pub clear_env: &'static [&'static str],
}

pub fn agents() -> impl Iterator<Item = &'static AgentKind> + Clone {
    crate::agent_registry::AGENTS.iter().filter_map(|a| a.accounts.as_ref())
}

pub fn agent_kind(name: &str) -> Option<&'static AgentKind> {
    crate::agent_registry::find(name)?.accounts.as_ref()
}

/// What kraal does when an account reaches its usage threshold (`[limits] mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Tell the user when an account crosses its threshold.
    #[default]
    Notify,
    /// Offer to continue on another account once the agent finished its turn.
    Ask,
    /// Continue on another account automatically once the agent finished its turn.
    Auto,
}

/// `[limits]` of accounts.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Limits {
    /// Usage (percent of the 5-hour or weekly window) at which an account counts as full.
    #[serde(default = "default_threshold")]
    pub threshold: u8,
    /// What happens when an account reaches the threshold.
    #[serde(default)]
    pub mode: Mode,
}

fn default_threshold() -> u8 {
    90
}

impl Default for Limits {
    fn default() -> Self {
        Limits { threshold: default_threshold(), mode: Mode::Notify }
    }
}

/// An `[[account]]` of accounts.toml: one subscription of one agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// Stable account identity.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id: String,
    /// Unique among the agent's accounts.
    pub name: String,
    /// The [`AgentKind`] it is for, by name.
    pub agent: String,
    /// Config home; the agent's default home means "the account you already have".
    pub home: String,
    /// This account's own threshold, instead of `[limits]`'s.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<u8>,
    /// The user's own text, which kraal keeps when it rewrites the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// A login was started but has not yet been verified by the agent.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sign_in_required: bool,
    /// Changes on every login attempt, so completed logins also invalidate identity checks.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub login_revision: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub review: bool,
    /// A name collision may add a suffix to the label; the identity shown in Review must stay exact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_identity: Option<String>,
    /// Old references and homes stay resolvable after accounts are joined or forgotten.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linked_to: Option<String>,
    /// Stored under their owner so older readers see only real accounts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub references: Vec<Account>,
}

#[cfg(test)]
thread_local! {
    pub(crate) static HOME_CHECKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Which accounts to use, in order, for a workspace or a directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Rule {
    /// The rule applies in this workspace only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    workspace: Option<String>,
    /// The rule applies to agents started in this directory or below it (`~` allowed); a
    /// worktree counts as its repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    /// The rule applies to this agent only; `None`: to every agent ([`Accounts::rule_accounts`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    agent: Option<String>,
    /// Readable names for older servers, in order. Empty forbids every account.
    accounts: Vec<String>,
    /// Authoritative references once bound; older servers keep using `accounts`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account_ids: Option<Vec<String>>,
}

/// `[claude]` of accounts.toml.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClaudeOptions {
    /// The user's own status line command, run after kraal's (kraal records usage first).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statusline: Option<String>,
}

impl ClaudeOptions {
    fn is_empty(&self) -> bool {
        self.statusline.is_none()
    }
}

/// accounts.toml as loaded, saved back to the file it came from.
#[derive(Debug, Clone)]
pub struct Accounts {
    pub limits: Limits,
    pub claude: ClaudeOptions,
    /// Includes the hidden references retained for older panes and clients.
    accounts: Vec<Account>,
    /// In order: the first rule that matches decides.
    rules: Vec<Rule>,
    path: PathBuf,
    /// The file as it was read, which a save edits; `None` when there was none.
    source: Option<String>,
}

#[cfg(test)]
mod tests;
