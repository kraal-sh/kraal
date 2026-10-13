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

mod migration;
use migration::{legacy_usage_name, migrate_usage};

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
    pub clear_env: &'static [&'static str],
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
    /// Stable identity; absent in files written before account identities were introduced.
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

impl Account {
    pub fn key(&self) -> String {
        key(&self.agent, &format!("@{}", self.id))
    }

    fn matches(&self, reference: &str) -> bool {
        reference.strip_prefix('@').is_some_and(|id| id == self.id)
            || reference == self.key()
            || reference == key(&self.agent, &self.name)
            || reference == self.name
    }

    pub fn same_home(&self, other: &Account) -> bool {
        let home = |a: &Account| {
            let path = paths::expand_tilde(&a.home);
            std::fs::canonicalize(&path).unwrap_or(path)
        };
        self.agent == other.agent && home(self) == home(other)
    }
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    limits: Limits,
    #[serde(default, skip_serializing_if = "ClaudeOptions::is_empty")]
    claude: ClaudeOptions,
    #[serde(default, rename = "account")]
    accounts: Vec<Account>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    forgotten: Vec<Account>,
    #[serde(default, rename = "rule", skip_serializing_if = "Vec::is_empty")]
    rules: Vec<Rule>,
}

impl File {
    fn expanded_accounts(mut self) -> Vec<Account> {
        migration::identify(&mut self);
        let mut accounts = Vec::new();
        for mut account in self.accounts {
            let references = std::mem::take(&mut account.references);
            let id = account.id.clone();
            accounts.push(account);
            for mut reference in references {
                reference.linked_to = Some(id.clone());
                accounts.push(reference);
            }
        }
        accounts.extend(self.forgotten);
        accounts
    }
}

// Move whole tables so comments and keys unknown to this kraal follow their reference.
fn expanded_document(source: &str) -> Result<String> {
    use toml_edit::{ArrayOfTables, Item};
    let tables = |item: Item| -> Result<ArrayOfTables> {
        if item.as_array().is_some_and(|array| array.is_empty()) {
            Ok(ArrayOfTables::new())
        } else {
            item.into_array_of_tables().map_err(|_| anyhow::anyhow!("invalid account reference tables"))
        }
    };
    let mut doc: toml_edit::DocumentMut = source.parse()?;
    let stored = doc.get("account").cloned().map(tables).transpose()?.unwrap_or_default();
    if doc.get("forgotten").is_none() && stored.iter().all(|t| !t.contains_key("references")) {
        return Ok(source.to_string());
    }
    let mut accounts = ArrayOfTables::new();
    doc.remove("account");
    for mut table in stored {
        let mut account: Account = toml::from_str(&table.to_string())?;
        migration::identify_account(&mut account);
        let id = account.id;
        let references = table.remove("references");
        accounts.push(table);
        if let Some(item) = references {
            for mut reference in tables(item)? {
                reference.insert("linked_to", toml_edit::value(&id));
                accounts.push(reference);
            }
        }
    }
    if let Some(item) = doc.remove("forgotten") {
        accounts.extend(tables(item)?);
    }
    if !accounts.is_empty() {
        doc.insert("account", Item::ArrayOfTables(accounts));
    }
    Ok(doc.to_string())
}

// A moved table and all its children follow their new parent, keeping their comments.
fn clear_positions(table: &mut toml_edit::Table) {
    use toml_edit::visit_mut::{self, VisitMut};
    struct Unplaced;
    impl VisitMut for Unplaced {
        fn visit_table_mut(&mut self, table: &mut toml_edit::Table) {
            table.set_position(None);
            visit_mut::visit_table_mut(self, table);
        }
    }
    Unplaced.visit_table_mut(table);
}

