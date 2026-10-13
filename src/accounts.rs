//! Agent accounts: several subscriptions per coding agent, each in its own config directory.
//!
//! An account is a config home for one agent (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`), logged in with
//! the agent's own login command. kraal never reads credentials: it only chooses which home a pane
//! starts with, and reads the usage the agents report (Claude's status line, Codex's app-server).
//! `~/.config/kraal/accounts.toml` lists accounts, context rules and usage thresholds.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::paths;

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
    default_home: &'static str,
    /// Arguments of the agent's own login command.
    pub login: &'static [&'static str],
    /// Arguments of its command that says whether an account is logged in (exit status).
    pub login_status: &'static [&'static str],
    /// Arguments that resume a conversation; `{session}` is replaced by its id.
    resume: &'static [&'static str],
    /// Flag that sets the id of a new conversation, so kraal knows it without reading files.
    pub session_flag: Option<&'static str>,
    /// Entries of the default home shared with every other account (symlinked).
    pub shared: &'static [&'static str],
    /// Entries copied once into a new account home.
    copied: &'static [&'static str],
    /// Variables that would override the account's login; cleared in the pane.
    clear_env: &'static [&'static str],
}

/// The agents whose accounts kraal manages; other agents run with their own single home.
pub const AGENTS: &[AgentKind] = &[
    AgentKind {
        name: "claude",
        command: "claude",
        home_env: "CLAUDE_CONFIG_DIR",
        default_home: "~/.claude",
        login: &["auth", "login"],
        login_status: &["auth", "status"],
        resume: &["--resume", "{session}"],
        session_flag: Some("--session-id"),
        shared: &[
            "CLAUDE.md",
            "settings.json",
            "skills",
            "agents",
            "commands",
            "output-styles",
            "plugins",
            "projects",
            "plans",
        ],
        copied: &[],
        clear_env: &["ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN", "CLAUDE_CODE_OAUTH_TOKEN"],
    },
    AgentKind {
        name: "codex",
        command: "codex",
        home_env: "CODEX_HOME",
        default_home: "~/.codex",
        login: &["login"],
        login_status: &["login", "status"],
        resume: &["resume", "{session}"],
        session_flag: None,
        // Codex keeps databases and locks in its home: nothing is shared.
        shared: &[],
        copied: &["config.toml"],
        clear_env: &["OPENAI_API_KEY", "CODEX_API_KEY"],
    },
];

pub fn agent_kind(name: &str) -> Option<&'static AgentKind> {
    AGENTS.iter().find(|a| a.name == name)
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
    /// Unique among the agent's accounts. `default` names the agent's default home only.
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
    /// Account names to try, in order. Empty: no account may be used where the rule applies.
    accounts: Vec<String>,
}

/// `[claude]` of accounts.toml.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClaudeOptions {
    /// The user's own status line command, run after kraal's (kraal records usage first).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub statusline: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    limits: Limits,
    #[serde(default)]
    claude: ClaudeOptions,
    #[serde(default, rename = "account")]
    accounts: Vec<Account>,
    #[serde(default, rename = "rule")]
    rules: Vec<Rule>,
}

const HEADER: &str = "# kraal agent accounts. One [[account]] per subscription: a name, the agent (claude, codex)\n\
# and its config home. [[rule]] picks accounts by workspace or directory, in order; [limits] sets\n\
# the usage threshold (percent) and what happens when an account reaches it: notify, ask or auto.\n\n";

/// accounts.toml as loaded, saved back to the file it came from.
#[derive(Debug, Clone)]
pub struct Accounts {
    pub limits: Limits,
    pub claude: ClaudeOptions,
    /// The accounts as listed; [`Accounts::for_agent`] adds the implicit default ones.
    accounts: Vec<Account>,
    /// In order: the first rule that matches decides.
    rules: Vec<Rule>,
    path: PathBuf,
    /// The file as it was read, which a save edits; `None` when there was none.
    source: Option<String>,
}

impl Accounts {
    pub fn path() -> PathBuf {
        paths::config_dir().join("accounts.toml")
    }

    pub fn load() -> Result<Accounts> {
        Self::load_from(&Self::path())
    }

    pub fn load_from(path: &Path) -> Result<Accounts> {
        Self::read(path).map(|(accounts, _)| accounts)
    }

    /// What `kraal config check` says of the accounts file at `path`: a warning for each key
    /// that kraal does not read, an error when the file cannot be read.
    pub fn check(path: &Path) -> Result<Vec<String>> {
        Self::read(path).map(|(_, warnings)| warnings)
    }

    fn read(path: &Path) -> Result<(Accounts, Vec<String>)> {
        let source = match std::fs::read_to_string(path) {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        let (file, warnings) = match &source {
            Some(text) => {
                crate::unknown_keys::from_toml::<File>(text).with_context(|| format!("parsing {}", path.display()))?
            }
            None => Default::default(),
        };
        let accounts = Accounts {
            limits: file.limits,
            claude: file.claude,
            accounts: file.accounts,
            rules: file.rules,
            path: path.to_path_buf(),
            source,
        };
        Ok((accounts, warnings))
    }

    pub fn empty() -> Accounts {
        Accounts {
            limits: Limits::default(),
            claude: ClaudeOptions::default(),
            accounts: Vec::new(),
            rules: Vec::new(),
            path: Self::path(),
            source: None,
        }
    }

    /// What a save writes: the file as it was read with what changed since edited in, so that
    /// the user's comments and the keys another kraal reads stay; a new file whole.
    fn text(&self) -> Result<String> {
        let file = File {
            limits: self.limits.clone(),
            claude: self.claude.clone(),
            accounts: self.accounts.clone(),
            rules: self.rules.clone(),
        };
        match &self.source {
            Some(text) => {
                crate::rewrite::rewrite(text, &file).with_context(|| format!("editing {}", self.path.display()))
            }
            None => Ok(format!("{HEADER}{}", toml::to_string_pretty(&file)?)),
        }
    }

    pub fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        paths::write_atomic(&self.path, self.text()?.as_bytes())
            .with_context(|| format!("writing {}", self.path.display()))
    }

    /// Load the accounts, change them with `f` and save them if they changed, with no other kraal
    /// process updating them in between.
    pub fn update<T>(f: impl FnOnce(&mut Accounts) -> Result<T>) -> Result<T> {
        Self::update_at(&Self::path(), f)
    }

    /// [`Accounts::update`] on the accounts file at `path`.
    pub fn update_at<T>(path: &Path, f: impl FnOnce(&mut Accounts) -> Result<T>) -> Result<T> {
        paths::locked(path, || {
            let mut all = Self::load_from(path)?;
            let before = all.text()?;
            let out = f(&mut all)?;
            if all.text()? != before {
                all.save()?;
            }
            Ok(out)
        })?
    }

    /// Accounts of an agent, with an implicit "default" account for the agent's default home
    /// when no listed account uses it.
    pub fn for_agent(&self, agent: &str) -> Vec<Account> {
        let mut out: Vec<Account> = self.accounts.iter().filter(|a| a.agent == agent).cloned().collect();
        if let Some(kind) = agent_kind(agent) {
            let default = paths::expand_tilde(kind.default_home);
            // A listed account named "default" is not hidden behind the implicit one: kraal
            // reserves the name since 0.1.0, and an accounts.toml from before may still have one.
            if !out.iter().any(|a| paths::expand_tilde(&a.home) == default || a.name == "default") {
                out.insert(
                    0,
                    Account {
                        name: "default".into(),
                        agent: agent.into(),
                        home: kind.default_home.into(),
                        threshold: None,
                        note: None,
                    },
                );
            }
        }
        out
    }

    /// Whether no account is listed: there are only the agents' own homes, each as `default`.
    pub fn none_listed(&self) -> bool {
        self.accounts.is_empty()
    }

    /// Every account of every known agent.
    pub fn all(&self) -> Vec<Account> {
        AGENTS.iter().flat_map(|k| self.for_agent(k.name)).collect()
    }

    pub fn get(&self, agent: &str, name: &str) -> Option<Account> {
        self.for_agent(agent).into_iter().find(|a| a.name == name)
    }

    /// The account whose home is `home` (used to tell which account a status line belongs to).
    pub fn by_home(&self, agent: &str, home: &Path) -> Option<Account> {
        let home = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
        self.for_agent(agent).into_iter().find(|a| {
            let h = paths::expand_tilde(&a.home);
            std::fs::canonicalize(&h).unwrap_or(h) == home
        })
    }

    /// The account an agent started by [`launch`] runs on, from what its pane keeps: the account
    /// `key` names, else, when none has that name any more (it was renamed), the one in the home
    /// `env` gave the agent.
    pub fn launched_on(&self, key: &str, env: &[(String, String)]) -> Option<Account> {
        let (agent, name) = key.split_once(':')?;
        if let Some(a) = self.get(agent, name) {
            return Some(a);
        }
        let kind = agent_kind(agent)?;
        let home = match env.iter().find(|(k, _)| k == kind.home_env) {
            Some((_, home)) => PathBuf::from(home),
            None => paths::expand_tilde(kind.default_home),
        };
        self.by_home(agent, &home)
    }

    pub fn threshold(&self, a: &Account) -> u8 {
        a.threshold.unwrap_or(self.limits.threshold)
    }

    pub fn add(&mut self, mut account: Account) -> Result<()> {
        let kind = agent_kind(&account.agent)
            .with_context(|| format!("unknown agent {:?} (known: {})", account.agent, agent_names()))?;
        account.home = absolute_home(&account.home)?;
        check_name(kind, &account.name, &account.home)?;
        if self.for_agent(kind.name).iter().any(|a| a.name == account.name && a.name != "default") {
            bail!("{} already has an account named {:?}", kind.name, account.name);
        }
        // Naming the default home adopts it instead of adding a second entry.
        self.accounts.retain(|a| !(a.agent == account.agent && a.name == account.name));
        self.accounts.push(account);
        Ok(())
    }

    pub fn remove(&mut self, agent: &str, name: &str) -> bool {
        let before = self.accounts.len();
        self.accounts.retain(|a| !(a.agent == agent && a.name == name));
        // A rule for every agent keeps the name while another agent still has an account so
        // named. When it then names none of this agent's accounts, a rule for this agent alone,
        // naming no account, goes before it: otherwise this agent would fall back to any of its
        // accounts where the rule used to restrict it.
        let shared = self.all().iter().any(|a| a.name == name);
        let mine: Vec<String> = self.for_agent(agent).into_iter().map(|a| a.name).collect();
        let mut refusals = Vec::new();
        for (i, r) in self.rules.iter_mut().enumerate() {
            if r.agent.as_deref() == Some(agent) || (r.agent.is_none() && !shared) {
                r.accounts.retain(|n| n != name);
            } else if r.agent.is_none()
                && r.accounts.iter().any(|n| n == name)
                && !r.accounts.iter().any(|n| mine.contains(n))
            {
                refusals.push((i, Rule { agent: Some(agent.to_string()), accounts: Vec::new(), ..r.clone() }));
            }
        }
        for (i, rule) in refusals.into_iter().rev() {
            self.rules.insert(i, rule);
        }
        before != self.accounts.len()
    }

