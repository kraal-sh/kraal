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
    pub name: &'static str,
    pub command: &'static str,
    /// Environment variable selecting the config home.
    pub home_env: &'static str,
    pub default_home: &'static str,
    /// Arguments of the agent's own login command.
    pub login: &'static [&'static str],
    /// Arguments that resume a conversation; `{session}` is replaced by its id.
    pub resume: &'static [&'static str],
    /// Flag that sets the id of a new conversation, so kraal knows it without reading files.
    pub session_flag: Option<&'static str>,
    /// Entries of the default home shared with every other account (symlinked).
    pub shared: &'static [&'static str],
    /// Entries copied once into a new account home.
    pub copied: &'static [&'static str],
    /// Variables that would override the account's login; cleared in the pane.
    pub clear_env: &'static [&'static str],
}

pub const AGENTS: &[AgentKind] = &[
    AgentKind {
        name: "claude",
        command: "claude",
        home_env: "CLAUDE_CONFIG_DIR",
        default_home: "~/.claude",
        login: &["auth", "login"],
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Limits {
    /// Usage (percent of the 5-hour or weekly window) at which an account counts as full.
    #[serde(default = "default_threshold")]
    pub threshold: u8,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub name: String,
    pub agent: String,
    /// Config home; the agent's default home means "the account you already have".
    pub home: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Which accounts to use, in order, for a workspace or a directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    pub accounts: Vec<String>,
}

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

#[derive(Debug, Clone)]
pub struct Accounts {
    pub limits: Limits,
    pub claude: ClaudeOptions,
    pub accounts: Vec<Account>,
    pub rules: Vec<Rule>,
    path: PathBuf,
}

impl Accounts {
    pub fn path() -> PathBuf {
        paths::config_dir().join("accounts.toml")
    }

    pub fn load() -> Result<Accounts> {
        Self::load_from(&Self::path())
    }

    pub fn load_from(path: &Path) -> Result<Accounts> {
        let file: File = match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => File::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Accounts {
            limits: file.limits,
            claude: file.claude,
            accounts: file.accounts,
            rules: file.rules,
            path: path.to_path_buf(),
        })
    }

    pub fn empty() -> Accounts {
        Accounts {
            limits: Limits::default(),
            claude: ClaudeOptions::default(),
            accounts: Vec::new(),
            rules: Vec::new(),
            path: Self::path(),
        }
    }

    pub fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let file = File {
            limits: self.limits.clone(),
            claude: self.claude.clone(),
            accounts: self.accounts.clone(),
            rules: self.rules.clone(),
        };
        let tmp = self.path.with_extension("toml.tmp");
        std::fs::write(&tmp, format!("{HEADER}{}", toml::to_string_pretty(&file)?))?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    /// Accounts of an agent, with an implicit "default" account for the agent's default home
    /// when no listed account uses it.
    pub fn for_agent(&self, agent: &str) -> Vec<Account> {
        let mut out: Vec<Account> = self.accounts.iter().filter(|a| a.agent == agent).cloned().collect();
        if let Some(kind) = agent_kind(agent) {
            let default = paths::expand_tilde(kind.default_home);
            if !out.iter().any(|a| paths::expand_tilde(&a.home) == default) {
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

    pub fn threshold(&self, a: &Account) -> u8 {
        a.threshold.unwrap_or(self.limits.threshold)
    }

    pub fn add(&mut self, account: Account) -> Result<()> {
        let kind = agent_kind(&account.agent)
            .with_context(|| format!("unknown agent {:?} (known: {})", account.agent, agent_names()))?;
        if !valid_name(&account.name) {
            bail!("invalid account name {:?} (letters, digits, - _ . only)", account.name);
        }
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
        self.rules.iter_mut().for_each(|r| r.accounts.retain(|n| n != name));
        before != self.accounts.len()
    }

    /// Accounts to try, in order, for an agent started in `workspace` and `cwd`.
    pub fn candidates(&self, agent: &str, workspace: Option<&str>, cwd: Option<&Path>) -> Vec<Account> {
        self.rule_accounts(agent, workspace, cwd).unwrap_or_else(|| self.for_agent(agent))
    }

    /// The accounts of the first rule matching this context, if any.
    pub fn rule_accounts(&self, agent: &str, workspace: Option<&str>, cwd: Option<&Path>) -> Option<Vec<Account>> {
        let all = self.for_agent(agent);
        for rule in &self.rules {
            if rule.agent.as_deref().is_some_and(|a| a != agent) {
                continue;
            }
            let ws_ok = rule.workspace.as_deref().is_none_or(|w| Some(w) == workspace);
            let path_ok =
                rule.path.as_deref().is_none_or(|p| cwd.is_some_and(|c| c.starts_with(paths::expand_tilde(p))));
            if rule.workspace.is_none() && rule.path.is_none() {
                continue;
            }
            if ws_ok && path_ok {
                let picked: Vec<Account> =
                    rule.accounts.iter().filter_map(|n| all.iter().find(|a| &a.name == n).cloned()).collect();
                if !picked.is_empty() {
                    return Some(picked);
                }
            }
        }
        None
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

    /// Another account of the same agent to continue on, if one is under its threshold.
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

/// Short label, e.g. `claude·pro`.
pub fn label(a: &Account) -> String {
    format!("{}·{}", a.agent, a.name)
}

pub fn key(agent: &str, name: &str) -> String {
    format!("{agent}:{name}")
}

/// Where a new account's home goes by default.
pub fn default_home_for(agent: &str, name: &str) -> PathBuf {
    dirs::data_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/share")))
        .unwrap_or_else(std::env::temp_dir)
        .join("kraal/accounts")
        .join(format!("{agent}-{name}"))
}

/// Create an account home: shared entries are symlinked from the agent's default home, a few
/// files are copied. Credentials are never touched; the agent's login creates them.
pub fn prepare_home(a: &Account) -> Result<Vec<String>> {
    let kind = agent_kind(&a.agent).context("unknown agent")?;
    let home = paths::expand_tilde(&a.home);
    let default = paths::expand_tilde(kind.default_home);
    let mut notes = Vec::new();
    if home == default {
        return Ok(notes);
    }
    std::fs::create_dir_all(&home).with_context(|| format!("creating {}", home.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700));
    }
    for entry in kind.shared {
        let src = default.join(entry);
        let dst = home.join(entry);
        if !src.exists() || dst.symlink_metadata().is_ok() {
            continue;
        }
        #[cfg(unix)]
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
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
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

/// The agent's own login command for an account.
pub fn login(a: &Account) -> Result<Launch> {
    let kind = agent_kind(&a.agent).context("unknown agent")?;
    let mut l = launch(a, &[], None, None)?;
    l.argv = std::iter::once(kind.command.to_string()).chain(kind.login.iter().map(|s| s.to_string())).collect();
    Ok(l)
}

/// A random UUID v4 (for `--session-id`).
pub fn new_uuid() -> String {
    let mut b = [0u8; 16];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        use std::io::Read;
        let _ = f.read_exact(&mut b);
    } else {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
        b.copy_from_slice(&t.to_le_bytes());
    }
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &h[0..8], &h[8..12], &h[12..16], &h[16..20], &h[20..32])
}

// ---- usage -----------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub used_percent: f64,
    /// Unix seconds.
    pub resets_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Usage {
    pub five_hour: Option<Window>,
    pub weekly: Option<Window>,
    /// Unix seconds when this was observed.
    pub observed_at: i64,
    /// "statusline", "app-server", "rollout", "screen".
    pub source: String,
}

impl Usage {
    fn windows(&self) -> impl Iterator<Item = &Window> {
        self.five_hour.iter().chain(self.weekly.iter())
    }

    /// Highest usage among windows that have not reset yet.
    pub fn peak(&self, now: i64) -> f64 {
        self.windows().filter(|w| w.resets_at.is_none_or(|r| r > now)).map(|w| w.used_percent).fold(0.0, f64::max)
    }

    /// When the fullest window resets.
    pub fn next_reset(&self, now: i64) -> Option<i64> {
        self.windows()
            .filter(|w| w.resets_at.is_some_and(|r| r > now))
            .max_by(|a, b| a.used_percent.total_cmp(&b.used_percent))
            .and_then(|w| w.resets_at)
    }
}

pub fn usage_path(agent: &str, name: &str) -> PathBuf {
    paths::state_dir().join("usage").join(format!("{agent}-{name}.json"))
}

pub fn save_usage(agent: &str, name: &str, u: &Usage) -> Result<()> {
    let path = usage_path(agent, name);
    if let Some(d) = path.parent() {
        paths::ensure_private_dir(d)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(u)?)?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

pub fn load_usage(agent: &str, name: &str) -> Option<Usage> {
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

/// "16:40", "Mon 09:00" style reset time for display (local time via `date` is not needed:
/// show the remaining time instead).
pub fn describe_reset(resets_at: Option<i64>, now: i64) -> String {
    let Some(r) = resets_at else { return String::new() };
    let secs = (r - now).max(0);
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
}