const HEADER: &str = "# kraal agent accounts. One [[account]] per subscription: a name, the agent (claude, codex)\n\
# and its config home. [[rule]] picks accounts by workspace or directory, in order; [limits] sets\n\
# the usage threshold (percent) and what happens when an account reaches it: notify, ask or auto.\n\n";

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
        let mut accounts = Accounts {
            limits: file.limits.clone(),
            claude: file.claude.clone(),
            rules: file.rules.clone(),
            accounts: file.expanded_accounts(),
            path: path.to_path_buf(),
            source,
        };
        let mut ids = std::collections::HashSet::new();
        for a in &mut accounts.accounts {
            anyhow::ensure!(!a.id.is_empty(), "accounts predate kraal 0.4; run kraal 0.4 once first");
            anyhow::ensure!(
                valid_name(&a.id) && a.linked_to.as_deref().is_none_or(valid_name) && ids.insert(a.id.clone()),
                "invalid or duplicate account identity in {}; the file is unchanged",
                path.display()
            );
        }
        let mut effective_ids = std::collections::HashSet::new();
        anyhow::ensure!(
            accounts.known().iter().all(|a| effective_ids.insert(a.id.clone())),
            "duplicate account identity in {}; the file is unchanged",
            path.display()
        );
        accounts.join_same_homes()?;
        accounts.bind_rules();
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
        let mut file = File {
            limits: self.limits.clone(),
            claude: self.claude.clone(),
            accounts: self.accounts.clone(),
            forgotten: Vec::new(),
            rules: self.rules.clone(),
        };
        let text = match &self.source {
            Some(text) => crate::rewrite::rewrite(&expanded_document(text)?, &file)?,
            None => format!("{HEADER}{}", toml::to_string_pretty(&file)?),
        };
        let text = self
            .compact_document(&text)
            .and_then(migration::without_empty_sections)
            .with_context(|| format!("editing {}", self.path.display()))?;
        // Compaction changes ordering and flattens links, but must preserve what kraal reads.
        let mut read: File = toml::from_str(&text).context("reading the compacted accounts")?;
        read.accounts = read.clone().expanded_accounts();
        read.forgotten.clear();
        for a in &mut file.accounts {
            if a.linked_to.is_some()
                && let Some(target) = self.resolve(a)
            {
                a.linked_to = Some(target.id);
            }
        }
        file.accounts.sort_by(|a, b| a.id.cmp(&b.id));
        read.accounts.sort_by(|a, b| a.id.cmp(&b.id));
        anyhow::ensure!(
            toml::Table::try_from(&read)? == toml::Table::try_from(&file)?,
            "kraal cannot compact this accounts file as it is laid out"
        );
        Ok(text)
    }

    fn compact_document(&self, text: &str) -> Result<String> {
        if self.accounts.iter().all(|a| a.linked_to.is_none()) {
            return Ok(text.to_string());
        }
        use toml_edit::{ArrayOfTables, Item};
        let mut doc: toml_edit::DocumentMut = text.parse()?;
        let Some(item) = doc.remove("account") else { return Ok(doc.to_string()) };
        let tables = item.into_array_of_tables().map_err(|_| anyhow::anyhow!("invalid account tables"))?;
        let mut kept = ArrayOfTables::new();
        let mut references: HashMap<String, ArrayOfTables> = HashMap::new();
        let mut forgotten = ArrayOfTables::new();
        for mut table in tables {
            let id = table.get("id").and_then(Item::as_str).context("account identity missing")?;
            let a = self.accounts.iter().find(|a| a.id == id).context("account identity changed")?;
            if a.linked_to.is_none() {
                kept.push(table);
            } else {
                clear_positions(&mut table);
                if let Some(target) = self.resolve(a) {
                    table.remove("linked_to");
                    references.entry(target.id).or_default().push(table);
                } else {
                    forgotten.push(table);
                }
            }
        }
        for table in kept.iter_mut() {
            let id = table.get("id").and_then(Item::as_str).unwrap();
            if let Some(references) = references.remove(id) {
                table.insert("references", Item::ArrayOfTables(references));
            }
        }
        if !kept.is_empty() {
            doc.insert("account", Item::ArrayOfTables(kept));
        }
        if !forgotten.is_empty() {
            doc.insert("forgotten", Item::ArrayOfTables(forgotten));
        }
        Ok(doc.to_string())
    }

    pub fn save(&self) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = self.text()?;
        migration::backup(self, &text)?;
        paths::write_atomic(&self.path, text.as_bytes()).with_context(|| format!("writing {}", self.path.display()))
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
            let joined = all.source.as_ref().and_then(|s| toml::from_str::<File>(s).ok()).is_some_and(|old| {
                old.accounts.iter().any(|a| a.linked_to.is_some())
                    || old.expanded_accounts().iter().zip(&all.accounts).any(|(a, b)| a.linked_to != b.linked_to)
            });
            let previous = all.clone();
            let out = f(&mut all)?;
            all.bind_rules();
            if joined || all.text()? != before {
                if path == Self::path() {
                    for a in previous.known() {
                        migrate_usage(&usage_dir(), &a)?;
                    }
                    crate::persist::bind_accounts(&previous)
                        .context("could not migrate saved account references; accounts.toml is unchanged; check the saved session and retry")?;
                    for dir in crate::config::Config::load()?.workspace_dirs() {
                        for (file, definition) in crate::workspace::load_dir(&dir) {
                            if definition.is_ok() {
                                crate::workspace::bind_accounts(&file, &previous).with_context(|| {
                                    format!(
                                        "could not migrate {}; accounts.toml is unchanged; check this file and retry",
                                        file.display()
                                    )
                                })?;
                            }
                        }
                    }
                }
                all.save()?;
            }
            Ok(out)
        })?
    }

    /// Confirmed accounts, with one entry per home or reviewed identity.
    pub fn for_agent(&self, agent: &str) -> Vec<Account> {
        let mut out: Vec<Account> = Vec::new();
        for a in self
            .accounts
            .iter()
            .filter(|a| a.agent == agent && a.linked_to.is_none() && !a.review && a.name != "default")
        {
            if !out.iter().any(|other| other.same_home(a)) {
                out.push(a.clone());
            }
        }
        out
    }

    pub fn all(&self) -> Vec<Account> {
        AGENTS.iter().flat_map(|k| self.for_agent(k.name)).collect()
    }

    pub fn reviews(&self) -> Vec<Account> {
        self.accounts.iter().filter(|a| a.review && a.linked_to.is_none()).cloned().collect()
    }

    fn standard(agent: &str) -> Option<Account> {
        let kind = agent_kind(agent)?;
        Some(Account {
            id: format!("default-{agent}"),
            name: "default".into(),
            agent: agent.into(),
            home: kind.default_home.into(),
            threshold: None,
            note: None,
            sign_in_required: false,
            review: false,
            public_identity: None,
            linked_to: None,
            references: Vec::new(),
        })
    }

    /// Unlisted homes are checked off the event loop, never inferred from credential files.
    pub fn unlisted(&self) -> Vec<Account> {
        AGENTS
            .iter()
            .filter_map(|kind| {
                let standard = Self::standard(kind.name)?;
                let found = self
                    .accounts
                    .iter()
                    .find(|a| a.same_home(&standard) || a.agent == kind.name && a.name == "default");
                match found {
                    Some(a) if a.name == "default" && a.linked_to.is_none() && !a.review => Some(a.clone()),
                    None => Some(standard),
                    _ => None,
                }
            })
            .collect()
    }

    fn known(&self) -> Vec<Account> {
        let mut all = self.accounts.clone();
        for kind in AGENTS {
            let standard = Self::standard(kind.name).unwrap();
            if !all.iter().any(|a| a.agent == kind.name && (a.same_home(&standard) || a.name == "default")) {
                all.push(standard);
            }
        }
        all
    }

    fn resolve(&self, account: &Account) -> Option<Account> {
        let mut a = account;
        for _ in 0..=self.accounts.len() {
            match &a.linked_to {
                Some(id) => a = self.accounts.iter().find(|other| &other.id == id && other.agent == a.agent)?,
                None => {
                    return Some(a.clone());
                }
            }
        }
        None
    }

    pub fn get(&self, agent: &str, name: &str) -> Option<Account> {
        let all = self.known();
        if let Some(a) = all.iter().find(|a| a.agent == agent && a.linked_to.is_none() && a.matches(name)) {
            return self.resolve(a);
        }
        if let Some(a) = all.iter().find(|a| a.agent == agent && a.matches(name)) {
            return self.resolve(a);
        }
        if name == format!("@default-{agent}") || name == format!("{agent}:@default-{agent}") {
            return self.by_id(&format!("default-{agent}"));
        }
        if name == "default" || name == key(agent, "default") {
            let standard = Self::standard(agent)?;
            return all.iter().filter(|a| a.same_home(&standard)).find_map(|a| self.resolve(a));
        }
        None
    }

    pub fn by_id(&self, id: &str) -> Option<Account> {
        if let Some(a) = self.known().iter().find(|a| a.id == id) {
            return self.resolve(a);
        }
        let agent = id.strip_prefix("default-")?;
        let standard = Self::standard(agent)?;
        self.accounts
            .iter()
            .find(|a| a.same_home(&standard) || a.agent == agent && a.name == "default")
            .and_then(|a| self.resolve(a))
    }

    fn join_same_homes(&mut self) -> Result<()> {
        for i in 0..self.accounts.len() {
            let a = &self.accounts[i];
            if a.linked_to.is_some() {
                continue;
            }
            let target = self
                .accounts
                .iter()
                .enumerate()
                .find(|(j, b)| {
                    *j != i
                        && b.linked_to.is_none()
                        && a.same_home(b)
                        && (*j < i || a.name == "default" && b.name != "default")
                })
                .map(|(_, b)| b.id.clone());
            self.accounts[i].linked_to = target;
        }
        for a in &self.accounts {
            let mut seen = std::collections::HashSet::new();
            let mut next = Some(a);
            while let Some(a) = next {
                anyhow::ensure!(seen.insert(&a.id), "account links form a cycle; the file is unchanged");
                next = a.linked_to.as_ref().and_then(|id| self.accounts.iter().find(|b| &b.id == id));
                anyhow::ensure!(
                    next.is_none_or(|b| b.agent == a.agent),
                    "account links must use the same agent; the file is unchanged"
                );
            }
        }
        Ok(())
    }

    pub fn proposed_name(&self, agent: &str, identity: Option<&str>) -> String {
        identity
            .filter(|name| {
                valid_account_name(name) && *name != "default" && self.get(agent, name).is_none_or(|a| a.review)
            })
            .unwrap_or_default()
            .to_string()
    }

    fn adopted_name(&self, agent: &str, identity: Option<&str>) -> String {
        let base = identity.filter(|name| valid_account_name(name) && *name != "default").unwrap_or("Personal");
        let mut name = base.to_string();
        let mut suffix = 2;
        while self.accounts.iter().any(|a| a.agent == agent && a.name == name) {
            name = format!("{base}{suffix}");
            suffix += 1;
        }
        name
    }

    pub fn adopt_checked(&mut self, checked: &Account, identity: Option<&str>) -> Result<()> {
        let Some(mut a) = self.unlisted().into_iter().find(|a| a.id == checked.id && a.same_home(checked)) else {
            return Ok(());
        };
        let old_default = (!Self::standard(&a.agent).is_some_and(|standard| a.same_home(&standard))).then(|| a.clone());
        if old_default.is_some() {
            a.id = new_uuid();
        }
        a.name = self.adopted_name(&a.agent, identity);
        a.sign_in_required = false;
        a.review = !self.for_agent(&a.agent).is_empty();
        a.public_identity = identity.filter(|name| a.review && valid_account_name(name)).map(String::from);
        if let Some(old_default) = old_default {
            let old = self.accounts.iter_mut().find(|old| old.id == old_default.id).unwrap();
            old.linked_to = Some(a.id.clone());
        } else {
            // Keep the old name and its usage reference when the visible label changes.
            self.accounts.push(Account { id: new_uuid(), linked_to: Some(a.id.clone()), ..checked.clone() });
        }
        if let Some(old) = self.accounts.iter_mut().find(|old| old.id == a.id) {
            *old = a;
        } else {
            self.accounts.push(a);
        }
        Ok(())
    }

    pub fn review(&mut self, existing: &Account, target: Option<&Account>) -> Result<Account> {
        let a = self
            .accounts
            .iter()
            .find(|a| a.id == existing.id && a.review && a.linked_to.is_none() && a.same_home(existing))
            .context("this review changed; open Review again")?
            .clone();
        let target = match target {
            Some(target) => self
                .for_agent(&a.agent)
                .into_iter()
                .find(|b| b.id == target.id && b.same_home(target))
                .context("the chosen account changed; open Review again")?,
            None => Account { review: false, public_identity: None, ..a.clone() },
        };
        self.bind_rules();
        let old = self.accounts.iter_mut().find(|b| b.id == a.id).unwrap();
        old.review = false;
        old.public_identity = None;
        old.linked_to = (a.id != target.id).then(|| target.id.clone());
        Ok(target)
    }

    fn bind_rules(&mut self) {
        let all: Vec<Account> = self.known().iter().filter_map(|a| self.resolve(a)).collect();
        let lookup = self.clone();
        for rule in &mut self.rules {
            // A name kept while its account did not exist binds as soon as it can be resolved.
            // References already bound keep their identity, even after that account is removed.
            let ids: Vec<String> = rule
                .account_ids
                .as_ref()
                .unwrap_or(&rule.accounts)
                .iter()
                .flat_map(|reference| {
                    let found: Vec<String> = all
                        .iter()
                        .filter(|a| {
                            rule.agent.as_deref().is_none_or(|agent| a.agent == agent)
                                && lookup.get(&a.agent, reference).is_some_and(|b| b.id == a.id)
                        })
                        .map(Account::key)
                        .collect();
                    if found.is_empty() { vec![reference.clone()] } else { found }
                })
                .collect();
            let mut names = Vec::new();
            for reference in &ids {
                let name =
                    all.iter().find(|a| a.matches(reference)).map_or_else(|| reference.clone(), |a| a.name.clone());
                if !names.contains(&name) {
                    names.push(name);
                }
            }
            rule.accounts = names;
            let mut unique = std::collections::HashSet::new();
            let ids = ids.into_iter().filter(|id| unique.insert(id.clone())).collect();
            rule.account_ids = Some(ids);
        }
    }

    pub fn set_sign_in_required(&mut self, account: &Account, required: bool) {
        if self.by_id(&account.id).is_none() {
            return;
        }
        let mut found = false;
        for a in &mut self.accounts {
            if a.id == account.id || a.same_home(account) && a.linked_to.is_none() {
                a.sign_in_required = required;
                found = true;
            }
        }
        if required && !found {
            let mut account = account.clone();
            account.sign_in_required = true;
            self.accounts.push(account);
        }
    }

    /// The account whose home is `home` (used to tell which account a status line belongs to).
    pub fn by_home(&self, agent: &str, home: &Path) -> Option<Account> {
        let probe = Account { home: home.display().to_string(), ..Self::standard(agent)? };
        self.known().iter().filter(|a| a.same_home(&probe)).find_map(|a| self.resolve(a))
    }

    /// Associate a live process only when its home selects one account and no credential overrides it.
    pub fn process_account(&self, kind: &AgentKind, env: &HashMap<String, String>) -> Option<Account> {
        if kind.clear_env.iter().any(|k| env.get(*k).is_some_and(|v| !v.is_empty())) {
            return None;
        }
        let home = match env.get(kind.home_env) {
            Some(home) if !home.is_empty() => PathBuf::from(home),
            _ => PathBuf::from(env.get("HOME")?).join(kind.default_home.strip_prefix("~/")?),
        };
        if !home.is_absolute() {
            return None;
        }
        let canonical = home.canonicalize().ok()?;
        let mut matches = self.all().into_iter().filter(|a| {
            a.agent == kind.name && paths::expand_tilde(&a.home).canonicalize().ok().as_ref() == Some(&canonical)
        });
        let mut account = matches.next()?;
        if matches.any(|a| a.id != account.id) {
            return None;
        }
        account.home = home.to_str()?.into();
        Some(account)
    }

    /// Resuming a pane must use the home it launched in, even after its identity was linked.
    pub fn at_launch_home(&self, mut a: Account, env: &[(String, String)]) -> Account {
        if let Some(kind) = agent_kind(&a.agent) {
            let home =
                env.iter().find(|(k, _)| k == kind.home_env).map(|(_, v)| v.as_str()).unwrap_or(kind.default_home);
            if self.by_home(&a.agent, &paths::expand_tilde(home)).is_some_and(|b| a.id == b.id) {
                a.home = home.into();
            }
        }
        a
    }

    /// Stable references never fall back to a reused name or home. Old panes have their
    /// launch environment to distinguish a renamed account from a replacement of its name.
    pub fn launched_on(&self, reference: &str, env: &[(String, String)]) -> Option<Account> {
        if let Some((_, id)) = env.iter().find(|(k, _)| k == "KRAAL_ACCOUNT_ID") {
            return self.by_id(id).map(|a| self.at_launch_home(a, env));
        }
        let (agent, name) = reference.split_once(':')?;
        if name.starts_with('@') {
            return self.get(agent, name).map(|a| self.at_launch_home(a, env));
        }
        let kind = agent_kind(agent)?;
        let home = env
            .iter()
            .find(|(k, _)| k == kind.home_env)
            .map(|(_, home)| PathBuf::from(home))
            .unwrap_or_else(|| paths::expand_tilde(kind.default_home));
        self.get(agent, name)
            .filter(|a| paths::expand_tilde(&a.home) == home)
            .or_else(|| self.by_home(agent, &home))
            .map(|a| self.at_launch_home(a, env))
    }

    pub fn bind_pane(&self, spec: &mut crate::pane::PaneSpec) {
        let Some(reference) = spec.account.as_deref() else { return };
        let account = match spec.account_id.as_deref() {
            Some(id) => self.by_id(id),
            None => self.launched_on(reference, &spec.env),
        };
        if let Some(a) = account {
            spec.account = Some(key(&a.agent, &a.name));
            spec.account_id = Some(a.id.clone());
            spec.env.retain(|(k, _)| k != "KRAAL_ACCOUNT_ID" && k != "KRAAL_ACCOUNT");
            spec.env.push(("KRAAL_ACCOUNT_ID".into(), a.id));
            spec.env.push(("KRAAL_ACCOUNT".into(), key(&a.agent, &a.name)));
        } else if spec.account_id.is_none() {
            // A dangling old reference must not become a new account on a later reload.
            spec.account_id = Some(
                spec.env
                    .iter()
                    .find(|(k, _)| k == "KRAAL_ACCOUNT_ID")
                    .map(|(_, id)| id.clone())
                    .unwrap_or_else(new_uuid),
            );
        }
    }

    pub fn threshold(&self, a: &Account) -> u8 {
        a.threshold.unwrap_or(self.limits.threshold)
    }

    pub fn add(&mut self, mut account: Account) -> Result<Account> {
        let kind = agent_kind(&account.agent)
            .with_context(|| format!("unknown agent {:?} (known: {})", account.agent, agent_names()))?;
        account.home = absolute_home(&account.home)?;
        if account.id.is_empty() {
            account.id = new_uuid();
        }
        anyhow::ensure!(valid_name(&account.id), "invalid account identity");
        anyhow::ensure!(!self.accounts.iter().any(|a| a.id == account.id), "account identity already exists");
        check_name(kind, &account.name)?;
        if self.accounts.iter().any(|a| a.agent == kind.name && a.name == account.name && a.linked_to.is_none()) {
            bail!("{} already has an account named {:?}", kind.name, account.name);
        }
        self.bind_rules();
        if let Some(existing) = self.accounts.iter().filter(|a| a.same_home(&account)).find_map(|a| self.resolve(a)) {
            anyhow::ensure!(
                !existing.review,
                "review the existing account with `kraal accounts review {}`",
                account.agent
            );
            if existing.name == "default" {
                account.id = existing.id;
                let old = self.accounts.iter_mut().find(|a| a.id == account.id).unwrap();
                old.name = account.name.clone();
                old.sign_in_required = account.sign_in_required;
                if account.threshold.is_some() {
                    old.threshold = account.threshold;
                }
                let adopted = old.clone();
                self.accounts.push(Account { id: new_uuid(), linked_to: Some(adopted.id.clone()), ..existing });
                return Ok(adopted);
            }
            account.linked_to = Some(existing.id.clone());
            if let Some(threshold) = account.threshold {
                self.accounts.iter_mut().find(|a| a.id == existing.id).unwrap().threshold = Some(threshold);
            }
            self.accounts.push(account);
            return Ok(self.by_id(&existing.id).unwrap());
        }
        if Self::standard(kind.name).is_some_and(|a| a.same_home(&account))
            && !self.accounts.iter().any(|a| a.id == format!("default-{}", kind.name) || a.same_home(&account))
        {
            account.id = format!("default-{}", kind.name);
            self.accounts.push(Account {
                id: new_uuid(),
                name: "default".into(),
                linked_to: Some(account.id.clone()),
                ..account.clone()
            });
        }
        self.accounts.push(account.clone());
        Ok(account)
    }

    pub fn remove(&mut self, agent: &str, name: &str) -> bool {
        self.bind_rules();
        let Some(account) = self.get(agent, name) else { return false };
        let before = self.accounts.len();
        if Self::standard(agent).is_some_and(|standard| standard.same_home(&account)) {
            let forgotten = new_uuid();
            if let Some(a) = self.accounts.iter_mut().find(|a| a.id == account.id) {
                a.linked_to = Some(forgotten);
                return true;
            }
        }
        self.accounts.retain(|a| a.id != account.id);
        before != self.accounts.len()
    }

    pub fn renamable(&self, agent: &str, name: &str) -> Result<Account> {
        let a = self.get(agent, name).with_context(|| missing_account(agent, name))?;
        anyhow::ensure!(
            self.accounts.iter().any(|listed| listed.id == a.id && !listed.review && listed.name != "default"),
            "check the existing sign-in with `kraal accounts list` first"
        );
        Ok(a)
    }

    /// Rename the label, binding any legacy rules first. Its home and identity stay.
    pub fn rename(&mut self, agent: &str, old: &str, new: &str) -> Result<()> {
        let account = self.renamable(agent, old)?;
        let kind = agent_kind(agent).with_context(|| format!("unknown agent {agent:?} (known: {})", agent_names()))?;
        check_name(kind, new)?;
        if self.get(agent, new).is_some() {
            bail!("{agent} already has an account named {new:?}");
        }
        self.bind_rules();
        for a in self.accounts.iter_mut().filter(|a| a.id == account.id) {
            a.name = new.to_string();
        }
        Ok(())
    }

    /// Accounts to try, in order, for an agent started in `workspace` and `cwd`.
    fn candidates(&self, agent: &str, workspace: Option<&str>, cwd: Option<&Path>) -> Vec<Account> {
        self.rule_accounts(agent, workspace, cwd).unwrap_or_else(|| {
            let listed = self.for_agent(agent);
            if listed.is_empty() { self.get(agent, "default").into_iter().collect() } else { listed }
        })
    }

    /// The accounts of the first rule matching this context, if any. A rule for every agent
    /// concerns an agent when it names one of its accounts, or names no account of any agent
    /// any more: then nothing is allowed here (rather than any account at all).
    fn rule_accounts(&self, agent: &str, workspace: Option<&str>, cwd: Option<&Path>) -> Option<Vec<Account>> {
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
                let references = rule.account_ids.as_ref().unwrap_or(&rule.accounts);
                let picked: Vec<Account> = references.iter().filter_map(|n| self.get(agent, n)).collect();
                let dangling = !references.iter().any(|n| AGENTS.iter().any(|kind| self.get(kind.name, n).is_some()));
                if !picked.is_empty()
                    || rule.agent.is_some()
                    || dangling
                    || references.iter().any(|r| r.starts_with(&format!("{agent}:@")))
                {
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

    /// Prefer measured headroom, then unknown usage, then the first reset among full accounts.
    pub fn choose(
        &self,
        agent: &str,
        workspace: Option<&str>,
        cwd: Option<&Path>,
        usage: &UsageMap,
        now: i64,
    ) -> Option<Account> {
        let candidates = self.candidates(agent, workspace, cwd);
        if let Some(a) = candidates.iter().find(|a| usage.peak(a, now).is_some_and(|p| p < self.threshold(a) as f64)) {
            return Some(a.clone());
        }
        if let Some(a) = candidates.iter().find(|a| usage.peak(a, now).is_none()) {
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
        self.candidates(&current.agent, workspace, cwd).into_iter().find(|a| {
            a.id != current.id
                && !a.same_home(current)
                && !a.sign_in_required
                && !a.review
                && usage.peak(a, now).is_some_and(|p| p < self.threshold(a) as f64)
        })
    }
}

pub fn missing_account(agent: &str, reference: &str) -> String {
    if reference.starts_with('@') || reference.contains(":@") {
        "the account was removed; choose an account again".into()
    } else {
        format!("{agent} has no account named {reference:?}")
    }
}

pub fn agent_names() -> String {
    AGENTS.iter().map(|a| a.name).collect::<Vec<_>>().join(", ")
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn valid_account_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('@')
        && name.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '@' | '+'))
}

