use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::identity::{check_name, valid_name};
use super::{Account, Accounts, ClaudeOptions, Limits, Rule, absolute_home, agent_kind, agent_names, missing_account};
use crate::identity::new_uuid;
use crate::paths;

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
    fn expanded_accounts(self) -> Vec<Account> {
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
        let account: Account = toml::from_str(&table.to_string())?;
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

pub(super) const HEADER: &str = "# kraal agent accounts. One [[account]] per subscription: a name, the agent (claude, codex)\n\
# and its config home. [[rule]] picks accounts by workspace or directory, in order; [limits] sets\n\
# the usage threshold (percent) and what happens when an account reaches it: notify, ask or auto.\n\n";

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

    pub(super) fn read(path: &Path) -> Result<(Accounts, Vec<String>)> {
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
    pub(super) fn text(&self) -> Result<String> {
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
        let text = self.compact_document(&text).with_context(|| format!("editing {}", self.path.display()))?;
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
            let out = f(&mut all)?;
            all.bind_rules();
            if joined || all.text()? != before {
                all.save()?;
            }
            Ok(out)
        })?
    }

    /// Revalidate a CLI check before preparing a home or publishing its sign-in state.
    pub fn add_prepared(
        &mut self,
        account: Account,
        checked: Option<(&Account, bool)>,
    ) -> Result<(Account, Vec<String>)> {
        let mut account = self.add(account)?;
        if let Some((expected, _)) = checked {
            anyhow::ensure!(
                account.id == expected.id && account.same_home(expected),
                "the account changed while checking sign-in; run the add command again"
            );
        }
        let verified = checked.is_some_and(|(_, verified)| verified);
        if verified {
            self.set_sign_in_required(&account, false);
            account.sign_in_required = false;
        }
        let notes = if verified { Vec::new() } else { super::prepare_home(&account)? };
        Ok((account, notes))
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
}