    /// Account `name` of `agent`, when it can be renamed; an error says why not.
    pub fn renamable(&self, agent: &str, name: &str) -> Result<Account> {
        let a = self.get(agent, name).with_context(|| format!("{agent} has no account named {name:?}"))?;
        if !self.accounts.contains(&a) {
            bail!(
                "{} is {agent}'s own home ({}): it is always called \"default\" and cannot be renamed",
                label(&a),
                a.home
            );
        }
        Ok(a)
    }

    /// Rename account `old` of `agent` to `new`, in the rules too. Its home stays where it is.
    fn rename(&mut self, agent: &str, old: &str, new: &str) -> Result<()> {
        let account = self.renamable(agent, old)?;
        let kind = agent_kind(agent).with_context(|| format!("unknown agent {agent:?} (known: {})", agent_names()))?;
        check_name(kind, new, &account.home)?;
        if self.get(agent, new).is_some() {
            bail!("{agent} already has an account named {new:?}");
        }
        for a in self.accounts.iter_mut().filter(|a| a.agent == agent && a.name == old) {
            a.name = new.to_string();
        }
        self.rename_in_rules(agent, old, new);
        Ok(())
    }

    /// Rules that name account `old` of `agent` name `new` instead. A rule for every agent names
    /// the other agents' accounts of the same names too: when one of them has an account named
    /// `old` or `new`, that rule stays theirs (without `old` if none of them has it), and this
    /// agent gets a rule of its own just before it.
    fn rename_in_rules(&mut self, agent: &str, old: &str, new: &str) {
        let others: Vec<String> = self.all().into_iter().filter(|a| a.agent != agent).map(|a| a.name).collect();
        let theirs = |name: &str| others.iter().any(|n| n == name);
        let split = theirs(old) || theirs(new);
        let renamed = |names: &[String]| -> Vec<String> {
            names.iter().map(|n| if n == old { new.to_string() } else { n.clone() }).collect()
        };
        let mut own = Vec::new();
        for (i, r) in self.rules.iter_mut().enumerate() {
            if !r.accounts.iter().any(|n| n == old) || r.agent.as_deref().is_some_and(|a| a != agent) {
                continue;
            }
            if r.agent.is_some() || !split {
                r.accounts = renamed(&r.accounts);
                continue;
            }
            own.push((i, Rule { agent: Some(agent.to_string()), accounts: renamed(&r.accounts), ..r.clone() }));
            if !theirs(old) {
                r.accounts.retain(|n| n != old);
            }
        }
        for (i, rule) in own.into_iter().rev() {
            self.rules.insert(i, rule);
        }
    }

    /// Accounts to try, in order, for an agent started in `workspace` and `cwd`.
    fn candidates(&self, agent: &str, workspace: Option<&str>, cwd: Option<&Path>) -> Vec<Account> {
        self.rule_accounts(agent, workspace, cwd).unwrap_or_else(|| self.for_agent(agent))
    }

    /// The accounts of the first rule matching this context, if any. A rule for every agent
    /// concerns an agent when it names one of its accounts, or names no account of any agent
    /// any more: then nothing is allowed here (rather than any account at all).
    fn rule_accounts(&self, agent: &str, workspace: Option<&str>, cwd: Option<&Path>) -> Option<Vec<Account>> {
        let all = self.for_agent(agent);
        let everyone = self.all();
        for rule in &self.rules {
            if rule.agent.as_deref().is_some_and(|a| a != agent) {
                continue;
            }
            let ws_ok = rule.workspace.as_deref().is_none_or(|w| Some(w) == workspace);
            // A worktree of a repository counts as the repository (see `undecided`).
            let path_ok = rule.path.as_deref().is_none_or(|p| {
                let p = paths::expand_tilde(p);
                cwd.is_some_and(|c| {
                    c.starts_with(&p) || crate::git::known_project_path(c).is_some_and(|k| k.starts_with(&p))
                })
            });
            if rule.workspace.is_none() && rule.path.is_none() {
                continue;
            }
            if ws_ok && path_ok {
                let picked: Vec<Account> =
                    rule.accounts.iter().filter_map(|n| all.iter().find(|a| &a.name == n).cloned()).collect();
                let dangling = !rule.accounts.iter().any(|n| everyone.iter().any(|a| &a.name == n));
                if !picked.is_empty() || rule.agent.is_some() || dangling {
                    return Some(picked);
                }
            }
        }
        None
    }

    /// Whether a rule could apply to `cwd` once git says which repository it belongs to (a
    /// linked worktree, wherever it is, counts as its repository): then ask git first, off the
    /// event loop, rather than choose among accounts a rule may forbid there.
    pub fn undecided(&self, agent: &str, cwd: Option<&Path>) -> bool {
        let Some(c) = cwd else { return false };
        if crate::git::known_project_path(c).is_some() {
            return false;
        }
        self.rules.iter().any(|r| {
            r.agent.as_deref().is_none_or(|a| a == agent)
                && r.path.as_deref().is_some_and(|p| !c.starts_with(paths::expand_tilde(p)))
        })
    }

    /// The account to start with: the first candidate under its threshold, else the one that
    /// frees up first.
    pub fn choose(
        &self,
        agent: &str,
        workspace: Option<&str>,
        cwd: Option<&Path>,
        usage: &UsageMap,
        now: i64,
    ) -> Option<Account> {
        let candidates = self.candidates(agent, workspace, cwd);
        if let Some(a) = candidates.iter().find(|a| usage.peak(a, now) < self.threshold(a) as f64) {
            return Some(a.clone());
        }
        candidates.into_iter().min_by_key(|a| usage.get(a).and_then(|u| u.next_reset(now)).unwrap_or(i64::MAX))
    }

    /// Another account of the same agent to continue on, under its threshold. When a rule
    /// covers this context, only its accounts are considered: work never moves to an account
    /// the rule does not list.
    pub fn alternative(
        &self,
        current: &Account,
        workspace: Option<&str>,
        cwd: Option<&Path>,
        usage: &UsageMap,
        now: i64,
    ) -> Option<Account> {
        self.candidates(&current.agent, workspace, cwd)
            .into_iter()
            .find(|a| a.name != current.name && usage.peak(a, now) < self.threshold(a) as f64)
    }
}

pub fn agent_names() -> String {
    AGENTS.iter().map(|a| a.name).collect::<Vec<_>>().join(", ")
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Refuse `name` for an account of `kind` in `home`: one kraal cannot use, or `default` for
/// another home than the agent's own.
fn check_name(kind: &AgentKind, name: &str, home: &str) -> Result<()> {
    if !valid_name(name) {
        bail!("invalid account name {name:?} (letters, digits, - _ . only)");
    }
    if name == "default" && paths::expand_tilde(home) != paths::expand_tilde(kind.default_home) {
        bail!("\"default\" is the account in {}; give this one another name", kind.default_home);
    }
    Ok(())
}

/// Rename account `old` of `agent` to `new` in accounts.toml, with the rules that name it and
/// its usage. Its home stays where it is: agents running on it go on.
pub fn rename(agent: &str, old: &str, new: &str) -> Result<()> {
    rename_in(&Accounts::path(), &usage_dir(), agent, old, new)
}

/// [`rename`] in the accounts file `file`, with the usage saved in `usage`.
fn rename_in(file: &Path, usage: &Path, agent: &str, old: &str, new: &str) -> Result<()> {
    Accounts::update_at(file, |all| {
        all.rename(agent, old, new)?;
        move_usage(usage, agent, old, new)
    })
}

/// How an account is shown, e.g. `claude·pro`.
pub fn label(a: &Account) -> String {
    format!("{}·{}", a.agent, a.name)
}

/// An account's identity outside accounts.toml: `agent:name`. It is saved with each pane an
/// account runs in (`PaneSpec.account`, restored after a restart), given to its agent as
/// `KRAAL_ACCOUNT`, reported in agent events, and keys the [`UsageMap`]. Readers split it at the
/// first `:`, which neither an agent's name nor an account's ([`valid_name`]) holds: the format
/// must not change.
pub fn key(agent: &str, name: &str) -> String {
    format!("{agent}:{name}")
}

/// `home` as an account keeps it: a relative path means where the command was typed, and the
/// server, which runs elsewhere, must find the same directory. `~/…` stays as written.
pub fn absolute_home(home: &str) -> Result<String> {
    if !paths::expand_tilde(home).is_relative() {
        return Ok(home.to_string());
    }
    Ok(std::path::absolute(home).with_context(|| format!("locating {home}"))?.display().to_string())
}

/// Where a new account's home goes by default.
pub fn default_home_for(agent: &str, name: &str) -> PathBuf {
    paths::data_home().join("kraal/accounts").join(format!("{agent}-{name}"))
}

/// Create an account home: shared entries are symlinked from the agent's default home, a few
/// files are copied. Credentials are never touched; the agent's login creates them.
pub fn prepare_home(a: &Account) -> Result<Vec<String>> {
    use std::os::unix::fs::PermissionsExt;
    let kind = agent_kind(&a.agent).context("unknown agent")?;
    let home = paths::expand_tilde(&a.home);
    let default = paths::expand_tilde(kind.default_home);
    let mut notes = Vec::new();
    if home == default {
        return Ok(notes);
    }
    std::fs::create_dir_all(&home).with_context(|| format!("creating {}", home.display()))?;
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("making {} private", home.display()))?;
    for entry in kind.shared {
        let src = default.join(entry);
        let dst = home.join(entry);
        // A shared directory not created yet (no conversation so far) is created now: linked
        // later, it would be too late (the account would have started its own).
        if !entry.contains('.') && !src.exists() {
            std::fs::create_dir_all(&src).with_context(|| format!("creating {}", src.display()))?;
        }
        if !src.exists() || dst.symlink_metadata().is_ok() {
            continue;
        }
        std::os::unix::fs::symlink(&src, &dst).with_context(|| format!("linking {}", dst.display()))?;
        notes.push(format!("shared {entry}"));
    }
    for entry in kind.copied {
        let src = default.join(entry);
        let dst = home.join(entry);
        if src.is_file() && !dst.exists() {
            std::fs::copy(&src, &dst).with_context(|| format!("copying {}", src.display()))?;
            notes.push(format!("copied {entry}"));
        }
    }
    Ok(notes)
}

