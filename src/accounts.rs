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

#[cfg(test)]
thread_local! {
    pub(crate) static HOME_CHECKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
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
        #[cfg(test)]
        HOME_CHECKS.with(|n| n.set(n.get() + 1));
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
                migration::before_account_change(&previous)?;
                all.save()?;
            }
            Ok(out)
        })?
    }

    pub(crate) fn entries(&self) -> &[Account] {
        &self.accounts
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
        agents().flat_map(|k| self.for_agent(k.name)).collect()
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
        agents()
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
        for kind in agents() {
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
        let kind = agent_kind(&account.agent).with_context(|| match crate::agent_registry::find(&account.agent) {
            Some(agent) => agent.account_message(),
            None => format!("unknown agent {:?} (known: {})", account.agent, agent_names()),
        })?;
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
                let dangling = !references.iter().any(|n| agents().any(|kind| self.get(kind.name, n).is_some()));
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
    if let Some(agent) = crate::agent_registry::find(agent).filter(|a| a.accounts.is_none()) {
        return agent.account_message();
    }
    if reference.starts_with('@') || reference.contains(":@") {
        "the account was removed; choose an account again".into()
    } else {
        format!("{agent} has no account named {reference:?}")
    }
}

pub fn agent_names() -> String {
    agents().map(|a| a.name).collect::<Vec<_>>().join(", ")
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
        migration::migrate_usage(usage, &a)
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

impl Launch {
    pub fn command(&self) -> std::process::Command {
        let mut cmd = std::process::Command::new(&self.argv[0]);
        cmd.args(&self.argv[1..]);
        for key in &self.env_remove {
            cmd.env_remove(key);
        }
        cmd.envs(self.env.iter().cloned());
        cmd
    }
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

/// Keep launch options and prompts, but never select a conversation from the copied pane.
pub fn duplicate_args(kind: &AgentKind, args: &[String]) -> Vec<String> {
    let mut kept = Vec::new();
    let mut prompt = Vec::new();
    let mut args = args.iter().peekable();
    let mut conversation = false;
    let mut selected = false;
    let mut positional = false;
    while let Some(arg) = args.next() {
        let (flag, attached) = arg.split_once('=').map_or((arg.as_str(), false), |(flag, _)| (flag, true));
        if arg == "--" {
            kept.push(arg.clone());
            if conversation && !selected {
                args.next();
            }
            kept.append(&mut prompt);
            kept.extend(args.cloned());
            break;
        }
        if kind.name == "claude" {
            match flag {
                "--session-id" | "--resume" | "-r" | "--from-pr" | "--teleport" => {
                    if !attached {
                        args.next_if(|v| !v.starts_with('-'));
                    }
                    continue;
                }
                "--continue" | "-c" | "--fork-session" => continue,
                _ => {}
            }
        } else if kind.name == "codex" {
            if !positional && matches!(flag, "exec" | "e") {
                kept.push(arg.clone());
                continue;
            }
            if !positional && CODEX_CONVERSATIONS.contains(&flag) {
                conversation = true;
                positional = true;
                continue;
            }
            if conversation && CODEX_CHOOSERS.contains(&flag) {
                selected |= flag == "--last";
                continue;
            }
            if conversation && !arg.starts_with('-') {
                if selected {
                    prompt.push(arg.clone());
                } else {
                    selected = true;
                }
                continue;
            }
        }
        kept.push(arg.clone());
        let (single, several) =
            if kind.name == "claude" { (CLAUDE_VALUE, CLAUDE_VALUES) } else { (CODEX_VALUE, CODEX_VALUES) };
        if !attached && single.contains(&flag) {
            kept.extend(args.next().cloned());
        } else if several.contains(&flag) {
            while let Some(value) = args.next_if(|v| !v.starts_with('-')) {
                kept.push(value.clone());
            }
        } else if !arg.starts_with('-') {
            positional = true;
        }
    }
    // Removing resume/fork exposes the prompt to subcommand parsing. Keep options before
    // the separator so they still apply, even when they originally followed the prompt.
    if prompt.first().is_some_and(|p| CODEX_COMMANDS.contains(&p.as_str()) || CODEX_CONVERSATIONS.contains(&p.as_str()))
    {
        kept.push("--".into());
    }
    kept.extend(prompt);
    kept
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
    "-o",
    "--output-last-message",
    "--output-schema",
    "--color",
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

    /// Each valid window with the time it counts until: its reset or, when it does not say when it
    /// resets, the end of its period after it was seen.
    fn valid_windows(&self, now: i64) -> [Option<(&Window, i64)>; 2] {
        [(&self.five_hour, 5 * 3600), (&self.weekly, 7 * 24 * 3600)].map(|(window, period)| {
            window
                .as_ref()
                .map(|w| (w, w.resets_at.unwrap_or(self.observed_at.saturating_add(period))))
                .filter(|(_, end)| *end > now)
        })
    }

    /// Five-hour and weekly percentages: absent when unreported, zero when expired.
    pub fn percentages(&self, now: i64) -> impl Iterator<Item = Option<f64>> {
        [&self.five_hour, &self.weekly]
            .into_iter()
            .zip(self.valid_windows(now))
            .map(|(reported, valid)| reported.as_ref().map(|_| valid.map_or(0.0, |(w, _)| w.used_percent)))
    }

    /// Highest usage still counting, or `None` when no windows are valid.
    pub fn valid_peak(&self, now: i64) -> Option<f64> {
        self.valid_windows(now)
            .into_iter()
            .flatten()
            .fold(None, |peak, (w, _)| Some(peak.unwrap_or(0.0).max(w.used_percent)))
    }

    /// Highest usage among windows that have not reset yet.
    pub fn peak(&self, now: i64) -> f64 {
        self.valid_peak(now).unwrap_or(0.0)
    }

    /// When [`Usage::peak`] changes next with nothing new seen: the earliest reset of a window
    /// that counts at `now`.
    pub fn next_end(&self, now: i64) -> Option<i64> {
        self.valid_windows(now).into_iter().flatten().map(|(_, end)| end).min()
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
            let _ = migration::migrate_usage(dir, &a);
            if let Some(u) = load_usage(dir, &a.agent, &a.id) {
                usage.observe(&target, u);
            }
            if let Some(name) = migration::legacy_usage_name(&a)
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
mod tests;