/// Keep `default` available for references written by older clients.
fn check_name(kind: &AgentKind, name: &str) -> Result<()> {
    if !valid_account_name(name) {
        bail!("invalid account name {name:?} (letters, digits, - _ . @ + only)");
    }
    if name == "default" {
        bail!("\"default\" is the account in {}; choose an ordinary name", kind.default_home);
    }
    Ok(())
}

/// Rename the account label, keeping its rules and usage on the same identity.
/// Its home stays where it is: agents running on it go on.
pub fn rename(agent: &str, old: &str, new: &str) -> Result<()> {
    rename_in(&Accounts::path(), &usage_dir(), agent, old, new)
}

/// [`rename`] in the accounts file `file`, with the usage saved in `usage`.
fn rename_in(file: &Path, usage: &Path, agent: &str, old: &str, new: &str) -> Result<()> {
    Accounts::update_at(file, |all| {
        let a = all.get(agent, old).with_context(|| missing_account(agent, old))?;
        all.rename(agent, old, new)?;
        migrate_usage(usage, &a)
    })
}

pub fn existing_label(agent: &str) -> String {
    let name = match agent {
        "claude" => "Claude",
        "codex" => "Codex",
        _ => agent,
    };
    format!("Existing {name} account")
}

/// How an account is shown, e.g. `claude·pro`.
pub fn label(a: &Account) -> String {
    format!("{}·{}", a.agent, if a.review || a.name == "default" { "Existing account" } else { &a.name })
}

/// The readable account label kept for older clients and saved sessions.
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
    l.env.push(("KRAAL_ACCOUNT_ID".into(), a.id.clone()));
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