/// A command line for a pane, with its environment.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Launch {
    /// The program, then its arguments.
    pub argv: Vec<String>,
    /// Variables set for it, over what it inherits.
    pub env: Vec<(String, String)>,
    /// Variables removed from what it inherits, so that the account's own login applies.
    pub env_remove: Vec<String>,
}

/// How to start `a`'s agent: resuming `resume` if given, else a new conversation with
/// `new_session` as its id when the agent supports choosing it.
pub fn launch(a: &Account, args: &[String], resume: Option<&str>, new_session: Option<&str>) -> Result<Launch> {
    let kind = agent_kind(&a.agent).context("unknown agent")?;
    let mut l = Launch { argv: vec![kind.command.to_string()], ..Default::default() };
    let home = paths::expand_tilde(&a.home);
    if home != paths::expand_tilde(kind.default_home) {
        l.env.push((kind.home_env.to_string(), home.display().to_string()));
    } else {
        l.env_remove.push(kind.home_env.to_string());
    }
    l.env.push(("KRAAL_ACCOUNT".into(), key(&a.agent, &a.name)));
    l.env_remove.extend(kind.clear_env.iter().map(|s| s.to_string()));
    match (resume, new_session, kind.session_flag) {
        (Some(id), _, _) => l.argv.extend(kind.resume.iter().map(|s| s.replace("{session}", id))),
        (None, Some(id), Some(flag)) => l.argv.extend([flag.to_string(), id.to_string()]),
        _ => {}
    }
    l.argv.extend(args.iter().cloned());
    Ok(l)
}

/// The arguments an agent was started with, without those [`launch`] adds (the conversation it
/// resumes or the id it gives a new one): what to start it with again.
pub fn launch_args(kind: &AgentKind, argv: &[String]) -> Vec<String> {
    let rest = argv.get(1..).unwrap_or_default();
    let resumes = rest.len() >= kind.resume.len()
        && kind.resume.iter().zip(rest).all(|(t, a)| t.contains("{session}") || *t == a.as_str());
    let skip = if resumes {
        kind.resume.len()
    } else if kind.session_flag.is_some_and(|f| rest.first().is_some_and(|a| a == f)) {
        2
    } else {
        0
    };
    rest.get(skip..).unwrap_or_default().to_vec()
}

// The agents' options, from their own `--help`, by what follows them: a word after an option is
// its value or the prompt given on the command line, which `resumable_args` tells apart.

/// Claude's options that take one value.
const CLAUDE_VALUE: &[&str] = &[
    "--model",
    "--fallback-model",
    "--permission-mode",
    "--permission-prompts",
    "--append-system-prompt",
    "--system-prompt",
    "--system-prompt-snapshot",
    "--settings",
    "--setting-sources",
    "--output-format",
    "--input-format",
    "--agents",
    "--agent",
    "--autocompact",
    "--environment",
    "--max-turns",
    "--permission-prompt-tool",
    "--session-id",
    "--effort",
    "--json-schema",
    "--max-budget-usd",
    "--debug-file",
    "--plugin-dir",
    "--plugin-url",
    "--remote",
    "--remote-control-session-name-prefix",
    "-n",
    "--name",
];
/// Claude's options that take no value: a word after one of them is a prompt.
const CLAUDE_FLAGS: &[&str] = &[
    "--verbose",
    "--dangerously-skip-permissions",
    "--allow-dangerously-skip-permissions",
    "--ax-screen-reader",
    "--bare",
    "--bg",
    "--background",
    "--brief",
    "--chrome",
    "--no-chrome",
    "--desktop",
    "--exclude-dynamic-system-prompt-sections",
    "--forward-subagent-text",
    "--ide",
    "--include-hook-events",
    "--strict-mcp-config",
    "--include-partial-messages",
    "--replay-user-messages",
    "--restricted",
    "--safe-mode",
    "--tmux",
    "--fork-session",
    "--mcp-debug",
    "--continue",
    "-c",
    "--no-session-persistence",
    "--disable-slash-commands",
];
/// Codex's options that take no value.
const CODEX_FLAGS: &[&str] = &[
    "--oss",
    "--search",
    "--full-auto",
    "--approve-for-me",
    "--dangerously-bypass-approvals-and-sandbox",
    "--dangerously-bypass-hook-trust",
    "--yolo",
    "--skip-git-repo-check",
    "--strict-config",
    "--worktree",
    "--no-alt-screen",
    "--no-daemon",
];
/// Claude's options that take every word up to the next option.
const CLAUDE_VALUES: &[&str] = &[
    "--add-dir",
    "--mcp-config",
    "--allowedTools",
    "--allowed-tools",
    "--disallowedTools",
    "--disallowed-tools",
    "--betas",
    "--file",
    "--tools",
];
/// Codex's options that take one value.
const CODEX_VALUE: &[&str] = &[
    "-m",
    "--model",
    "-p",
    "--profile",
    "-c",
    "--config",
    "-s",
    "--sandbox",
    "-a",
    "--ask-for-approval",
    "-C",
    "--cd",
    "--add-dir",
    "--local-provider",
    "--enable",
    "--disable",
    "--remote",
    "--remote-auth-token-env",
];
/// Codex's options that take every word up to the next option.
const CODEX_VALUES: &[&str] = &["-i", "--image"];
/// Claude's options that make a run of its own, not a conversation (`claude -p`).
const CLAUDE_ONE_SHOT: &[&str] = &["-p", "--print", "-h", "--help", "-v", "--version"];
/// Codex's options that make a run of its own, not a conversation.
const CODEX_ONE_SHOT: &[&str] = &["-h", "--help", "-V", "--version"];
/// Claude subcommands: runs of their own, not a conversation.
const CLAUDE_COMMANDS: &[&str] = &[
    "agents",
    "attach",
    "auth",
    "auto-mode",
    "doctor",
    "gateway",
    "import",
    "install",
    "logs",
    "mcp",
    "plugin",
    "plugins",
    "purge",
    "respawn",
    "rm",
    "setup-token",
    "stop",
    "kill",
    "ultrareview",
    "update",
    "upgrade",
];
/// Codex subcommands: runs of their own, not a conversation.
const CODEX_COMMANDS: &[&str] = &[
    "exec",
    "e",
    "review",
    "login",
    "logout",
    "mcp",
    "mcp-server",
    "plugin",
    "app-server",
    "exec-server",
    "remote-control",
    "completion",
    "update",
    "doctor",
    "sandbox",
    "debug",
    "apply",
    "a",
    "queue",
    "archive",
    "unarchive",
    "delete",
    "migrate-rollouts",
    "agents",
    "proto",
    "cloud",
    "features",
    "help",
];
/// Codex subcommands that open an interactive conversation of their own: `resume [ID] [PROMPT]`,
/// and `fork [ID] [PROMPT]`, whose conversation is the fork's, not the one it was taken from.
/// Their id and prompt choose what to start; resuming the conversation they made replaces both.
const CODEX_CONVERSATIONS: &[&str] = &["resume", "fork"];
/// Options of those subcommands that choose the conversation, without a value.
const CODEX_CHOOSERS: &[&str] = &["--last", "--all"];

/// What may be repeated when a conversation is resumed, of the arguments an agent was started
/// with: its options, not a prompt given on the command line (resuming would send it again).
/// `None` for a one-shot run (`claude -p`, `--help`, a Codex subcommand such as `exec`): it is
/// not resumed. The flag is false when a prompt was left out, or when a word could not be told
/// from a prompt (after an option kraal does not know, or one whose value is optional): then
/// everything from that option on is kept exactly as given, since dropping a value breaks the
/// command, and the command must be shown before it runs: it may send a prompt again. A Codex
/// `resume` or `fork` is a conversation: what chose it (its id, `--last`) and its prompt go.
pub fn resumable_args(kind: &AgentKind, args: &[String]) -> Option<(Vec<String>, bool)> {
    type Table = &'static [&'static str];
    let (one_shot, single, several, flags, commands, conversations): (Table, Table, Table, Table, Table, Table) =
        match kind.name {
            "claude" => (CLAUDE_ONE_SHOT, CLAUDE_VALUE, CLAUDE_VALUES, CLAUDE_FLAGS, CLAUDE_COMMANDS, &[]),
            "codex" => (CODEX_ONE_SHOT, CODEX_VALUE, CODEX_VALUES, CODEX_FLAGS, CODEX_COMMANDS, CODEX_CONVERSATIONS),
            _ => return None,
        };
    // A one-shot run says so anywhere among the options, even past one whose value kraal cannot
    // tell from a prompt (`--debug api --print`): such a run is never taken for a conversation.
    let options = &args[..args.iter().position(|a| a == "--").unwrap_or(args.len())];
    if options.iter().any(|a| one_shot.contains(&a.split('=').next().unwrap_or(a))) {
        return None;
    }
    let mut kept = Vec::new();
    let mut exact = true;
    // A word was seen where the subcommand would be (a prompt, or the subcommand itself).
    let mut positional = false;
    // In `codex resume|fork`: how many of its words (id, then prompt) were left out, and whether
    // `--last` chose the conversation (then its first word may be a prompt too).
    let mut conversation: Option<(usize, bool)> = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            // What follows is the prompt.
            exact &= i + 1 == args.len();
            break;
        }
        if a.starts_with('-') && a.len() > 1 {
            let (name, attached) = match a.split_once('=') {
                Some((n, _)) => (n, true),
                None => (a.as_str(), false),
            };
            i += 1;
            if let Some((_, last)) = conversation.as_mut()
                && CODEX_CHOOSERS.contains(&name)
            {
                // The conversation resumed now is the one it made.
                *last |= name == "--last";
                continue;
            }
            kept.push(a.clone());
            if flags.contains(&name) || (attached && single.contains(&name)) {
                continue;
            }
            if single.contains(&name) {
                kept.extend(args.get(i).cloned());
                i += 1;
            } else if several.contains(&name) {
                // All its values, the first one possibly given with `=`.
                while let Some(v) = args.get(i).filter(|v| !v.starts_with('-')) {
                    kept.push(v.clone());
                    i += 1;
                }
            } else if args.get(i).is_some_and(|v| !v.starts_with('-')) {
                // An option kraal does not know (or one whose value is optional, such as
                // `--debug [filter]` or `--worktree [name]`), then a word: its value or a prompt
                // (or another value, after `--opt=value`), which only the agent's own parser
                // knows. So is whether a later word is a subcommand (`--worktree update` names a
                // worktree): the rest stays exactly as given, to be shown before it runs.
                kept.extend(args[i..].iter().cloned());
                exact = false;
                break;
            }
        } else {
            i += 1;
            match conversation.as_mut() {
                Some((words, last)) => {
                    // Its id goes; a prompt after it (or after `--last`) is left out.
                    if *words > 0 || *last {
                        exact = false;
                    }
                    *words += 1;
                }
                None if !positional && conversations.contains(&a.as_str()) => conversation = Some((0, false)),
                None if !positional && commands.contains(&a.as_str()) => return None,
                None => {
                    positional = true;
                    exact = false;
                }
            }
        }
    }
    Some((kept, exact))
}

