use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

#[cfg(test)]
use super::HOME_CHECKS;
use super::{Account, Accounts, AgentKind, agent_kind, agents};
use crate::identity::new_uuid;
use crate::paths;

enum PaneIdentity<'a> {
    Id(&'a str),
    Reference { agent: &'a str, name: &'a str },
}

impl<'a> PaneIdentity<'a> {
    fn new(reference: Option<&'a str>, id: Option<&'a str>, env: &'a [(String, String)]) -> Option<Self> {
        if let Some(id) = id {
            return Some(Self::Id(id));
        }
        let reference = reference?;
        if let Some((_, id)) = env.iter().find(|(k, _)| k == "KRAAL_ACCOUNT_ID") {
            return Some(Self::Id(id));
        }
        let (agent, name) = reference.split_once(':')?;
        Some(Self::Reference { agent, name })
    }
}

impl Account {
    pub fn new(agent: &str, name: &str, home: String, threshold: Option<u8>, sign_in_required: bool) -> Self {
        Self {
            id: new_uuid(),
            name: name.into(),
            agent: agent.into(),
            home,
            threshold,
            note: None,
            sign_in_required,
            login_revision: String::new(),
            review: false,
            public_identity: None,
            linked_to: None,
            references: Vec::new(),
        }
    }

    pub fn key(&self) -> String {
        key(&self.agent, &format!("@{}", self.id))
    }

    pub(super) fn matches(&self, reference: &str) -> bool {
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

impl Accounts {
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

    pub(super) fn standard(agent: &str) -> Option<Account> {
        let kind = agent_kind(agent)?;
        Some(Account {
            id: format!("default-{agent}"),
            name: "default".into(),
            agent: agent.into(),
            home: kind.default_home.into(),
            threshold: None,
            note: None,
            sign_in_required: false,
            login_revision: String::new(),
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

    pub(super) fn known(&self) -> Vec<Account> {
        let mut all = self.accounts.clone();
        for kind in agents() {
            let standard = Self::standard(kind.name).unwrap();
            if !all.iter().any(|a| a.agent == kind.name && (a.same_home(&standard) || a.name == "default")) {
                all.push(standard);
            }
        }
        all
    }

    pub(super) fn resolve(&self, account: &Account) -> Option<Account> {
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

    pub(super) fn join_same_homes(&mut self) -> Result<()> {
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

    pub fn set_sign_in_required(&mut self, account: &Account, required: bool) {
        if self.by_id(&account.id).is_none() {
            return;
        }
        let revision = required.then(new_uuid);
        let mut found = false;
        for a in &mut self.accounts {
            if a.id == account.id || a.same_home(account) && a.linked_to.is_none() {
                a.sign_in_required = required;
                if let Some(revision) = &revision {
                    a.login_revision = revision.clone();
                }
                found = true;
            }
        }
        if required && !found {
            let mut account = account.clone();
            account.sign_in_required = true;
            account.login_revision = revision.unwrap();
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
        let mut matches = self.known().into_iter().filter_map(|a| {
            if a.agent != kind.name || paths::expand_tilde(&a.home).canonicalize().ok().as_ref() != Some(&canonical) {
                return None;
            }
            self.resolve(&a).filter(|a| !a.review && a.name != "default")
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

    /// Stable references never fall back to a reused name or home. Old panes use their
    /// launch environment to distinguish a renamed account from a replacement of its name.
    pub fn pane_account(&self, spec: &crate::pane::PaneSpec) -> Option<Account> {
        self.resolve_pane(spec.account.as_deref(), spec.account_id.as_deref(), &spec.env)
            .map(|a| self.at_launch_home(a, &spec.env))
    }

    fn resolve_pane(&self, reference: Option<&str>, id: Option<&str>, env: &[(String, String)]) -> Option<Account> {
        let (agent, name) = match PaneIdentity::new(reference, id, env)? {
            PaneIdentity::Id(id) => return self.by_id(id),
            PaneIdentity::Reference { agent, name } if name.starts_with('@') => return self.get(agent, name),
            PaneIdentity::Reference { agent, name } => (agent, name),
        };
        let kind = agent_kind(agent)?;
        let home = env
            .iter()
            .find(|(k, _)| k == kind.home_env)
            .map(|(_, home)| PathBuf::from(home))
            .unwrap_or_else(|| paths::expand_tilde(kind.default_home));
        self.get(agent, name).filter(|a| paths::expand_tilde(&a.home) == home).or_else(|| self.by_home(agent, &home))
    }

    pub fn bind_pane(&self, spec: &mut crate::pane::PaneSpec) {
        let Some(reference) = spec.account.as_deref() else { return };
        let account = self.resolve_pane(Some(reference), spec.account_id.as_deref(), &spec.env);
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

pub(super) fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn valid_account_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('@')
        && name.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '@' | '+'))
}

/// Keep `default` available for references written by older clients.
pub(super) fn check_name(kind: &AgentKind, name: &str) -> Result<()> {
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
    Accounts::update(|all| all.rename(agent, old, new))
}

pub fn review_question(account: &Account) -> String {
    format!("? Which account is {}", paths::shorten_home(&paths::expand_tilde(&account.home)))
}

pub fn existing_label(agent: &str) -> String {
    format!("Existing {} account", crate::agent_registry::find(agent).map_or(agent, |a| a.label))
}

/// How an account is shown, e.g. `claude·pro`.
pub fn label(a: &Account) -> String {
    format!("{}·{}", a.agent, if a.review || a.name == "default" { "Existing account" } else { &a.name })
}

/// The readable account label kept for older clients and saved sessions.
pub fn key(agent: &str, name: &str) -> String {
    format!("{agent}:{name}")
}