/// Verify the saved conversation before promising a return to it.
pub fn check_resume(a: &Account, session: &str) -> Result<()> {
    anyhow::ensure!(
        !session.is_empty() && session.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
        "could not verify the conversation id"
    );
    if a.agent == "claude" {
        return check_carry(a, a, session);
    }
    fn find(dir: &Path, session: &str, depth: usize) -> Result<bool> {
        use std::io::{BufRead, Read};
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() && depth < 3 && find(&entry.path(), session, depth + 1)? {
                return Ok(true);
            }
            if kind.is_file() && entry.file_name().to_str().is_some_and(|n| n.ends_with(&format!("-{session}.jsonl"))) {
                let file = std::fs::File::open(entry.path())?;
                let mut first = String::new();
                std::io::BufReader::new(file.take(64 * 1024)).read_line(&mut first)?;
                if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&first)
                    && meta["type"] == "session_meta"
                    && meta["payload"]["id"] == session
                {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
    anyhow::ensure!(
        a.agent == "codex" && find(&paths::expand_tilde(&a.home).join("sessions"), session, 0)?,
        "could not find the saved conversation on {}",
        label(a)
    );
    Ok(())
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
    /// This agent cannot move a conversation between accounts.
    Nothing,
    /// `to` sees the newest transcript already (the same file, or a copy holding all of it).
    There,
    /// `to` needs the newest transcript, `from`'s, at that place.
    Copy(PathBuf, PathBuf),
}

fn carry_plan(from: &Account, to: &Account, session: &str) -> Result<Carry> {
    if from.agent != "claude" || to.agent != "claude" {
        return Ok(Carry::Nothing);
    }
    anyhow::ensure!(
        !session.is_empty() && session.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
        "could not verify the Claude conversation id"
    );
    let (src, dst) = (transcript(from, session)?, transcript(to, session)?);
    let src = src.with_context(|| format!("the Claude transcript for {session} is missing on {}", label(from)))?;
    // Even a shared file must be readable before stopping the process that owns it.
    let readable = |path: &Path| -> Result<u64> {
        let mut file = std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
        std::io::copy(&mut file, &mut std::io::sink()).with_context(|| format!("reading {}", path.display()))
    };
    anyhow::ensure!(readable(&src)? > 0, "the Claude transcript for {session} is empty on {}", label(from));
    let Some(dst) = dst else {
        let project = src.parent().and_then(|d| d.file_name()).context("an odd transcript path")?;
        let dst = paths::expand_tilde(&to.home).join("projects").join(project).join(src.file_name().unwrap());
        return Ok(Carry::Copy(src, dst));
    };
    readable(&dst)?;
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

/// The lock lives in the home so aliases and other kraal sessions share it.
pub fn lock_login(a: &Account) -> Result<std::sync::Arc<std::fs::File>> {
    use std::os::unix::fs::OpenOptionsExt;
    let home = paths::expand_tilde(&a.home);
    std::fs::create_dir_all(&home)?;
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(home.join(format!(".kraal-{}-login.lock", a.agent)))?;
    anyhow::ensure!(paths::try_lock(&lock)?, "{}: sign-in already running", label(a));
    Ok(std::sync::Arc::new(lock))
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

/// A random UUID v4 for saved identities and agent sessions.
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

    /// Each window with the time it counts until: its reset or, when it does not say when it
    /// resets, the end of its period after it was seen.
    fn ends(&self) -> impl Iterator<Item = (&Window, i64)> {
        let end = |w: &Window, period: i64| w.resets_at.unwrap_or(self.observed_at.saturating_add(period));
        let five = self.five_hour.iter().map(move |w| (w, end(w, 5 * 3600)));
        let week = self.weekly.iter().map(move |w| (w, end(w, 7 * 24 * 3600)));
        five.chain(week)
    }

    /// Highest usage among windows that have not reset yet.
    pub fn peak(&self, now: i64) -> f64 {
        self.ends().filter(|(_, end)| *end > now).map(|(w, _)| w.used_percent).fold(0.0, f64::max)
    }

    /// When [`Usage::peak`] changes next with nothing new seen: the earliest reset of a window
    /// that counts at `now`.
    pub fn next_end(&self, now: i64) -> Option<i64> {
        self.ends().map(|(_, end)| end).filter(|end| *end > now).min()
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
fn usage_path(agent: &str, id: &str) -> PathBuf {
    usage_dir().join("by-id").join(usage_file(agent, id))
}

pub fn save_usage(a: &Account, u: &Usage) -> Result<()> {
    let path = usage_path(&a.agent, &a.id);
    if let Some(d) = path.parent() {
        paths::ensure_private_dir(d)?;
    }
    paths::write_private(&path, &serde_json::to_vec(u)?)?;
    Ok(())
}

fn load_usage(dir: &Path, agent: &str, id: &str) -> Option<Usage> {
    serde_json::from_slice(&std::fs::read(dir.join("by-id").join(usage_file(agent, id))).ok()?).ok()
}

/// Latest usage per account, keyed by stable identity.
#[derive(Debug, Clone, Default)]
pub struct UsageMap(pub HashMap<String, Usage>);

impl UsageMap {
    pub fn load(accounts: &Accounts) -> UsageMap {
        Self::load_from(accounts, &usage_dir())
    }

    fn load_from(accounts: &Accounts, dir: &Path) -> UsageMap {
        let mut usage = UsageMap::default();
        for a in accounts.known() {
            let Some(target) = accounts.by_id(&a.id) else { continue };
            let _ = migrate_usage(dir, &a);
            if let Some(u) = load_usage(dir, &a.agent, &a.id) {
                usage.observe(&target, u);
            }
            if let Some(name) = legacy_usage_name(&a)
                && let Ok(bytes) = std::fs::read(dir.join(usage_file(&a.agent, name)))
                && let Ok(u) = serde_json::from_slice(&bytes)
            {
                usage.observe(&target, u);
            }
        }
        usage
    }

    fn observe(&mut self, account: &Account, usage: Usage) {
        let key = account.key();
        if self.0.get(&key).is_none_or(|old| usage.observed_at > old.observed_at) {
            self.0.insert(key, usage);
        }
    }

    pub fn get(&self, a: &Account) -> Option<&Usage> {
        self.0.get(&a.key())
    }

    pub fn peak(&self, a: &Account, now: i64) -> Option<f64> {
        self.get(a).filter(|u| u.windows().next().is_some()).map(|u| u.peak(now))
    }

    /// When the usage of an account changes next with nothing new seen: a window resets (see
    /// [`Usage::next_end`]). What shows the accounts' usage at `now` holds until then.
    pub fn next_end(&self, now: i64) -> Option<i64> {
        self.0.values().filter_map(|u| u.next_end(now)).min()
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
    fn linked_files_rewrite_to_one_entry_per_account_with_all_references_and_legacy_fields() {
        let dir = std::env::temp_dir().join(format!("kraal-compact-{}", new_uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("accounts.toml");
        let source = "# My accounts.\n\
[[account]]\nid = 'work'\nname = 'patron.bzh'\nagent = 'claude'\nhome = '/patron'\n\
[[account]]\nid = 'gmail'\nname = 'gmail'\nagent = 'claude'\nhome = '/gmail'\n\
# Keep the old home.\n\
[[account]]\nid = 'old-claude'\nname = 'default'\nagent = 'claude'\nhome = '~/.claude'\nlinked_to = 'default-claude'\nnote = 'mine'\nfuture = 7\n\
[[account]]\nid = 'default-claude'\nname = 'Personal'\nagent = 'claude'\nhome = '~/.claude'\nlinked_to = 'work'\n\
[[account]]\nid = 'old-codex'\nname = 'default'\nagent = 'codex'\nhome = '~/.codex'\nlinked_to = 'default-codex'\n\
[[account]]\nid = 'default-codex'\nname = 'Personal'\nagent = 'codex'\nhome = '~/.codex'\n\
[[rule]]\nworkspace = 'w'\nagent = 'claude'\naccounts = ['default']\n";
        std::fs::write(&path, source).unwrap();
        Accounts::update_at(&path, |_| Ok(())).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert_eq!(saved.matches("[[account]]").count(), 3, "{saved}");
        assert!(!saved.contains("linked_to"), "{saved}");
        for kept in ["# My accounts.", "# Keep the old home.", "note = 'mine'", "future = 7"] {
            assert!(saved.contains(kept), "{saved}");
        }
        let all = Accounts::load_from(&path).unwrap();
        assert_eq!(all.all().len(), 3);
        for id in ["old-claude", "default-claude", "work"] {
            assert_eq!(all.by_id(id).unwrap().name, "patron.bzh", "{saved}");
        }
        assert_eq!(all.get("claude", "default").unwrap().id, "work");
        assert_eq!(all.get("claude", "Personal").unwrap().id, "work");
        assert_eq!(all.by_home("claude", &paths::expand_tilde("~/.claude")).unwrap().id, "work");
        assert_eq!(all.by_id("old-codex").unwrap().name, "Personal");
        assert_eq!(all.choose("claude", Some("w"), None, &UsageMap::default(), 0).unwrap().id, "work");
        let env = vec![("CLAUDE_CONFIG_DIR".into(), paths::expand_tilde("~/.claude").display().to_string())];
        let launched = all.launched_on("claude:default", &env).unwrap();
        assert_eq!(launched.id, "work");
        assert!(launched.same_home(&Accounts::standard("claude").unwrap()));

        #[derive(Deserialize)]
        struct LegacyAccount {
            name: String,
            agent: String,
            home: String,
            threshold: Option<u8>,
        }
        #[derive(Deserialize)]
        struct LegacyFile {
            account: Vec<LegacyAccount>,
        }
        let old: LegacyFile = toml::from_str(&saved).unwrap();
        assert_eq!(old.account.len(), 3);
        for (a, b) in old.account.iter().zip(all.all()) {
            assert_eq!((&a.name, &a.agent, &a.home, a.threshold), (&b.name, &b.agent, &b.home, b.threshold));
        }
        assert_eq!(std::fs::read_to_string(path.with_extension("toml.kraal-bak")).unwrap(), source);
        Accounts::update_at(&path, |all| all.save()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), saved, "rewriting is idempotent");
        Accounts::update_at(&path, |all| {
            all.limits.threshold = 75;
            Ok(())
        })
        .unwrap();
        let changed = std::fs::read_to_string(&path).unwrap();
        assert!(changed.contains("# Keep the old home.") && changed.contains("future = 7"));
        assert_eq!(changed.matches("[[account]]").count(), 3);
        Accounts::update_at(&path, |all| {
            assert!(all.remove("claude", "patron.bzh"));
            Ok(())
        })
        .unwrap();
        let all = Accounts::load_from(&path).unwrap();
        assert!(all.by_id("default-claude").is_none());
        assert!(all.unlisted().iter().all(|a| a.agent != "claude"));
        assert_eq!(all.all().len(), 2);
        assert_eq!(std::fs::read_to_string(&path).unwrap().matches("[[account]]").count(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn compaction_keeps_nested_unknown_fields_with_their_reference() {
        let dir = std::env::temp_dir().join(format!("kraal-compact-nested-{}", new_uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("accounts.toml");
        let source = "[[account]]\nid = 'work'\nname = 'work'\nagent = 'claude'\nhome = '/work'\n\
[[account]]\nid = 'personal'\nname = 'personal'\nagent = 'claude'\nhome = '/personal'\n\
# Keep this reference.\n\
[[account]]\nid = 'old'\nname = 'old'\nagent = 'claude'\nhome = '/old'\nlinked_to = 'work'\n\
[account.future] # Keep this table.\nvalue = 7 # Keep this value.\n\
[account.future.nested]\nlabel = 'nested'\n\
[[account.future.entries]] # Keep this entry.\nname = 'first'\n\
[account.future.entries.options]\nenabled = true\n\
[[account.future.entries]]\nname = 'second'\n";
        std::fs::write(&path, source).unwrap();
        let original: toml::Value = toml::from_str(source).unwrap();
        let future = &original["account"][2]["future"];
        for forgotten in [false, true] {
            Accounts::update_at(&path, |all| {
                if forgotten {
                    assert!(all.remove("claude", "work"));
                }
                Ok(())
            })
            .unwrap();
            let saved = std::fs::read_to_string(&path).unwrap();
            let doc: toml::Value = toml::from_str(&saved).unwrap();
            let references = if forgotten {
                doc["forgotten"].as_array().unwrap()
            } else {
                doc["account"][0]["references"].as_array().unwrap()
            };
            let old = references.iter().find(|a| a["id"].as_str() == Some("old")).unwrap();
            assert_eq!(&old["future"], future, "{saved}");
            let personal =
                doc["account"].as_array().unwrap().iter().find(|a| a["id"].as_str() == Some("personal")).unwrap();
            assert!(personal.get("references").is_none(), "{saved}");
            for comment in ["# Keep this reference.", "# Keep this table.", "# Keep this value.", "# Keep this entry."]
            {
                assert!(saved.contains(comment), "{saved}");
            }
            let all = Accounts::load_from(&path).unwrap();
            assert_eq!(all.all().len(), if forgotten { 1 } else { 2 });
            assert_eq!(all.by_id("old").map(|a| a.id), (!forgotten).then(|| "work".to_string()));
            Accounts::update_at(&path, |_| Ok(())).unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), saved, "rewriting is idempotent");
            Accounts::update_at(&path, |all| {
                all.limits.threshold = 75;
                Ok(())
            })
            .unwrap();
            assert_eq!(Accounts::load_from(&path).unwrap().limits.threshold, 75);
        }
        assert_eq!(std::fs::read_to_string(path.with_extension("toml.kraal-bak")).unwrap(), source);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn inline_references_and_empty_lists_survive_rewriting() {
        let dir = std::env::temp_dir().join(format!("kraal-inline-references-{}", new_uuid()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("accounts.toml");
        for source in [
            "account = [{id = 'work', name = 'work', agent = 'claude', home = '/work', references = [{id = 'old', name = 'old', agent = 'claude', home = '/old', future = 7}]}]\n",
            "account = []\nforgotten = []\n",
            "[[account]]\nid = 'work'\nname = 'work'\nagent = 'claude'\nhome = '/work'\nreferences = []\n",
        ] {
            std::fs::write(&path, source).unwrap();
            Accounts::update_at(&path, |all| {
                all.limits.threshold = 75;
                Ok(())
            })
            .unwrap();
            let saved = std::fs::read_to_string(&path).unwrap();
            let all = Accounts::load_from(&path).unwrap();
            assert_eq!(all.limits.threshold, 75);
            if source.contains("future") {
                assert!(saved.contains("future = 7"), "{saved}");
                assert_eq!(all.get("claude", "old").unwrap().id, "work");
                assert_eq!(all.all().len(), 1);
            }
            Accounts::update_at(&path, |all| all.save()).unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), saved);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn verified_existing_sign_ins_are_ordinary_renameable_accounts() {
        for kind in AGENTS {
            let mut all = Accounts::empty();
            let original = all.get(kind.name, "default").unwrap();
            assert!(all.all().is_empty());
            all.adopt_checked(&original, None).unwrap();
            assert_eq!(all.for_agent(kind.name).len(), 1);
            let saved: toml::Value = toml::from_str(&all.text().unwrap()).unwrap();
            assert_eq!(saved["account"].as_array().unwrap().len(), 1);
            let adopted = all.get(kind.name, "Personal").unwrap();
            assert_eq!(adopted.id, original.id);
            assert_eq!(adopted.home, original.home);
            assert!(!adopted.sign_in_required);
            let legacy = all.accounts.iter().find(|a| a.agent == kind.name && a.name == "default").unwrap();
            assert_eq!(legacy.home, original.home);
            assert_eq!(legacy.linked_to.as_deref(), Some(adopted.id.as_str()));
            assert_ne!(legacy.id, adopted.id);
            all.rename(kind.name, "default", "Mine").unwrap();
            assert_eq!(all.get(kind.name, "default").unwrap().name, "Mine");
            assert_eq!(all.by_id(&original.id).unwrap().name, "Mine");
            assert!(all.unlisted().iter().all(|a| a.agent != kind.name));
            assert!(all.remove(kind.name, "Mine"));
            assert!(all.for_agent(kind.name).is_empty());
            assert!(all.by_id(&original.id).is_none());
            assert!(all.unlisted().iter().all(|a| a.agent != kind.name), "forgetting is permanent");
        }
    }

    #[test]
    fn public_identity_proposes_a_name_but_does_not_prove_two_sign_ins_are_the_same() {
        let mut all = Accounts::empty();
        let existing = all.get("claude", "default").unwrap();
        let email = "me+personal@example.com";
        all.adopt_checked(&existing, Some(email)).unwrap();
        assert_eq!(all.get("claude", "default").unwrap().name, email);
        all.rename("claude", email, "Personal").unwrap();
        assert_eq!(all.proposed_name("claude", None), "");
        assert_eq!(all.proposed_name("codex", Some("bad\nname@example.com")), "");
        let mut other = Accounts::empty();
        other.add(acct("claude", email, "/a-different-home")).unwrap();
        other.adopt_checked(&existing, Some(email)).unwrap();
        assert_eq!(other.all().len(), 1);
        assert_eq!(other.reviews().len(), 1, "matching email alone still needs review");
        let pending = other.reviews().pop().unwrap();
        assert_eq!(pending.public_identity.as_deref(), Some(email));
        other.review(&pending, None).unwrap();
        assert_eq!(other.all().len(), 2);
        assert_eq!(other.get("claude", "default").unwrap().name, "me+personal@example.com2");
        assert!(other.reviews().is_empty());
    }

    #[test]
    fn a_rule_for_a_pending_or_legacy_claude_account_leaves_codex_unrestricted() {
        let mut all = Accounts::empty();
        let codex = all.add(acct("codex", "work", "/codex-work")).unwrap();
        all.add(acct("claude", "work", "/claude-work")).unwrap();
        let existing = all.get("claude", "default").unwrap();
        all.rules.push(Rule {
            workspace: Some("w".into()),
            path: None,
            agent: None,
            accounts: vec!["claude:default".into()],
            account_ids: None,
        });
        let chosen = |all: &Accounts| all.choose("codex", Some("w"), None, &UsageMap::default(), 0);
        assert_eq!(chosen(&all), Some(codex.clone()), "the implicit legacy account resolves");
        all.adopt_checked(&existing, None).unwrap();
        assert_eq!(all.reviews().len(), 1);
        assert_eq!(chosen(&all), Some(codex.clone()), "a pending review still resolves by name");
        all.bind_rules();
        assert_eq!(chosen(&all), Some(codex.clone()), "a pending review still resolves by identity");
        all.remove("claude", "default");
        assert_eq!(chosen(&all), None, "a truly dangling rule still forbids every account");
    }

    #[test]
    fn review_links_old_references_without_moving_a_running_or_saved_panes_home() {
        let mut all = Accounts::empty();
        let original = all.get("claude", "default").unwrap();
        let mut spec = crate::pane::PaneSpec {
            command: vec!["claude".into(), "--resume".into(), "conversation".into()],
            account: Some("claude:default".into()),
            session: Some("conversation".into()),
            ..Default::default()
        };
        all.add(acct("claude", "patron.bzh", "/patron")).unwrap();
        all.add(acct("claude", "gmail", "/gmail")).unwrap();
        all.rules.push(Rule {
            workspace: Some("old".into()),
            path: None,
            agent: Some("claude".into()),
            accounts: vec!["default".into()],
            account_ids: None,
        });
        all.adopt_checked(&original, None).unwrap();
        let pending = all.reviews().pop().unwrap();
        let target = all.get("claude", "patron.bzh").unwrap();
        assert_eq!(label(&all.get("claude", "default").unwrap()), "claude·Existing account");
        assert_eq!(all.all().len(), 2);
        all.bind_pane(&mut spec);
        let command = spec.command.clone();
        let home_env = spec.env.clone();
        all.review(&pending, Some(&target)).unwrap();
        all.bind_pane(&mut spec);
        assert_eq!(spec.command, command);
        assert_eq!(spec.session.as_deref(), Some("conversation"));
        assert_eq!(spec.account_id.as_deref(), Some(target.id.as_str()));
        assert_eq!(spec.account.as_deref(), Some("claude:patron.bzh"));
        assert_eq!(all.launched_on("claude:default", &home_env).unwrap().home, "~/.claude");
        assert_eq!(all.at_launch_home(target.clone(), &spec.env).home, "~/.claude");
        assert_eq!(all.by_home("claude", &paths::expand_tilde("~/.claude")).unwrap().id, target.id);
        assert_eq!(all.by_id(&original.id).unwrap().id, target.id);
        assert_eq!(all.get("claude", "claude:default").unwrap().id, target.id);
        assert_eq!(all.get("claude", "default").unwrap().home, target.home, "new launches use the chosen home");
        assert_eq!(all.choose("claude", Some("old"), None, &UsageMap::default(), 0).unwrap().id, target.id);
        assert!(all.reviews().is_empty());
        assert_eq!(all.all().len(), 2);
        assert!(all.review(&pending, Some(&target)).is_err(), "a second confirmation is stale");
    }

    #[test]
    fn a_virtual_default_identity_never_moves_to_a_replacement_home() {
        let mut all = Accounts::empty();
        let old = acct("claude", "Personal", "~/.claude");
        all.accounts.push(old.clone());
        assert_eq!(all.by_id("default-claude").unwrap().id, old.id);
        assert!(all.remove("claude", "Personal"));
        let replacement = all.add(Account { id: new_uuid(), ..old.clone() }).unwrap();
        assert_ne!(replacement.id, "default-claude");
        assert!(all.by_id("default-claude").is_none());
        assert!(all.get("claude", "@default-claude").is_none());
        assert!(all.by_id(&old.id).is_none());
        assert_eq!(all.get("claude", "default").unwrap().id, replacement.id, "names may select the replacement");
    }

    #[test]
    fn stale_review_choices_leave_the_pending_account_intact() {
        let mut all = Accounts::empty();
        let target = all.add(acct("claude", "work", "/work")).unwrap();
        let original = all.get("claude", "default").unwrap();
        all.adopt_checked(&original, None).unwrap();
        let pending = all.reviews().pop().unwrap();
        all.remove("claude", "work");
        all.add(Account { id: new_uuid(), ..target.clone() }).unwrap();
        let before = all.text().unwrap();
        assert!(all.review(&pending, Some(&target)).is_err());
        assert_eq!(all.text().unwrap(), before);
        let mut moved = target;
        moved.home = "/moved".into();
        assert!(all.review(&pending, Some(&moved)).is_err());
        assert_eq!(all.text().unwrap(), before);
    }

    #[test]
    fn persisted_review_preserves_the_original_file_and_is_not_offered_again() {
        let dir = std::env::temp_dir().join(format!("kraal-review-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("accounts.toml");
        let source = "# My accounts.\n[[account]]\nagent = 'claude'\nname = 'default'\nhome = '~/.claude'\nnote = 'keep me'\nfuture = 7\n\n[[account]]\nagent = 'claude'\nname = 'patron.bzh'\nhome = '/patron'\n";
        std::fs::write(&path, source).unwrap();
        let existing =
            Accounts::load_from(&path).unwrap().unlisted().into_iter().find(|a| a.agent == "claude").unwrap();
        Accounts::update_at(&path, |all| all.adopt_checked(&existing, None)).unwrap();
        let all = Accounts::load_from(&path).unwrap();
        let pending = all.reviews().pop().unwrap();
        let chosen = all.get("claude", "patron.bzh").unwrap();
        Accounts::update_at(&path, |all| all.review(&pending, Some(&chosen))).unwrap();
        let all = Accounts::load_from(&path).unwrap();
        assert!(all.reviews().is_empty());
        assert_eq!(all.get("claude", "default").unwrap().id, chosen.id);
        assert_eq!(all.by_id(&existing.id).unwrap().id, chosen.id);
        assert!(all.unlisted().iter().all(|a| a.agent != "claude"));
        assert_eq!(all.all().len(), 1);
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(saved.contains("# My accounts.") && saved.contains("note = 'keep me'") && saved.contains("future = 7"));
        assert_eq!(std::fs::read_to_string(path.with_extension("toml.kraal-bak")).unwrap(), source);
        Accounts::update_at(&path, |all| all.adopt_checked(&existing, None)).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), saved, "late discovery cannot undo a review");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn invalid_links_and_failed_review_writes_leave_the_file_unchanged() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("kraal-review-failure-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("accounts.toml");
        for (target, agent) in [("a", "claude"), ("b", "codex"), ("../bad", "claude")] {
            let source = format!(
                "[[account]]\nid = 'a'\nname = 'one'\nagent = 'claude'\nhome = '/one'\nlinked_to = '{target}'\n[[account]]\nid = 'b'\nname = 'two'\nagent = '{agent}'\nhome = '/two'\n"
            );
            std::fs::write(&path, &source).unwrap();
            assert!(Accounts::update_at(&path, |_| Ok(())).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        }
        let mut all = Accounts::empty();
        all.path = path.clone();
        all.add(acct("claude", "work", "/work")).unwrap();
        let existing = all.get("claude", "default").unwrap();
        all.adopt_checked(&existing, None).unwrap();
        all.save().unwrap();
        let pending = all.reviews().pop().unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let result = Accounts::update_at(&path, |all| all.review(&pending, None));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        if result.is_err() {
            assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
            assert_eq!(Accounts::load_from(&path).unwrap().reviews().len(), 1);
            Accounts::update_at(&path, |all| all.review(&pending, None)).unwrap();
        }
        assert!(Accounts::load_from(&path).unwrap().reviews().is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn same_canonical_home_is_one_account_and_keeps_legacy_names_ids_and_notes() {
        let dir = std::env::temp_dir().join(format!("kraal-same-home-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("home")).unwrap();
        let alias = dir.join("alias");
        std::os::unix::fs::symlink(dir.join("home"), &alias).unwrap();
        let path = dir.join("accounts.toml");
        let source = format!(
            "# Mine.\n[[account]]\nname = 'work'\nagent = 'claude'\nhome = {:?}\n\n# Keep this note.\n[[account]]\nname = 'other'\nagent = 'claude'\nhome = {:?}\nnote = 'old label'\nfuture = 7\n",
            dir.join("home").display().to_string(),
            alias.display().to_string()
        );
        std::fs::write(&path, &source).unwrap();
        let all = Accounts::load_from(&path).unwrap();
        let first = all.get("claude", "work").unwrap();
        assert_eq!(all.all().len(), 1);
        assert_eq!(all.get("claude", "other").unwrap().id, first.id);
        let old_alias = all.accounts.iter().find(|a| a.name == "other").unwrap().id.clone();
        assert_eq!(all.by_id(&old_alias).unwrap().id, first.id);
        Accounts::update_at(&path, |_| Ok(())).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved.contains("# Keep this note.") && saved.contains("future = 7") && saved.contains("note = 'old label'")
        );
        assert!(saved.contains("[[account.references]]"));
        assert_eq!(saved.matches("[[account]]").count(), 1);
        assert_eq!(std::fs::read_to_string(path.with_extension("toml.kraal-bak")).unwrap(), source);
        let mut all = Accounts::load_from(&path).unwrap();
        let added = all.add(acct("claude", "third", alias.to_str().unwrap())).unwrap();
        assert_eq!(added.id, first.id);
        assert_eq!(all.all().len(), 1);
        assert_eq!(all.get("claude", "third").unwrap().id, first.id);
        all.add(acct("codex", "work", alias.to_str().unwrap())).unwrap();
        assert_eq!(all.all().len(), 2, "different agents remain separate");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn linked_usage_uses_the_latest_observation_and_never_adds_two_homes() {
        let dir = std::env::temp_dir().join(format!("kraal-linked-usage-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("by-id")).unwrap();
        let mut all = Accounts::empty();
        let target = all.add(acct("claude", "patron.bzh", "/patron")).unwrap();
        let original = all.get("claude", "default").unwrap();
        all.adopt_checked(&original, None).unwrap();
        let pending = all.reviews().pop().unwrap();
        all.review(&pending, Some(&target)).unwrap();
        let observation = |percent, at| Usage {
            observed_at: at,
            five_hour: Some(Window { used_percent: percent, resets_at: None }),
            ..Default::default()
        };
        for (account, usage) in [(&original, observation(57.0, 200)), (&target, observation(40.0, 100))] {
            std::fs::write(
                dir.join("by-id").join(usage_file(&account.agent, &account.id)),
                serde_json::to_vec(&usage).unwrap(),
            )
            .unwrap();
        }
        let usage = UsageMap::load_from(&all, &dir);
        assert_eq!(usage.0.len(), 1);
        assert_eq!(usage.peak(&target, 200), Some(57.0));
        std::fs::write(
            dir.join("by-id").join(usage_file(&target.agent, &target.id)),
            serde_json::to_vec(&observation(20.0, 300)).unwrap(),
        )
        .unwrap();
        assert_eq!(UsageMap::load_from(&all, &dir).peak(&target, 300), Some(20.0));
        assert!(dir.join("by-id").join(usage_file(&original.agent, &original.id)).exists());
        std::fs::write(dir.join("claude-default.json"), serde_json::to_vec(&observation(60.0, 400)).unwrap()).unwrap();
        assert_eq!(
            UsageMap::load_from(&all, &dir).peak(&target, 400),
            Some(60.0),
            "a hook from an older kraal still reports usage"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

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
            id: crate::accounts::new_uuid(),
            name: "t".into(),
            agent: "claude".into(),
            home: home.display().to_string(),
            threshold: None,
            note: None,
            sign_in_required: false,
            review: false,
            public_identity: None,
            linked_to: None,
            references: Vec::new(),
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

    pub(super) fn acct(agent: &str, name: &str, home: &str) -> Account {
        let mut a = Account {
            id: String::new(),
            name: name.into(),
            agent: agent.into(),
            home: home.into(),
            threshold: None,
            note: None,
            sign_in_required: false,
            review: false,
            public_identity: None,
            linked_to: None,
            references: Vec::new(),
        };
        a.id = format!("legacy-{:016x}", crate::text::fnv64(&format!("{agent}\0{name}\0{home}")));
        a
    }

    #[test]
    fn unfinished_sign_in_survives_reload_and_verification_clears_only_its_home() {
        let dir = std::env::temp_dir().join(format!("kraal-sign-in-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("accounts.toml");
        let a = acct("claude", "work", dir.to_str().unwrap());
        let alias = Account { id: new_uuid(), name: "alias".into(), ..a.clone() };
        let other = acct("codex", "work", dir.to_str().unwrap());
        let mut all = Accounts::load_from(&path).unwrap();
        all.add(a.clone()).unwrap();
        all.add(alias).unwrap();
        all.add(other).unwrap();
        all.save().unwrap();
        let original = std::fs::read_to_string(&path).unwrap();
        assert!(!original.contains("sign_in_required"));
        Accounts::update_at(&path, |all| {
            all.set_sign_in_required(&a, true);
            Ok(())
        })
        .unwrap();
        let mut loaded = Accounts::load_from(&path).unwrap();
        assert!(loaded.get("claude", "work").unwrap().sign_in_required);
        assert!(loaded.get("claude", "alias").unwrap().sign_in_required);
        assert!(!loaded.get("codex", "work").unwrap().sign_in_required);
        loaded.set_sign_in_required(&a, false);
        loaded.save().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        loaded.remove("claude", "work");
        loaded.remove("claude", "alias");
        loaded.set_sign_in_required(&a, false);
        assert!(loaded.get("claude", "work").is_none(), "a late check must not recreate a removed account");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_same_sign_in_is_recognized_through_a_directory_alias() {
        let dir = std::env::temp_dir().join(format!("kraal-sign-in-alias-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("home")).unwrap();
        std::os::unix::fs::symlink(dir.join("home"), dir.join("alias")).unwrap();
        let a = acct("claude", "work", dir.join("home").to_str().unwrap());
        let b = acct("claude", "renamed", dir.join("alias").to_str().unwrap());
        assert!(a.same_home(&b));
        assert!(!a.same_home(&Account { agent: "codex".into(), ..b }));
        let alias = acct("claude", "alias", dir.join("alias").to_str().unwrap());
        let held = lock_login(&a).unwrap();
        assert!(lock_login(&alias).unwrap_err().to_string().contains("sign-in already running"));
        let waiter = held.clone();
        drop(held);
        assert!(lock_login(&alias).is_err(), "a closed pane's waiter still holds the login");
        drop(waiter);
        // Another test's fork can hold this descriptor until exec closes it (as with the
        // session locks). Releasing our last hold must still make the lock available.
        let start = std::time::Instant::now();
        loop {
            if let Ok(lock) = lock_login(&alias) {
                drop(lock);
                break;
            }
            assert!(start.elapsed() < crate::testing::PATIENCE, "the sign-in lock was never released");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn accounts() -> Accounts {
        let mut a = Accounts::empty();
        a.accounts = vec![acct("claude", "pro", "/tmp/kraal-test-claude-pro"), acct("claude", "perso", "~/.claude")];
        a.rules = vec![Rule {
            account_ids: None,
            workspace: Some("work".into()),
            path: None,
            agent: None,
            accounts: vec!["pro".into()],
        }];
        a
    }

    #[test]
    fn legacy_default_is_resolvable_but_never_listed() {
        let a = Accounts::empty();
        let codex = a.for_agent("codex");
        assert!(codex.is_empty());
        assert_eq!(a.get("codex", "default").unwrap().home, "~/.codex");
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
            a.get("claude", "pro").unwrap().key(),
            Usage { five_hour: Some(Window { used_percent: 95.0, resets_at: Some(now + 600) }), ..Default::default() },
        );
        // Over the threshold: the rule has no other account, so it takes the one that resets first.
        assert_eq!(a.choose("claude", Some("work"), None, &usage, now).unwrap().name, "pro");
        assert_eq!(a.choose("claude", None, None, &usage, now).unwrap().name, "perso");
        let pro = a.get("claude", "pro").unwrap();
        // The "work" rule only lists pro: no alternative there, but elsewhere perso is fine.
        assert!(a.alternative(&pro, Some("work"), None, &usage, now).is_none());
        assert!(a.alternative(&pro, None, None, &usage, now).is_none(), "unknown usage is not zero");
        usage.0.insert(
            a.get("claude", "perso").unwrap().key(),
            Usage { five_hour: Some(Window { used_percent: 0.0, resets_at: Some(now + 600) }), ..Default::default() },
        );
        assert_eq!(a.alternative(&pro, None, None, &usage, now).unwrap().name, "perso");
        // A window that already reset does not count.
        assert_eq!(usage.peak(&pro, now + 601), Some(0.0));
    }

    #[test]
    fn unknown_usage_and_sign_in_required_accounts_are_not_threshold_alternatives() {
        let mut a = accounts();
        let pro = a.get("claude", "pro").unwrap();
        let perso = a.get("claude", "perso").unwrap();
        let mut usage = UsageMap::default();
        assert_eq!(usage.peak(&perso, 0), None);
        usage.0.insert(perso.key(), Usage::default());
        assert_eq!(usage.peak(&perso, 0), None);
        assert!(a.alternative(&pro, None, None, &usage, 0).is_none());
        usage.0.insert(
            a.get("claude", "perso").unwrap().key(),
            Usage { five_hour: Some(Window { used_percent: 0.0, resets_at: Some(100) }), ..Default::default() },
        );
        assert_eq!(a.choose("claude", None, None, &usage, 0).unwrap().name, "perso", "known headroom before unknown");
        assert_eq!(a.alternative(&pro, None, None, &usage, 0).unwrap().name, "perso");
        a.set_sign_in_required(&perso, true);
        assert!(a.alternative(&pro, None, None, &usage, 0).is_none());
        a.set_sign_in_required(&perso, false);
        a.accounts.iter_mut().find(|a| a.name == "perso").unwrap().home = pro.home.clone();
        assert!(a.alternative(&pro, None, None, &usage, 0).is_none(), "an alias is the same account");
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
            account_ids: None,
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
        assert!(carry_conversation(&a, &b, "nope").is_err(), "a missing transcript is not a new conversation");
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
        std::fs::set_permissions(&on_from, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&on_from).is_err() {
            assert!(check_carry(&a, &c, "abc-1").is_err(), "even a shared transcript must be readable");
            std::fs::remove_file(&on_to).unwrap();
            assert!(check_carry(&a, &b, "abc-1").is_err(), "a first copy must be readable before stopping");
        }
        std::fs::set_permissions(&on_from, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::remove_file(&on_from).unwrap();
        std::fs::write(&on_to, "old copy\n").unwrap();
        assert!(check_carry(&a, &b, "abc-1").is_err(), "the destination alone cannot prove the source's history");
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
            account_ids: None,
            workspace: Some("ops".into()),
            path: None,
            agent: Some("codex".into()),
            accounts: vec!["pro".into()],
        });
        assert!(a.remove("claude", "pro"));
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
            account_ids: None,
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
        a.bind_rules();
        let mut rules = a.rules.clone();
        let pro_id = a.get("claude", "pro").unwrap().id;
        a.save().unwrap();
        std::fs::write(usage.join("claude-pro.json"), "pro's").unwrap();
        // A removed account's usage, under the name perso takes.
        std::fs::write(usage.join("claude-me.json"), "stale").unwrap();

        rename_in(&file, &usage, "claude", "pro", "work").unwrap();
        rename_in(&file, &usage, "claude", "perso", "me").unwrap();
        let a = Accounts::load_from(&file).unwrap();
        let names: Vec<String> = a.all().iter().map(label).collect();
        assert_eq!(names, ["claude·work", "claude·me", "codex·pro"]);
        for (rule, names) in rules.iter_mut().zip([vec!["work", "pro"], vec!["work", "me"], vec!["pro"], vec!["me"]]) {
            rule.accounts = names.into_iter().map(String::from).collect();
        }
        assert_eq!(a.rules, rules);
        let none = UsageMap::default();
        assert_eq!(a.choose("claude", Some("work"), None, &none, 0).unwrap().name, "work");
        assert_eq!(a.choose("codex", Some("work"), None, &none, 0).unwrap().name, "pro");
        assert_eq!(
            std::fs::read_to_string(usage.join("by-id").join(format!("claude-{pro_id}.json"))).unwrap(),
            "pro's"
        );
        assert!(usage.join("claude-pro.json").exists(), "legacy cache is kept for rollback");
        assert_eq!(
            std::fs::read_to_string(usage.join("claude-me.json")).unwrap(),
            "stale",
            "unrelated cache is untouched"
        );

        let text = std::fs::read_to_string(&file).unwrap();
        let refused = |agent: &str, old: &str, new: &str, why: &str| {
            let e = format!("{:#}", rename_in(&file, &usage, agent, old, new).unwrap_err());
            assert!(e.contains(why), "{old} to {new}: {e}");
            assert_eq!(std::fs::read_to_string(&file).unwrap(), text, "{old} to {new} changed the file");
        };
        refused("claude", "work", "me", "already has an account named");
        refused("claude", "work", "my work", "invalid account name");
        refused("claude", "work", "default", "is the account in ~/.claude");
        refused("codex", "default", "mine", "check the existing sign-in");
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
        assert_eq!(a.launched_on("claude:work", &pro).unwrap().name, "work");
        assert!(a.launched_on("codex:gone", &[("CODEX_HOME".into(), "/tmp/kraal-test-nowhere".into())]).is_none());
    }

    #[test]
    fn path_rules() {
        let mut a = accounts();
        a.rules.push(Rule {
            account_ids: None,
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

    /// What is worked out of the usage at one moment holds until the reset said to come next,
    /// and no longer: frames that keep it until then show what a frame made afresh would.
    #[test]
    fn usage_holds_until_the_next_reset() {
        let week = 7 * 24 * 3600;
        let claude = Usage {
            five_hour: Some(Window { used_percent: 93.0, resets_at: Some(1_100) }),
            // It does not say when it resets: a week after it was seen.
            weekly: Some(Window { used_percent: 41.0, resets_at: None }),
            observed_at: 1_000,
            ..Default::default()
        };
        let codex = Usage {
            five_hour: Some(Window { used_percent: 58.0, resets_at: Some(1_250) }),
            observed_at: 1_000,
            ..Default::default()
        };
        let all = UsageMap(HashMap::from([("claude:default".into(), claude.clone()), ("codex:default".into(), codex)]));
        let peaks = |now| -> Vec<f64> { ["claude:default", "codex:default"].map(|k| all.0[k].peak(now)).to_vec() };
        let resets = [1_100, 1_250, 1_000 + week];
        let mut from = 1_000;
        for reset in resets {
            assert_eq!(all.next_end(from), Some(reset));
            for now in [from, (from + reset) / 2, reset - 1] {
                assert_eq!((peaks(now), all.next_end(now)), (peaks(from), Some(reset)), "at {now}");
            }
            assert_ne!(peaks(reset), peaks(reset - 1), "at {reset}");
            from = reset;
        }
        assert_eq!((peaks(from), all.next_end(from)), (vec![0.0, 0.0], None));
        assert_eq!((claude.peak(1_099), claude.peak(1_100), claude.next_end(1_100)), (93.0, 41.0, Some(1_000 + week)));
    }

    #[test]
    fn screen_limits() {
        assert_eq!(limit_from_screen("…\nYou've hit your session limit · resets 3:45pm\n"), Some((false, 100.0)));
        assert_eq!(limit_from_screen("You've used 85% of your weekly limit · resets Mon"), Some((true, 85.0)));
        assert_eq!(limit_from_screen("all good"), None);
    }

    #[test]
    fn reused_names_and_homes_do_not_capture_panes_rules_or_late_login_results() {
        let mut all = accounts();
        let old = all.get("claude", "pro").unwrap();
        let mut pane = crate::pane::PaneSpec {
            account: Some(key(&old.agent, &old.name)),
            env: launch(&old, &[], None, None).unwrap().env,
            ..Default::default()
        };
        // A saved pane from 0.2 or 0.3 has no identity, even in its environment.
        pane.env.retain(|(k, _)| k != "KRAAL_ACCOUNT_ID");
        all.bind_pane(&mut pane);
        assert_eq!(pane.account_id.as_deref(), Some(old.id.as_str()));
        all.rename("claude", "pro", "work").unwrap();
        all.bind_pane(&mut pane);
        assert_eq!(pane.account.as_deref(), Some("claude:work"));
        let replacement =
            Account { id: new_uuid(), home: "/replacement".into(), sign_in_required: true, ..old.clone() };
        all.add(replacement.clone()).unwrap();
        assert_eq!(all.launched_on("claude:pro", &pane.env).unwrap().id, old.id);
        all.remove("claude", "work");
        all.bind_pane(&mut pane);
        assert_eq!(pane.account.as_deref(), Some("claude:work"), "the removed account remains readable");
        assert_eq!(pane.account_id.as_deref(), Some(old.id.as_str()));
        assert!(all.by_id(&old.id).is_none());
        assert!(all.launched_on("claude:pro", &pane.env).is_none());
        assert!(all.choose("claude", Some("work"), None, &UsageMap::default(), 0).is_none());
        all.set_sign_in_required(&old, false);
        assert!(all.get("claude", "pro").unwrap().sign_in_required, "a late login belongs to the removed identity");
    }

    #[test]
    fn a_dangling_legacy_pane_never_binds_to_a_later_account() {
        let mut all = Accounts::empty();
        let mut spec = crate::pane::PaneSpec {
            account: Some("claude:gone".into()),
            env: vec![("CLAUDE_CONFIG_DIR".into(), "/gone".into())],
            ..Default::default()
        };
        all.bind_pane(&mut spec);
        let id = spec.account_id.clone();
        all.add(acct("claude", "gone", "/gone")).unwrap();
        all.bind_pane(&mut spec);
        assert_eq!(spec.account_id, id);
        assert!(all.by_id(id.as_deref().unwrap()).is_none());
    }

    #[test]
    fn invalid_account_identities_leave_the_file_untouched() {
        let dir = std::env::temp_dir().join(format!("kraal-account-bad-id-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("accounts.toml");
        for source in [
            "[[account]]\nid = '../bad'\nname = 'a'\nagent = 'claude'\nhome = '/a'\n".to_string(),
            "[[account]]\nid = 'default-claude'\nname = 'a'\nagent = 'claude'\nhome = '/a'\n".to_string(),
            ["a", "b"]
                .map(|name| format!("[[account]]\nid = 'same'\nname = '{name}'\nagent = 'claude'\nhome = '/{name}'\n"))
                .concat(),
        ] {
            std::fs::write(&path, &source).unwrap();
            assert!(
                Accounts::update_at(&path, |all| {
                    all.limits.threshold = 80;
                    Ok(())
                })
                .is_err()
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
            assert!(!path.with_extension("toml.kraal-bak").exists());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn naming_the_default_home_keeps_its_identity_and_bindings() {
        let dir = std::env::temp_dir().join(format!("kraal-adopt-default-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("accounts.toml");
        std::fs::write(&file, "[[rule]]\nworkspace = 'w'\naccounts = ['default']\nagent = 'claude'\n").unwrap();
        let mut all = Accounts::load_from(&file).unwrap();
        let old = all.get("claude", "default").unwrap();
        let mut pane = crate::pane::PaneSpec {
            account: Some(key(&old.agent, &old.name)),
            env: launch(&old, &[], None, None).unwrap().env,
            ..Default::default()
        };
        all.bind_pane(&mut pane);
        let definition = dir.join("w.kdl");
        std::fs::write(&definition, "layout { pane agent=\"claude:default\" }\n").unwrap();
        crate::workspace::bind_accounts(&definition, &all).unwrap();
        std::fs::write(
            dir.join("claude-default.json"),
            serde_json::to_vec(&Usage {
                five_hour: Some(Window { used_percent: 42.0, resets_at: Some(i64::MAX) }),
                ..Default::default()
            })
            .unwrap(),
        )
        .unwrap();
        migrate_usage(&dir, &old).unwrap();
        let adopted = all.add(Account { id: new_uuid(), name: "personal".into(), ..old.clone() }).unwrap();
        assert_eq!(adopted.id, old.id, "callers get the adopted identity too");
        assert_eq!(all.for_agent("claude"), std::slice::from_ref(&adopted));
        let legacy = all.accounts.iter().find(|a| a.name == "default").unwrap();
        assert_eq!(legacy.linked_to.as_deref(), Some(adopted.id.as_str()));
        assert!(legacy.same_home(&adopted));
        all.bind_pane(&mut pane);
        assert_eq!(pane.account.as_deref(), Some("claude:personal"));
        assert_eq!(pane.account_id.as_deref(), Some(old.id.as_str()));
        assert_eq!(all.launched_on("claude:default", &pane.env), Some(adopted.clone()));
        let def = crate::workspace::load_file(&definition).unwrap();
        assert_eq!(all.by_id(def.panes()[0].account_id.as_deref().unwrap()), Some(adopted.clone()));
        let usage: Usage =
            serde_json::from_slice(&std::fs::read(dir.join("by-id").join(usage_file("claude", &adopted.id))).unwrap())
                .unwrap();
        let usage = UsageMap(HashMap::from([(adopted.key(), usage)]));
        assert_eq!(usage.peak(&adopted, 0), Some(42.0));
        assert_eq!(all.choose("claude", Some("w"), None, &usage, 0), Some(adopted.clone()));
        all.bind_rules();
        all.save().unwrap();
        assert_eq!(Accounts::load_from(&file).unwrap().get("claude", "personal"), Some(adopted));
        std::fs::remove_dir_all(dir).unwrap();
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
id = \"legacy-67bae7cfc475c28e\"

[[account]]
name = \"pro\"
agent = \"codex\"
home = \"/tmp/kraal-test-codex-pro\"
id = \"legacy-e51c15a4282e68d8\"

# Work first, then mine.
[[rule]]
workspace = \"work\"
accounts = [\"pro\", \"default\"]
account_ids = [\"claude:@legacy-67bae7cfc475c28e\", \"codex:@legacy-e51c15a4282e68d8\", \"claude:@default-claude\", \"codex:@default-codex\"]
until = 2027-01-01    # read by a newer kraal
";

    /// What a file that held `BY_HAND` holds once `change` was made to the accounts.
    fn changed(name: &str, change: impl FnOnce(&mut Accounts) -> Result<()>) -> String {
        changed_text(name, BY_HAND, change)
    }

    pub(super) fn changed_text(name: &str, text: &str, change: impl FnOnce(&mut Accounts) -> Result<()>) -> String {
        let dir = std::env::temp_dir().join(format!("kraal-accounts-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("accounts.toml");
        std::fs::write(&path, text).unwrap();
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
        let text = changed("add", |all| all.add(acct("claude", "perso", "/tmp/kraal-test-claude-perso")).map(|_| ()));
        let added = format!(
            "[[account]]\nid = {:?}\nname = \"perso\"\nagent = \"claude\"\nhome = \"/tmp/kraal-test-claude-perso\"\n",
            acct("claude", "perso", "/tmp/kraal-test-claude-perso").id
        );
        assert_eq!(text, BY_HAND.replace("\n# Work first", &format!("\n{added}\n# Work first")));
        // `kraal accounts rm`: the account goes with the comment about it, the header stays.
        let text = changed("rm", |all| {
            all.remove("claude", "pro");
            Ok(())
        });
        assert_eq!(
            text,
            BY_HAND
                .replace(&BY_HAND[BY_HAND.find("# Paid").unwrap()..BY_HAND.find("[[account]]\nname").unwrap()], "")
                .replace(
                    "accounts = [\"pro\", \"default\"]",
                    "accounts = [\"claude:@legacy-67bae7cfc475c28e\",\"pro\", \"default\"]"
                )
        );
        // Renaming changes only the label; the rule keeps its identity.
        let text = changed("rename", |all| all.rename("claude", "pro", "work"));
        assert_eq!(
            text,
            BY_HAND
                .replace(
                    "name = \"pro\"\nhome = \"/tmp/kraal-test-claude-pro\"",
                    "name = \"work\"\nhome = \"/tmp/kraal-test-claude-pro\""
                )
                .replace("accounts = [\"pro\", \"default\"]", "accounts = [\"work\",\"pro\", \"default\"]")
        );
    }

    #[test]
    fn claude_settings_and_existing_rules_keep_their_spelling_and_order() {
        for (i, claude) in [
            "[claude]\nstatusline = 'my-line' # run mine too\n",
            "[claude]\nstatusline = '' # still a command value\n",
            "[claude]\nfuture = 'kept'\n",
            "[claude]\n[claude.future]\ncolor = 'blue'\n",
            "claude = { future = 'kept' }\n",
            "claude = {}\n",
            "[claude]\nrule = [] # a key this kraal does not read\n",
            "[claude]\nstatusline = '''\nrule = []\n[claude]\n'''\n",
        ]
        .iter()
        .enumerate()
        {
            let old = format!("{claude}{BY_HAND}");
            let text = changed_text(&format!("claude-{i}"), &old, |all| {
                all.limits.threshold = 70;
                Ok(())
            });
            assert_eq!(text, old.replace("threshold = 85", "threshold = 70"));
        }
        let rule = BY_HAND.lines().find(|line| line.starts_with("accounts = ")).unwrap();
        let ids = BY_HAND.lines().find(|line| line.starts_with("account_ids = ")).unwrap();
        let old = BY_HAND.replace(rule, "accounts = []").replace(ids, "account_ids = []");
        assert_eq!(changed_text("rule-no-accounts", &old, |all| all.save()), old);
    }

    #[test]
    fn setting_and_clearing_a_statusline_keeps_unknown_claude_settings() {
        for (i, settings) in ["", "future = 'kept'\n"].iter().enumerate() {
            let old = format!("# My status line.\n[claude] # integration\n{settings}\n{BY_HAND}");
            let text = changed_text(&format!("set-statusline-{i}"), &old, |all| {
                all.claude.statusline = Some("my-line".into());
                Ok(())
            });
            let expected = old.replace(
                &format!("{settings}\n{BY_HAND}"),
                &format!("{settings}statusline = \"my-line\"\n\n{BY_HAND}"),
            );
            assert_eq!(text, expected);
            let text = changed_text(&format!("clear-statusline-{i}"), &text, |all| {
                all.claude.statusline = None;
                Ok(())
            });
            assert_eq!(text, if settings.is_empty() { old.replace("[claude]", "") } else { old });
        }
    }

    #[test]
    fn a_new_file_is_written_whole_and_one_that_cannot_be_read_is_left_alone() {
        let dir = std::env::temp_dir().join(format!("kraal-accounts-new-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("accounts.toml");
        let add = |all: &mut Accounts| all.add(acct("codex", "work", "/tmp/kraal-test-codex-work"));
        Accounts::update_at(&path, add).unwrap();
        let whole = "[limits]\nthreshold = 90\nmode = \"notify\"\n\n\
                     [[account]]\nname = \"work\"\nagent = \"codex\"\nhome = \"/tmp/kraal-test-codex-work\"\n";
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            format!("{HEADER}{whole}").replace(
                "[[account]]\n",
                &format!("[[account]]\nid = {:?}\n", acct("codex", "work", "/tmp/kraal-test-codex-work").id)
            )
        );
        // The file `kraal accounts edit` makes when there is none takes its first account
        // under its header.
        std::fs::remove_file(&path).unwrap();
        Accounts::load_from(&path).unwrap().save().unwrap();
        let empty = std::fs::read_to_string(&path).unwrap();
        assert!(empty.starts_with(HEADER) && !empty.contains("rule = []") && !empty.contains("\n[claude]"), "{empty}");
        Accounts::update_at(&path, add).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(HEADER) && text.contains("name = \"work\"\n"), "{text}");
        assert_eq!(Accounts::load_from(&path).unwrap().accounts, [acct("codex", "work", "/tmp/kraal-test-codex-work")]);

        for broken in [
            "# mine\nrule = []\n[claude]\n[[account]\nname = \"work\"\n",
            "# mine\nrule = []\n[claude]\nstatusline = 42\n",
        ] {
            std::fs::write(&path, broken).unwrap();
            assert!(Accounts::update_at(&path, add).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
        }
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
            account_ids: None,
            workspace: Some("shop".into()),
            path: Some("~/src/shop".into()),
            agent: Some("claude".into()),
            accounts: vec!["pro".into()],
        }];
        full.bind_rules();
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
    #[test]
    fn a_process_account_uses_its_own_home_and_refuses_credential_overrides() {
        let dir = std::env::temp_dir().join(format!("kraal-process-account-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        let mut all = Accounts::empty();
        let mut a = Accounts::standard("claude").unwrap();
        a.name = "personal".into();
        a.id = new_uuid();
        a.home = dir.join(".claude").display().to_string();
        all.add(a.clone()).unwrap();
        let kind = agent_kind("claude").unwrap();
        let mut env = HashMap::from([("HOME".into(), dir.display().to_string())]);
        assert_eq!(all.process_account(kind, &env).unwrap().id, a.id);
        env.insert("HOME".into(), "/unrelated-home".into());
        assert!(all.process_account(kind, &env).is_none());
        env.insert("CLAUDE_CONFIG_DIR".into(), a.home.clone());
        assert_eq!(all.process_account(kind, &env).unwrap().id, a.id);
        for key in kind.clear_env {
            env.insert((*key).into(), "override".into());
            assert!(all.process_account(kind, &env).is_none(), "{key}");
            env.remove(*key);
        }
        env.insert("CLAUDE_CONFIG_DIR".into(), "relative".into());
        assert!(all.process_account(kind, &env).is_none());
        assert!(all.process_account(kind, &HashMap::new()).is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn codex_resume_requires_readable_matching_session_metadata() {
        let dir = std::env::temp_dir().join(format!("kraal-codex-resume-{}", std::process::id()));
        let mut a = Accounts::standard("codex").unwrap();
        a.home = dir.display().to_string();
        let folder = dir.join("sessions/2026/10/13");
        std::fs::create_dir_all(&folder).unwrap();
        let file = folder.join("rollout-today-original.jsonl");
        assert!(check_resume(&a, "original").is_err());
        for text in ["", "not json", r#"{"type":"session_meta","payload":{"id":"different"}}"#] {
            std::fs::write(&file, text).unwrap();
            assert!(check_resume(&a, "original").is_err());
        }
        std::fs::write(&file, "{\"type\":\"session_meta\",\"payload\":{\"id\":\"original\"}}\n").unwrap();
        check_resume(&a, "original").unwrap();
        assert!(check_resume(&a, "../original").is_err());
        assert!(check_resume(&a, "").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