/// A command line that started agent `kind` directly (`claude --model opus`, not through an
/// account), rewritten to resume conversation `session` with the same options (see
/// [`resumable_args`] for the flag). `None` when it cannot be done safely (an odd id, a
/// one-shot run).
pub fn resume_argv(kind: &AgentKind, argv: &[String], session: &str) -> Option<(Vec<String>, bool)> {
    let safe = !session.is_empty() && session.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    let program = argv.first()?;
    if !safe || program.rsplit('/').next() != Some(kind.command) {
        return None;
    }
    let mut rest: Vec<String> = Vec::new();
    let args = argv[1..].iter();
    match kind.name {
        "claude" => {
            // Whatever chose the conversation before goes: this one is resumed. `--resume`'s
            // value is optional (alone, it opens a picker).
            let mut args = args.peekable();
            while let Some(a) = args.next() {
                match a.as_str() {
                    "--session-id" => {
                        args.next();
                    }
                    "--resume" | "-r" => {
                        args.next_if(|v| !v.starts_with('-'));
                    }
                    "--continue" | "-c" => {}
                    _ if a.starts_with("--resume=") || a.starts_with("--session-id=") => {}
                    _ => rest.push(a.clone()),
                }
            }
        }
        // `codex resume|fork …` is told by [`resumable_args`].
        "codex" => rest.extend(args.cloned()),
        _ => return None,
    }
    let (kept, exact) = resumable_args(kind, &rest)?;
    let mut out = vec![program.clone()];
    out.extend(kind.resume.iter().map(|t| t.replace("{session}", session)));
    out.extend(kept);
    Some((out, exact))
}

/// Whether conversation `session` exists for account `a`, when the agent's files tell (Claude
/// keeps a transcript per conversation, once something was said in it); `None` when they do not
/// (another agent, an odd id). An error when they could not be read: that is not "no".
pub fn conversation_exists(a: &Account, session: &str) -> Result<Option<bool>> {
    if a.agent != "claude" || !session.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Ok(None);
    }
    Ok(Some(transcript(a, session)?.is_some()))
}

/// Where conversation `session`'s transcript is in account `a`'s home, if anywhere. An error
/// when a directory where it may be could not be read.
fn transcript(a: &Account, session: &str) -> Result<Option<PathBuf>> {
    let file = format!("{session}.jsonl");
    let projects = paths::expand_tilde(&a.home).join("projects");
    let dirs = match std::fs::read_dir(&projects) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", projects.display())),
    };
    for dir in dirs {
        let path = dir.with_context(|| format!("reading {}", projects.display()))?.path().join(&file);
        match std::fs::metadata(&path) {
            Ok(m) if m.is_file() => return Ok(Some(path)),
            Ok(_) => {}
            // A project directory without this conversation, or not a directory at all.
            Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT) | Some(libc::ENOTDIR)) => {}
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }
    Ok(None)
}

/// Whether file `short` holds the start of file `long`.
fn is_prefix(short: &Path, long: &Path) -> std::io::Result<bool> {
    use std::io::Read;
    let (mut a, mut b) = (std::fs::File::open(short)?, std::fs::File::open(long)?);
    if a.metadata()?.len() > b.metadata()?.len() {
        return Ok(false);
    }
    let (mut x, mut y) = (vec![0u8; 64 << 10], vec![0u8; 64 << 10]);
    loop {
        let n = a.read(&mut x)?;
        if n == 0 {
            return Ok(true);
        }
        b.read_exact(&mut y[..n])?;
        if x[..n] != y[..n] {
            return Ok(false);
        }
    }
}

/// What continuing conversation `session` on account `to` needs (Claude: accounts that keep
/// their conversations apart each have a copy of the transcript, which only ever grows).
enum Carry {
    /// No transcript anywhere: nothing was said yet, there is nothing to continue.
    Nothing,
    /// `to` sees the newest transcript already (the same file, or a copy holding all of it).
    There,
    /// `to` needs the newest transcript, `from`'s, at that place.
    Copy(PathBuf, PathBuf),
}

fn carry_plan(from: &Account, to: &Account, session: &str) -> Result<Carry> {
    if from.agent != "claude" || to.agent != "claude" || conversation_exists(to, session)?.is_none() {
        return Ok(Carry::Nothing);
    }
    let (src, dst) = (transcript(from, session)?, transcript(to, session)?);
    let Some(src) = src else {
        return Ok(if dst.is_some() { Carry::There } else { Carry::Nothing });
    };
    let Some(dst) = dst else {
        let project = src.parent().and_then(|d| d.file_name()).context("an odd transcript path")?;
        let dst = paths::expand_tilde(&to.home).join("projects").join(project).join(src.file_name().unwrap());
        return Ok(Carry::Copy(src, dst));
    };
    let same = match (std::fs::canonicalize(&src), std::fs::canonicalize(&dst)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if same || is_prefix(&src, &dst)? {
        return Ok(Carry::There);
    }
    if is_prefix(&dst, &src)? {
        return Ok(Carry::Copy(src, dst));
    }
    bail!(
        "{} and {} each went on with this conversation on their own: continuing on one would lose \
         what was said on the other",
        label(from),
        label(to)
    )
}

/// Refuse early, before an agent is stopped for a switch, when its conversation cannot follow
/// to `to` whole.
pub fn check_carry(from: &Account, to: &Account, session: &str) -> Result<()> {
    carry_plan(from, to, session).map(|_| ())
}

/// Give account `to` the newest transcript of conversation `session` of account `from` (call it
/// once `from`'s agent stopped writing it). True when `to` can then resume it; an error when it
/// cannot without losing part of it.
pub fn carry_conversation(from: &Account, to: &Account, session: &str) -> Result<bool> {
    let (src, dst) = match carry_plan(from, to, session)? {
        Carry::Nothing => return Ok(false),
        Carry::There => return Ok(true),
        Carry::Copy(src, dst) => (src, dst),
    };
    let dir = dst.parent().context("an odd transcript path")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let data = std::fs::read(&src).with_context(|| format!("reading {}", src.display()))?;
    paths::write_private(&dst, &data).with_context(|| format!("copying the conversation to {}", dst.display()))?;
    Ok(true)
}

/// The agent's own login command for an account.
pub fn login(a: &Account) -> Result<Launch> {
    let kind = agent_kind(&a.agent).context("unknown agent")?;
    agent_command(a, kind.login)
}

/// The agent's own command that tells whether an account is logged in.
pub fn login_status(a: &Account) -> Result<Launch> {
    let kind = agent_kind(&a.agent).context("unknown agent")?;
    agent_command(a, kind.login_status)
}

/// The agent's program with `args`, in an account's environment.
fn agent_command(a: &Account, args: &[&str]) -> Result<Launch> {
    let kind = agent_kind(&a.agent).context("unknown agent")?;
    let mut l = launch(a, &[], None, None)?;
    l.argv = std::iter::once(kind.command.to_string()).chain(args.iter().map(|s| s.to_string())).collect();
    Ok(l)
}

/// A random UUID v4 (for `--session-id`).
pub fn new_uuid() -> String {
    use std::io::Read;
    let mut b = [0u8; 16];
    let random = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut b));
    if random.is_err() {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        b = t.to_le_bytes();
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

// ---- usage -----------------------------------------------------------------------------------

/// One of an account's usage windows: five hours, or a week.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// How much of the window is used, in percent.
    pub used_percent: f64,
    /// When the window starts again from zero, in Unix seconds; `None` when the source does not
    /// say.
    pub resets_at: Option<i64>,
}

/// The latest usage seen for an account, saved under the state directory ([`usage_path`]).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Usage {
    /// The five-hour window, when a source reported it.
    pub five_hour: Option<Window>,
    /// The weekly window, when a source reported it.
    pub weekly: Option<Window>,
    /// Unix seconds when this was observed.
    pub observed_at: i64,
    /// Where it was read: "statusline" (Claude Code's status line), "codex" (what Codex reports)
    /// or "screen" (a limit message on the agent's screen).
    pub source: String,
}

impl Usage {
    fn windows(&self) -> impl Iterator<Item = &Window> {
        self.five_hour.iter().chain(self.weekly.iter())
    }

    /// Highest usage among windows that have not reset yet. A window that does not say when it
    /// resets counts until the end of its period after it was seen.
    pub fn peak(&self, now: i64) -> f64 {
        let live = |w: &Window, period: i64| w.resets_at.unwrap_or(self.observed_at.saturating_add(period)) > now;
        let five = self.five_hour.iter().filter(|w| live(w, 5 * 3600));
        let week = self.weekly.iter().filter(|w| live(w, 7 * 24 * 3600));
        five.chain(week).map(|w| w.used_percent).fold(0.0, f64::max)
    }

    /// A limit message seen on the agent's screen, saying `pct` percent of a window (weekly or
    /// five-hour) is used: the latest word, up or down. A window that already reset says
    /// nothing any more; the screen does not tell when this one resets, so at the latest when
    /// its period ends (a full account is not full for ever).
    pub fn observe_screen(&mut self, weekly: bool, pct: f64, now: i64) {
        let w = if weekly { &mut self.weekly } else { &mut self.five_hour };
        let current = w.as_ref().filter(|w| w.resets_at.is_none_or(|r| r > now));
        let period = if weekly { 7 * 24 * 3600 } else { 5 * 3600 };
        let resets_at = current.and_then(|w| w.resets_at).unwrap_or(now.saturating_add(period));
        *w = Some(Window { used_percent: pct, resets_at: Some(resets_at) });
        self.observed_at = now;
        self.source = "screen".into();
    }

    /// When the fullest window resets.
    pub fn next_reset(&self, now: i64) -> Option<i64> {
        self.windows()
            .filter(|w| w.resets_at.is_some_and(|r| r > now))
            .max_by(|a, b| a.used_percent.total_cmp(&b.used_percent))
            .and_then(|w| w.resets_at)
    }
}

fn usage_dir() -> PathBuf {
    paths::state_dir().join("usage")
}

/// An account's usage file in the usage directory.
fn usage_file(agent: &str, name: &str) -> String {
    format!("{agent}-{name}.json")
}

/// Where an account's latest usage is saved: the status line command Claude runs and the server
/// write it, the server and `kraal accounts` read it, and it outlives a restart.
fn usage_path(agent: &str, name: &str) -> PathBuf {
    usage_dir().join(usage_file(agent, name))
}

/// Give account `old`'s usage, in directory `dir`, to its new name `new`. With none seen yet,
/// the new name has none either: never that of a removed account once so named.
fn move_usage(dir: &Path, agent: &str, old: &str, new: &str) -> Result<()> {
    let (from, to) = (dir.join(usage_file(agent, old)), dir.join(usage_file(agent, new)));
    if from.exists() {
        return std::fs::rename(&from, &to).with_context(|| format!("moving {} to {}", from.display(), to.display()));
    }
    match std::fs::remove_file(&to) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", to.display()))
        }
        _ => Ok(()),
    }
}

pub fn save_usage(agent: &str, name: &str, u: &Usage) -> Result<()> {
    let path = usage_path(agent, name);
    if let Some(d) = path.parent() {
        paths::ensure_private_dir(d)?;
    }
    paths::write_private(&path, &serde_json::to_vec(u)?)?;
    Ok(())
}

fn load_usage(agent: &str, name: &str) -> Option<Usage> {
    serde_json::from_slice(&std::fs::read(usage_path(agent, name)).ok()?).ok()
}

/// Latest usage per account, keyed by `agent:name`.
#[derive(Debug, Clone, Default)]
pub struct UsageMap(pub HashMap<String, Usage>);

impl UsageMap {
    pub fn load(accounts: &[Account]) -> UsageMap {
        UsageMap(
            accounts
                .iter()
                .filter_map(|a| load_usage(&a.agent, &a.name).map(|u| (key(&a.agent, &a.name), u)))
                .collect(),
        )
    }

    pub fn get(&self, a: &Account) -> Option<&Usage> {
        self.0.get(&key(&a.agent, &a.name))
    }

    pub fn peak(&self, a: &Account, now: i64) -> f64 {
        self.get(a).map(|u| u.peak(now)).unwrap_or(0.0)
    }
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Usage from a Claude Code status line payload (`rate_limits.five_hour` / `seven_day`).
pub fn usage_from_statusline(v: &serde_json::Value) -> Option<Usage> {
    let rl = v.get("rate_limits")?;
    let window = |name: &str| -> Option<Window> {
        let w = rl.get(name)?;
        Some(Window {
            used_percent: w.get("used_percentage")?.as_f64()?,
            resets_at: w.get("resets_at").and_then(|r| r.as_i64().or_else(|| r.as_f64().map(|f| f as i64))),
        })
    };
    let u = Usage {
        five_hour: window("five_hour"),
        weekly: window("seven_day"),
        observed_at: now(),
        source: "statusline".into(),
    };
    (u.five_hour.is_some() || u.weekly.is_some()).then_some(u)
}

/// Limit messages printed by agents, e.g. "You've hit your session limit · resets 3:45pm" or
/// "You've used 85% of your weekly limit". Returns (weekly?, percent).
pub fn limit_from_screen(screen: &str) -> Option<(bool, f64)> {
    let tail: Vec<&str> = screen.lines().rev().filter(|l| !l.trim().is_empty()).take(15).collect();
    for line in tail {
        let l = line.to_lowercase();
        let weekly = l.contains("weekly limit");
        let session = l.contains("session limit") || l.contains("5-hour limit") || l.contains("usage limit");
        if !(weekly || session) {
            continue;
        }
        if l.contains("hit your") || l.contains("limit reached") || l.contains("reached your") {
            return Some((weekly, 100.0));
        }
        if let Some(i) = l.find("used ")
            && let Some(p) = l[i + 5..].split('%').next()
            && let Ok(n) = p.trim().parse::<f64>()
        {
            return Some((weekly, n));
        }
    }
    None
}

/// How long until `resets_at`, for display: "resets in 1d2h", "resets in 3h05" or "resets in
/// 12m"; empty when it is not known.
pub fn describe_reset(resets_at: Option<i64>, now: i64) -> String {
    let Some(r) = resets_at else { return String::new() };
    // What an agent says: any number.
    let secs = r.saturating_sub(now).max(0);
    if secs >= 86_400 {
        format!("resets in {}d{}h", secs / 86_400, (secs % 86_400) / 3600)
    } else if secs >= 3600 {
        format!("resets in {}h{:02}", secs / 3600, (secs % 3600) / 60)
    } else {
        format!("resets in {}m", secs / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_started_directly_resume_with_their_arguments() {
        let claude = agent_kind("claude").unwrap();
        let argv = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(
            resume_argv(claude, &argv("/usr/bin/claude --model opus -c"), "abc").unwrap(),
            (argv("/usr/bin/claude --resume abc --model opus"), true)
        );
        assert_eq!(
            resume_argv(claude, &argv("claude --resume old"), "abc").unwrap(),
            (argv("claude --resume abc"), true)
        );
        assert_eq!(
            resume_argv(claude, &argv("claude -r --model opus"), "abc").unwrap(),
            (argv("claude --resume abc --model opus"), true),
            "a picker's -r has no value: the option after it stays"
        );
        assert!(resume_argv(claude, &argv("claude"), "x; rm -rf /").is_none());
        assert!(resume_argv(claude, &argv("vim"), "abc").is_none());
        let codex = agent_kind("codex").unwrap();
        assert_eq!(resume_argv(codex, &argv("codex -m o3"), "abc").unwrap(), (argv("codex resume abc -m o3"), true));
        assert!(resume_argv(codex, &argv("codex exec hi"), "abc").is_none());
        assert!(resume_argv(codex, &argv("codex -m o3 exec hi"), "abc").is_none());
        // A one-shot run is never resumed; a prompt is never sent again, and the pane then
        // waits for Enter.
        assert!(resume_argv(claude, &argv("claude -p deploy"), "abc").is_none());
        assert!(resume_argv(claude, &argv("claude --model opus --print deploy"), "abc").is_none());
        assert_eq!(
            resume_argv(claude, &argv("claude --model opus deploy"), "abc").unwrap(),
            (argv("claude --resume abc --model opus"), false)
        );
        assert_eq!(
            resume_argv(claude, &argv("claude --add-dir a b --verbose -- deploy"), "abc").unwrap(),
            (argv("claude --resume abc --add-dir a b --verbose"), false)
        );
        assert_eq!(
            resume_argv(codex, &argv("codex -m o3 fix"), "abc").unwrap(),
            (argv("codex resume abc -m o3"), false)
        );
        // Option values are kept: dropping one would break the command.
        assert_eq!(
            resume_argv(claude, &argv("claude --effort high"), "abc").unwrap(),
            (argv("claude --resume abc --effort high"), true)
        );
        assert_eq!(
            resume_argv(codex, &argv("codex --enable web_search"), "abc").unwrap(),
            (argv("codex resume abc --enable web_search"), true)
        );
        assert_eq!(
            resume_argv(claude, &argv("claude --verbose deploy"), "abc").unwrap(),
            (argv("claude --resume abc --verbose"), false),
            "a word after a flag that takes no value is a prompt"
        );
        // After an option kraal does not know, or one whose value is optional, everything stays
        // as given (a value is never dropped), and the pane waits for Enter.
        assert_eq!(
            resume_argv(claude, &argv("claude --new-option x"), "abc").unwrap(),
            (argv("claude --resume abc --new-option x"), false)
        );
        assert_eq!(
            resume_argv(claude, &argv("claude --new-option a b --verbose c"), "abc").unwrap(),
            (argv("claude --resume abc --new-option a b --verbose c"), false)
        );
        assert_eq!(
            resume_argv(claude, &argv("claude --debug api deploy"), "abc").unwrap(),
            (argv("claude --resume abc --debug api deploy"), false)
        );
        assert!(resume_argv(claude, &argv("claude --debug --verbose"), "abc").unwrap().1);
        // Options that take several values keep them all.
        assert_eq!(
            resume_argv(claude, &argv("claude --file file_abc:doc.txt file_def:img.png"), "abc").unwrap(),
            (argv("claude --resume abc --file file_abc:doc.txt file_def:img.png"), true)
        );
        // A flag the agent knows takes no value: the word after it is a prompt, never resent.
        assert_eq!(
            resume_argv(claude, &argv("claude --chrome Deploy"), "abc").unwrap(),
            (argv("claude --resume abc --chrome"), false)
        );
        assert!(resume_argv(claude, &argv("claude --help"), "abc").is_none());
        // A one-shot run says so anywhere among the options, even after one kraal cannot read.
        assert!(resume_argv(claude, &argv("claude --debug api --print Deploy"), "abc").is_none());
        assert!(resume_argv(claude, &argv("claude --brand-new x -p Deploy"), "abc").is_none());
        // A subcommand runs on its own only where a subcommand goes: the first word that is not
        // an option or an option's value.
        assert!(resume_argv(claude, &argv("claude update"), "abc").is_none());
        assert!(resume_argv(claude, &argv("claude mcp list"), "abc").is_none());
        assert!(resume_argv(claude, &argv("claude --model opus update"), "abc").is_none());
        assert!(resume_argv(claude, &argv("claude --verbose mcp list"), "abc").is_none());
        // After an option whose value is optional or unknown, which word is the subcommand cannot
        // be told: nothing is taken for a one-shot run, everything stays, and the command is shown
        // before it runs. `--worktree update` names a worktree, `update` is not run.
        assert_eq!(
            resume_argv(claude, &argv("claude --worktree update Deploy"), "abc").unwrap(),
            (argv("claude --resume abc --worktree update Deploy"), false)
        );
        assert_eq!(
            resume_argv(claude, &argv("claude -w update Deploy"), "abc").unwrap(),
            (argv("claude --resume abc -w update Deploy"), false)
        );
        assert_eq!(
            resume_argv(codex, &argv("codex --brand-new x exec hi"), "abc").unwrap(),
            (argv("codex resume abc --brand-new x exec hi"), false)
        );
        // A value given with `=` to an option that takes several keeps the ones after it.
        assert_eq!(
            resume_argv(claude, &argv("claude --file=file_abc:doc.txt file_def:img.png"), "abc").unwrap(),
            (argv("claude --resume abc --file=file_abc:doc.txt file_def:img.png"), true)
        );
        assert_eq!(
            resume_argv(claude, &argv("claude --model=opus Deploy"), "abc").unwrap(),
            (argv("claude --resume abc --model=opus"), false)
        );
        // After `--opt=value` of an option kraal does not know, a word may be another value.
        assert_eq!(
            resume_argv(claude, &argv("claude --brand-new=a b"), "abc").unwrap(),
            (argv("claude --resume abc --brand-new=a b"), false)
        );
        // A Codex fork or resume is a conversation: the one it made is resumed, without the id
        // it was taken from or its prompt (never sent again).
        assert_eq!(
            resume_argv(codex, &argv("codex fork parent"), "child").unwrap(),
            (argv("codex resume child"), true)
        );
        assert_eq!(
            resume_argv(codex, &argv("codex fork parent Deploy"), "child").unwrap(),
            (argv("codex resume child"), false)
        );
        assert_eq!(
            resume_argv(codex, &argv("codex -m o3 fork --last --search Deploy"), "child").unwrap(),
            (argv("codex resume child -m o3 --search"), false)
        );
        assert_eq!(
            resume_argv(codex, &argv("codex fork --all -i a.png b.png parent"), "child").unwrap(),
            (argv("codex resume child -i a.png b.png parent"), true),
            "-i takes every word after it, as Codex's own parser does"
        );
        assert_eq!(
            resume_argv(codex, &argv("codex resume old -m o3"), "abc").unwrap(),
            (argv("codex resume abc -m o3"), true)
        );
        assert_eq!(resume_argv(codex, &argv("codex resume --last"), "abc").unwrap(), (argv("codex resume abc"), true));
        assert_eq!(
            resumable_args(codex, &argv("fork parent Deploy")).unwrap(),
            (Vec::new(), false),
            "started from an account, the same"
        );
    }

    #[test]
    fn conversations_are_found_by_their_transcript() {
        let home = std::env::temp_dir().join(format!("kraal-conv-{}", std::process::id()));
        let a = Account {
            name: "t".into(),
            agent: "claude".into(),
            home: home.display().to_string(),
            threshold: None,
            note: None,
        };
        assert_eq!(conversation_exists(&a, "abc").unwrap(), Some(false));
        std::fs::create_dir_all(home.join("projects/-home-me")).unwrap();
        std::fs::write(home.join("projects/-home-me/abc.jsonl"), "").unwrap();
        assert_eq!(conversation_exists(&a, "abc").unwrap(), Some(true));
        assert_eq!(conversation_exists(&a, "other").unwrap(), Some(false));
        // Files that cannot be read do not say "no conversation".
        use std::os::unix::fs::PermissionsExt;
        let projects = home.join("projects");
        std::fs::set_permissions(&projects, std::fs::Permissions::from_mode(0o000)).unwrap();
        let unreadable = std::fs::read_dir(&projects).is_err(); // not when tests run as root
        if unreadable {
            assert!(conversation_exists(&a, "abc").is_err());
        }
        std::fs::set_permissions(&projects, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn login_status_commands() {
        let a = Accounts::empty();
        let argv = |agent: &str| login_status(&a.get(agent, "default").unwrap()).unwrap().argv;
        assert_eq!(argv("claude"), vec!["claude", "auth", "status"]);
        assert_eq!(argv("codex"), vec!["codex", "login", "status"]);
    }

    #[test]
    fn launch_args_are_what_the_user_gave() {
        let claude = agent_kind("claude").unwrap();
        let argv = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
        assert_eq!(launch_args(claude, &argv("claude --session-id abc --model opus")), argv("--model opus"));
        assert_eq!(launch_args(claude, &argv("claude --resume abc --model opus")), argv("--model opus"));
        assert!(launch_args(claude, &argv("claude")).is_empty());
        let codex = agent_kind("codex").unwrap();
        assert_eq!(launch_args(codex, &argv("codex resume abc -m o3")), argv("-m o3"));
        assert_eq!(launch_args(codex, &argv("codex -m o3")), argv("-m o3"));
    }

    fn acct(agent: &str, name: &str, home: &str) -> Account {
        Account { name: name.into(), agent: agent.into(), home: home.into(), threshold: None, note: None }
    }

    fn accounts() -> Accounts {
        let mut a = Accounts::empty();
        a.accounts = vec![acct("claude", "pro", "/tmp/kraal-test-claude-pro"), acct("claude", "perso", "~/.claude")];
        a.rules = vec![Rule { workspace: Some("work".into()), path: None, agent: None, accounts: vec!["pro".into()] }];
        a
    }

    #[test]
    fn implicit_default_account() {
        let a = Accounts::empty();
        let codex = a.for_agent("codex");
        assert_eq!(codex.len(), 1);
        assert_eq!(codex[0].name, "default");
        // When an account uses the default home, no implicit one is added.
        let a = accounts();
        let names: Vec<String> = a.for_agent("claude").into_iter().map(|x| x.name).collect();
        assert_eq!(names, vec!["pro", "perso"]);
        // "default" names the default home only.
        let mut a = Accounts::empty();
        assert!(a.add(acct("codex", "default", "/tmp/kraal-test-other")).is_err());
        a.accounts.push(acct("codex", "default", "/tmp/kraal-test-other"));
        assert_eq!(a.get("codex", "default").unwrap().home, "/tmp/kraal-test-other", "an older one stays reachable");
    }

    #[test]
    fn account_homes_are_kept_absolute() {
        assert_eq!(absolute_home("~/.claude-x").unwrap(), "~/.claude-x");
        assert_eq!(absolute_home("/srv/x").unwrap(), "/srv/x");
        let rel = absolute_home("agent-home").unwrap();
        assert!(Path::new(&rel).is_absolute() && rel.ends_with("agent-home"), "{rel}");
    }

    #[test]
    fn rules_and_thresholds_choose() {
        let a = accounts();
        let mut usage = UsageMap::default();
        let now = 1_000_000;
        assert_eq!(a.choose("claude", Some("work"), None, &usage, now).unwrap().name, "pro");
        assert_eq!(a.choose("claude", Some("other"), None, &usage, now).unwrap().name, "pro");
        usage.0.insert(
            "claude:pro".into(),
            Usage { five_hour: Some(Window { used_percent: 95.0, resets_at: Some(now + 600) }), ..Default::default() },
        );
        // Over the threshold: the rule has no other account, so it takes the one that resets first.
        assert_eq!(a.choose("claude", Some("work"), None, &usage, now).unwrap().name, "pro");
        assert_eq!(a.choose("claude", None, None, &usage, now).unwrap().name, "perso");
        let pro = a.get("claude", "pro").unwrap();
        // The "work" rule only lists pro: no alternative there, but elsewhere perso is fine.
        assert!(a.alternative(&pro, Some("work"), None, &usage, now).is_none());
        assert_eq!(a.alternative(&pro, None, None, &usage, now).unwrap().name, "perso");
        // A window that already reset does not count.
        assert_eq!(usage.peak(&pro, now + 601), 0.0);
    }

    #[test]
    fn a_worktree_anywhere_waits_for_git_before_any_account() {
        let dir = std::env::temp_dir().join(format!("kraal-rule-worktree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = dir.join("company/repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |cwd: &Path, args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(cwd)
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .unwrap();
            assert!(ok.status.success(), "{args:?}: {}", String::from_utf8_lossy(&ok.stderr));
        };
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "one"]);
        // A linked worktree far from the repository, not in kraal's `<repo>.worktrees/`.
        let feature = dir.join("elsewhere/feature");
        git(&repo, &["worktree", "add", "-q", "-b", "feature", feature.to_str().unwrap()]);
        let repo = std::fs::canonicalize(&repo).unwrap();
        let feature = std::fs::canonicalize(&feature).unwrap();
        let mut a = accounts();
        a.rules = vec![Rule {
            workspace: None,
            path: Some(repo.display().to_string()),
            agent: None,
            accounts: vec!["pro".into()],
        }];
        let usage = UsageMap::default();
        // Until git has said where the worktree belongs, no choice is made from the
        // unrestricted list: the caller asks git first.
        assert!(a.undecided("claude", Some(&feature)));
        assert!(!a.undecided("claude", Some(&repo.join("src"))), "inside the rule's own path");
        crate::git::project_path(&feature);
        assert!(!a.undecided("claude", Some(&feature)));
        assert_eq!(a.choose("claude", None, Some(&feature), &usage, 0).unwrap().name, "pro");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_conversation_follows_to_an_account_that_keeps_its_own() {
        let dir = std::env::temp_dir().join(format!("kraal-carry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (from, to) = (dir.join("perso"), dir.join("pro"));
        std::fs::create_dir_all(from.join("projects/-work-app")).unwrap();
        std::fs::create_dir_all(to.join("projects")).unwrap();
        std::fs::write(from.join("projects/-work-app/abc-1.jsonl"), "{}\n").unwrap();
        let a = acct("claude", "perso", from.to_str().unwrap());
        let b = acct("claude", "pro", to.to_str().unwrap());
        assert_eq!(conversation_exists(&b, "abc-1").unwrap(), Some(false));
        assert!(carry_conversation(&a, &b, "abc-1").unwrap());
        assert_eq!(conversation_exists(&b, "abc-1").unwrap(), Some(true));
        assert!(carry_conversation(&a, &b, "abc-1").unwrap(), "again: already there");
        assert!(!carry_conversation(&a, &b, "nope").unwrap(), "nothing to carry");
        let codex = acct("codex", "pro", to.to_str().unwrap());
        assert!(!carry_conversation(&codex, &codex, "abc-1").unwrap());
        // The conversation went on on pro; switching back to perso takes all of it, never the
        // older copy perso still has.
        let (on_from, on_to) = (from.join("projects/-work-app/abc-1.jsonl"), to.join("projects/-work-app/abc-1.jsonl"));
        std::fs::write(&on_to, "{}\n{\"more\":1}\n").unwrap();
        assert!(carry_conversation(&b, &a, "abc-1").unwrap());
        assert_eq!(std::fs::read_to_string(&on_from).unwrap(), "{}\n{\"more\":1}\n");
        assert!(carry_conversation(&b, &a, "abc-1").unwrap(), "perso has all of it now");
        assert!(carry_conversation(&a, &b, "abc-1").unwrap(), "pro has all of it too");
        // Each went on on its own: neither copy holds the other, continuing would lose a part.
        std::fs::write(&on_from, "{}\n{\"more\":1}\n{\"perso\":1}\n").unwrap();
        std::fs::write(&on_to, "{}\n{\"more\":1}\n{\"pro\":1}\n").unwrap();
        assert!(check_carry(&a, &b, "abc-1").is_err());
        assert!(carry_conversation(&a, &b, "abc-1").is_err());
        assert_eq!(std::fs::read_to_string(&on_to).unwrap(), "{}\n{\"more\":1}\n{\"pro\":1}\n", "untouched");
        // A directory that cannot be read is an error, never "nothing to carry" (which would
        // start a new conversation) nor "only the other copy" (an older one).
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(&on_from, "{}\n{\"more\":1}\n").unwrap();
        std::fs::write(&on_to, "{}\n").unwrap();
        for side in [from.join("projects"), to.join("projects")] {
            std::fs::set_permissions(&side, std::fs::Permissions::from_mode(0o000)).unwrap();
            if std::fs::read_dir(&side).is_err() {
                assert!(check_carry(&a, &b, "abc-1").is_err(), "{}", side.display());
                assert!(carry_conversation(&a, &b, "abc-1").is_err(), "{}", side.display());
            }
            std::fs::set_permissions(&side, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(std::fs::read_to_string(&on_to).unwrap(), "{}\n", "untouched");
        // Accounts sharing their conversations see one file: nothing to do.
        let shared = dir.join("shared");
        std::fs::create_dir_all(shared.join("projects")).unwrap();
        std::os::unix::fs::symlink(from.join("projects"), shared.join("projects/../projects-link")).unwrap();
        let c = acct("claude", "shared", shared.to_str().unwrap());
        std::fs::remove_dir(shared.join("projects")).unwrap();
        std::fs::rename(shared.join("projects-link"), shared.join("projects")).unwrap();
        assert!(carry_conversation(&a, &c, "abc-1").unwrap());
        assert!(check_carry(&c, &a, "abc-1").is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_the_screen_says_goes_up_or_down_and_ends() {
        let mut u = Usage::default();
        u.observe_screen(false, 100.0, 1_000);
        assert_eq!(u.peak(1_000), 100.0);
        u.observe_screen(false, 40.0, 2_000);
        assert_eq!(u.peak(2_000), 40.0, "a lower figure seen since counts");
        assert_eq!(u.five_hour.as_ref().unwrap().resets_at, Some(1_000 + 5 * 3600), "the same window");
        assert_eq!(u.peak(1_000 + 5 * 3600 + 1), 0.0, "the window ends with its period");
    }

    #[test]
    fn removing_an_account_keeps_other_agents_rules_and_narrows() {
        let mut a = accounts();
        a.accounts.push(acct("codex", "pro", "/tmp/kraal-test-codex-pro"));
        a.rules.push(Rule {
            workspace: Some("ops".into()),
            path: None,
            agent: Some("codex".into()),
            accounts: vec!["pro".into()],
        });
        assert!(a.remove("claude", "pro"));
        // Codex's rule and the shared one keep "pro": codex still has it.
        let named: Vec<(Option<String>, Vec<String>)> =
            a.rules.iter().map(|r| (r.agent.clone(), r.accounts.clone())).collect();
        assert_eq!(
            named,
            vec![
                (Some("claude".into()), vec![]),
                (None, vec!["pro".into()]),
                (Some("codex".into()), vec!["pro".into()])
            ]
        );
        let usage = UsageMap::default();
        assert_eq!(a.choose("codex", Some("work"), None, &usage, 0).unwrap().name, "pro");
        // Claude used pro in "work": now no account there rather than any.
        assert!(a.choose("claude", Some("work"), None, &usage, 0).is_none());
        assert_eq!(a.choose("claude", None, None, &usage, 0).unwrap().name, "perso");
        assert!(a.remove("codex", "pro"));
        assert!(a.choose("codex", Some("ops"), None, &usage, 0).is_none(), "an emptied rule refuses");
    }

    #[test]
    fn renaming_an_account_takes_its_rules_and_usage_along() {
        let dir = std::env::temp_dir().join(format!("kraal-rename-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (file, usage) = (dir.join("accounts.toml"), dir.join("usage"));
        std::fs::create_dir_all(&usage).unwrap();
        let rule = |workspace: &str, agent: Option<&str>, accounts: &[&str]| Rule {
            workspace: Some(workspace.into()),
            path: None,
            agent: agent.map(String::from),
            accounts: accounts.iter().map(|n| n.to_string()).collect(),
        };
        let mut a = accounts();
        a.path = file.clone();
        a.accounts.push(acct("codex", "pro", "/tmp/kraal-test-codex-pro"));
        a.rules = vec![
            rule("work", None, &["pro"]),
            rule("ops", Some("claude"), &["pro", "perso"]),
            rule("ci", Some("codex"), &["pro"]),
            rule("home", None, &["perso"]),
        ];
        a.save().unwrap();
        std::fs::write(usage.join("claude-pro.json"), "pro's").unwrap();
        // A removed account's usage, under the name perso takes.
        std::fs::write(usage.join("claude-me.json"), "stale").unwrap();

        rename_in(&file, &usage, "claude", "pro", "work").unwrap();
        rename_in(&file, &usage, "claude", "perso", "me").unwrap();
        let a = Accounts::load_from(&file).unwrap();
        let names: Vec<String> = a.all().iter().map(label).collect();
        assert_eq!(names, ["claude·work", "claude·me", "codex·default", "codex·pro"]);
        // Codex's pro keeps the rules naming it; claude's work gets its own before the shared one.
        assert_eq!(
            a.rules,
            vec![
                rule("work", Some("claude"), &["work"]),
                rule("work", None, &["pro"]),
                rule("ops", Some("claude"), &["work", "me"]),
                rule("ci", Some("codex"), &["pro"]),
                rule("home", None, &["me"]),
            ]
        );
        let none = UsageMap::default();
        assert_eq!(a.choose("claude", Some("work"), None, &none, 0).unwrap().name, "work");
        assert_eq!(a.choose("codex", Some("work"), None, &none, 0).unwrap().name, "pro");
        assert_eq!(std::fs::read_to_string(usage.join("claude-work.json")).unwrap(), "pro's");
        assert!(!usage.join("claude-pro.json").exists());
        assert!(!usage.join("claude-me.json").exists(), "me has seen no usage yet");

        let text = std::fs::read_to_string(&file).unwrap();
        let refused = |agent: &str, old: &str, new: &str, why: &str| {
            let e = format!("{:#}", rename_in(&file, &usage, agent, old, new).unwrap_err());
            assert!(e.contains(why), "{old} to {new}: {e}");
            assert_eq!(std::fs::read_to_string(&file).unwrap(), text, "{old} to {new} changed the file");
        };
        refused("claude", "work", "me", "already has an account named");
        refused("claude", "work", "my work", "invalid account name");
        refused("claude", "work", "default", "is the account in ~/.claude");
        refused("codex", "default", "mine", "cannot be renamed");
        refused("claude", "gone", "here", "no account named");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_pane_follows_its_account_renamed() {
        let mut a = accounts();
        let env = |account: &Account| launch(account, &[], None, None).unwrap().env;
        let (pro, perso) = (env(&a.get("claude", "pro").unwrap()), env(&a.get("claude", "perso").unwrap()));
        a.rename("claude", "pro", "work").unwrap();
        a.rename("claude", "perso", "me").unwrap();
        assert_eq!(a.launched_on("claude:pro", &pro).unwrap().name, "work");
        assert_eq!(a.launched_on("claude:perso", &perso).unwrap().name, "me", "in the default home");
        assert_eq!(a.launched_on("claude:work", &[]).unwrap().name, "work", "its name first");
        assert!(a.launched_on("codex:gone", &[("CODEX_HOME".into(), "/tmp/kraal-test-nowhere".into())]).is_none());
    }

    #[test]
    fn path_rules() {
        let mut a = accounts();
        a.rules.push(Rule {
            workspace: None,
            path: Some("/srv/client".into()),
            agent: Some("claude".into()),
            accounts: vec!["perso".into()],
        });
        let picked = a.candidates("claude", None, Some(Path::new("/srv/client/app")));
        assert_eq!(picked[0].name, "perso");
        assert_eq!(picked.len(), 1);
    }

    #[test]
    fn launch_lines() {
        let a = accounts();
        let pro = a.get("claude", "pro").unwrap();
        let l = launch(&pro, &[], None, Some("abc")).unwrap();
        assert_eq!(l.argv, vec!["claude", "--session-id", "abc"]);
        assert!(l.env.contains(&("CLAUDE_CONFIG_DIR".into(), "/tmp/kraal-test-claude-pro".into())));
        assert!(l.env_remove.contains(&"ANTHROPIC_API_KEY".to_string()));
        let l = launch(&pro, &["--model".into(), "opus".into()], Some("s-1"), None).unwrap();
        assert_eq!(l.argv, vec!["claude", "--resume", "s-1", "--model", "opus"]);
        let perso = a.get("claude", "perso").unwrap();
        let l = launch(&perso, &[], None, None).unwrap();
        assert!(l.env_remove.contains(&"CLAUDE_CONFIG_DIR".to_string()), "default home clears the variable");
        let codex = Accounts::empty().get("codex", "default").unwrap();
        assert_eq!(launch(&codex, &[], Some("t-9"), None).unwrap().argv, vec!["codex", "resume", "t-9"]);
        assert_eq!(login(&pro).unwrap().argv, vec!["claude", "auth", "login"]);
    }

    #[test]
    fn statusline_usage() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"session_id":"x","rate_limits":{"five_hour":{"used_percentage":42.5,"resets_at":1760000000},"seven_day":{"used_percentage":71}}}"#,
        )
        .unwrap();
        let u = usage_from_statusline(&v).unwrap();
        assert_eq!(u.five_hour.as_ref().unwrap().used_percent, 42.5);
        assert_eq!(u.weekly.as_ref().unwrap().used_percent, 71.0);
        assert!(usage_from_statusline(&serde_json::json!({"model":{}})).is_none());
    }

    #[test]
    fn limits_without_a_reset_time_end() {
        let u = Usage {
            five_hour: Some(Window { used_percent: 100.0, resets_at: None }),
            observed_at: 1_000,
            ..Default::default()
        };
        assert_eq!(u.peak(1_000 + 3600), 100.0);
        assert_eq!(u.peak(1_000 + 5 * 3600 + 1), 0.0, "five hours later it has reset");
    }

    #[test]
    fn screen_limits() {
        assert_eq!(limit_from_screen("…\nYou've hit your session limit · resets 3:45pm\n"), Some((false, 100.0)));
        assert_eq!(limit_from_screen("You've used 85% of your weekly limit · resets Mon"), Some((true, 85.0)));
        assert_eq!(limit_from_screen("all good"), None);
    }

    #[test]
    fn uuids_are_v4() {
        let u = new_uuid();
        assert_eq!(u.len(), 36);
        assert_eq!(&u[14..15], "4");
        assert_ne!(u, new_uuid());
    }

    #[test]
    fn save_and_reload() {
        let dir = std::env::temp_dir().join(format!("kraal-accounts-{}", std::process::id()));
        let path = dir.join("accounts.toml");
        let mut a = Accounts::load_from(&path).unwrap();
        a.add(acct("codex", "work", "/tmp/x")).unwrap();
        assert!(a.add(acct("codex", "work", "/tmp/y")).is_err());
        assert!(a.add(acct("nope", "w", "/tmp/z")).is_err());
        a.limits.mode = Mode::Ask;
        a.save().unwrap();
        let b = Accounts::load_from(&path).unwrap();
        assert_eq!(b.limits.mode, Mode::Ask);
        assert_eq!(b.accounts.len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A user's own file: a header, comments, an order that is not kraal's, and keys only another
    /// kraal reads.
    const BY_HAND: &str = "\
# The accounts of the team laptop.

[limits]
mode = \"ask\"          # never move by itself
threshold = 85

# Paid by work.
[[account]]
agent = \"claude\"
name = \"pro\"
home = \"/tmp/kraal-test-claude-pro\"
seat = 4              # read by a newer kraal

[[account]]
name = \"pro\"
agent = \"codex\"
home = \"/tmp/kraal-test-codex-pro\"

# Work first, then mine.
[[rule]]
workspace = \"work\"
accounts = [\"pro\", \"default\"]
until = 2027-01-01    # read by a newer kraal
";

    /// What a file that held `BY_HAND` holds once `change` was made to the accounts.
    fn changed(name: &str, change: impl FnOnce(&mut Accounts) -> Result<()>) -> String {
        let dir = std::env::temp_dir().join(format!("kraal-accounts-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("accounts.toml");
        std::fs::write(&path, BY_HAND).unwrap();
        Accounts::update_at(&path, change).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_dir_all(dir);
        text
    }

    #[test]
    fn a_change_leaves_the_rest_of_the_file_as_the_user_wrote_it() {
        assert_eq!(changed("same", |_| Ok(())), BY_HAND);
        // `kraal accounts limits`: the value changes where it is.
        let text = changed("limits", |all| {
            all.limits.threshold = 70;
            all.limits.mode = Mode::Auto;
            Ok(())
        });
        let expected = BY_HAND.replace("mode = \"ask\"", "mode = \"auto\"").replace("threshold = 85", "threshold = 70");
        assert_eq!(text, expected);
        // Installing the hooks: a table the file did not have comes last.
        let text = changed("statusline", |all| {
            all.claude.statusline = Some("my-line".into());
            Ok(())
        });
        assert_eq!(text, format!("{BY_HAND}\n[claude]\nstatusline = \"my-line\"\n"));
        // `kraal accounts add`: one more account, after the others.
        let text = changed("add", |all| all.add(acct("claude", "perso", "/tmp/kraal-test-claude-perso")));
        let added = "[[account]]\nname = \"perso\"\nagent = \"claude\"\nhome = \"/tmp/kraal-test-claude-perso\"\n";
        assert_eq!(text, BY_HAND.replace("\n# Work first", &format!("\n{added}\n# Work first")));
        // `kraal accounts rm`: the account goes with the comment about it, the header stays.
        let text = changed("rm", |all| {
            all.remove("claude", "pro");
            Ok(())
        });
        assert_eq!(
            text,
            BY_HAND.replace(&BY_HAND[BY_HAND.find("# Paid").unwrap()..BY_HAND.find("[[account]]\nname").unwrap()], "")
        );
        // `kraal accounts rename`: the account keeps its place and what the other kraal reads;
        // the rule Codex's account of that name still needs stays whole, after Claude's own.
        let text = changed("rename", |all| all.rename("claude", "pro", "work"));
        let own =
            "[[rule]]\nworkspace = \"work\"\nagent = \"claude\"\naccounts = [\n    \"work\",\n    \"default\",\n]\n";
        let expected = BY_HAND
            .replace("agent = \"claude\"\nname = \"pro\"", "agent = \"claude\"\nname = \"work\"")
            .replace("\n# Work first", &format!("\n{own}\n# Work first"));
        assert_eq!(text, expected);
    }

    #[test]
    fn a_new_file_is_written_whole_and_one_that_cannot_be_read_is_left_alone() {
        let dir = std::env::temp_dir().join(format!("kraal-accounts-new-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("accounts.toml");
        let add = |all: &mut Accounts| all.add(acct("codex", "work", "/tmp/kraal-test-codex-work"));
        Accounts::update_at(&path, add).unwrap();
        let whole = "rule = []\n\n[limits]\nthreshold = 90\nmode = \"notify\"\n\n[claude]\n\n\
                     [[account]]\nname = \"work\"\nagent = \"codex\"\nhome = \"/tmp/kraal-test-codex-work\"\n";
        assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("{HEADER}{whole}"));
        // The file `kraal accounts edit` makes when there is none takes its first account
        // under its header.
        std::fs::remove_file(&path).unwrap();
        Accounts::load_from(&path).unwrap().save().unwrap();
        Accounts::update_at(&path, add).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(HEADER) && text.contains("[[account]]\nname = \"work\"\n"), "{text}");
        assert_eq!(Accounts::load_from(&path).unwrap().accounts, [acct("codex", "work", "/tmp/kraal-test-codex-work")]);

        let broken = "# mine\n[[account]\nname = \"work\"\n";
        std::fs::write(&path, broken).unwrap();
        assert!(Accounts::update_at(&path, add).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn keys_the_accounts_file_does_not_read_are_warnings() {
        let dir = std::env::temp_dir().join(format!("kraal-accounts-unknown-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("accounts.toml");
        std::fs::write(
            &path,
            "[limits]\ntreshold = 80\n\n[[account]]\nname = \"pro\"\nagent = \"claude\"\nhome = \"/tmp/x\"\n\
             notes = \"team plan\"\n\n[[rule]]\nworkspce = \"shop\"\naccounts = [\"pro\"]\n",
        )
        .unwrap();
        let (a, warnings) = Accounts::read(&path).unwrap();
        assert_eq!(
            warnings,
            [
                "account[0].notes: unknown key (did you mean note?)",
                "limits.treshold: unknown key (did you mean threshold?)",
                "rule[0].workspce: unknown key (did you mean workspace?)",
            ]
        );
        assert_eq!((a.limits.threshold, a.accounts.len(), a.rules[0].workspace.as_deref()), (90, 1, None));
        // What kraal writes, every field set, it reads.
        let mut full = Accounts::empty();
        full.path = path.clone();
        full.limits = Limits { threshold: 80, mode: Mode::Auto };
        full.claude.statusline = Some("my-statusline".into());
        full.accounts =
            vec![Account { threshold: Some(70), note: Some("team plan".into()), ..acct("claude", "pro", "/tmp/x") }];
        full.rules = vec![Rule {
            workspace: Some("shop".into()),
            path: Some("~/src/shop".into()),
            agent: Some("claude".into()),
            accounts: vec!["pro".into()],
        }];
        full.save().unwrap();
        let (again, warnings) = Accounts::read(&path).unwrap();
        assert_eq!(warnings, [] as [&str; 0]);
        assert_eq!(
            (again.accounts, again.rules, again.claude.statusline),
            (full.accounts, full.rules, full.claude.statusline)
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// An agent may say anything of when its limits reset, and a usage file anything of when it was
    /// seen: nothing overflows working out how long that is.
    #[test]
    fn usage_any_time_says_is_shown() {
        let now = 1_700_000_000;
        for at in [i64::MIN, i64::MAX, -1] {
            assert!(describe_reset(Some(at), now).starts_with("resets in"));
            assert!(describe_reset(Some(now), at).starts_with("resets in"));
            let usage = Usage {
                five_hour: Some(Window { used_percent: 10.0, resets_at: None }),
                weekly: None,
                observed_at: at,
                source: String::new(),
            };
            let _ = usage.peak(now);
        }
    }

    /// What agents say of their usage (Claude Code's status line payload, the usage files kraal keeps,
    /// limit messages on a screen), any of it: read and shown without panicking.
    #[test]
    fn fuzz_accounts_usage() {
        crate::fuzz::run("accounts_usage", 300, |rng| {
            let number = |rng: &mut crate::fuzz::Rng| -> String {
                match rng.below(5) {
                    0 => (rng.number() as i64).to_string(),
                    1 => format!("{}.5", rng.number() as i64),
                    2 => format!("{:e}", rng.number() as f64 * 1e300),
                    3 => "null".into(),
                    _ => (rng.below(200) as f64 - 50.0).to_string(),
                }
            };
            let window = |rng: &mut crate::fuzz::Rng| -> String {
                format!("{{\"used_percentage\":{},\"resets_at\":{}}}", number(rng), number(rng))
            };
            let (a, b) = (window(rng), window(rng));
            let mut payload = format!("{{\"rate_limits\":{{\"five_hour\":{a},\"seven_day\":{b}}}}}").into_bytes();
            if rng.one_in(3) {
                rng.mutate(&mut payload, &[b"{", b"}", b":", b",", b"\"", b"9e999", b"-"]);
            }
            let now = rng.number() as i64 >> rng.below(40);
            let value: serde_json::Value = serde_json::from_slice(&payload).unwrap_or_default();
            if let Some(usage) = usage_from_statusline(&value) {
                let _ = usage.peak(now);
                let _ = describe_reset(usage.next_reset(now), now);
                for w in usage.five_hour.iter().chain(usage.weekly.iter()) {
                    let _ = describe_reset(w.resets_at, now);
                }
                let _ = crate::server::usage_text(&usage, now);
            }
            // A usage file, as kraal wrote it or damaged.
            let mut saved = serde_json::to_vec(&Usage {
                five_hour: Some(Window { used_percent: 50.0, resets_at: Some(rng.number() as i64) }),
                weekly: None,
                observed_at: rng.number() as i64,
                source: "statusline".into(),
            })
            .unwrap();
            rng.mutate(&mut saved, &[b"9223372036854775807", b"-9223372036854775808", b"null"]);
            if let Ok(mut usage) = serde_json::from_slice::<Usage>(&saved) {
                let _ = usage.peak(now);
                let _ = usage.next_reset(now);
                usage.observe_screen(rng.one_in(2), 85.0, now);
                let _ = crate::server::usage_text(&usage, now);
            }
            let screen = String::from_utf8_lossy(&rng.text(400)).into_owned()
                + *rng.pick(&["You've used 85% of your weekly limit", "used 1e309% of your session limit", "used -5%"]);
            let _ = limit_from_screen(&screen);
        });
    }
}
