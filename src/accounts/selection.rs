use std::path::Path;

use super::{Account, Accounts, UsageMap, agents};
use crate::paths;

impl Accounts {
    pub(super) fn bind_rules(&mut self) {
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

    pub fn threshold(&self, a: &Account) -> u8 {
        a.threshold.unwrap_or(self.limits.threshold)
    }

    /// Accounts to try, in order, for an agent started in `workspace` and `cwd`.
    pub(super) fn candidates(&self, agent: &str, workspace: Option<&str>, cwd: Option<&Path>) -> Vec<Account> {
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
        let mut candidates = self.candidates(agent, workspace, cwd);
        // An account awaiting sign-in would open on its login screen: only when there is no other.
        if candidates.iter().any(|a| !a.sign_in_required) {
            candidates.retain(|a| !a.sign_in_required);
        }
        if let Some(a) = candidates.iter().find(|a| usage.peak(a, now).is_some_and(|p| p < self.threshold(a) as f64)) {
            return Some(a.clone());
        }
        if let Some(a) = candidates.iter().find(|a| usage.peak(a, now).is_none()) {
            return Some(a.clone());
        }
        candidates.into_iter().min_by_key(|a| usage.get(a).and_then(|u| u.next_reset(now)).unwrap_or(i64::MAX))
    }

    /// Another account of the same agent to continue on, under its threshold, other than those
    /// `skip` leaves out. When a rule covers this context, only its accounts are considered: work
    /// never moves to an account the rule does not list.
    pub fn alternative(
        &self,
        current: &Account,
        workspace: Option<&str>,
        cwd: Option<&Path>,
        usage: &UsageMap,
        now: i64,
        skip: impl Fn(&Account) -> bool,
    ) -> Option<Account> {
        self.candidates(&current.agent, workspace, cwd).into_iter().find(|a| {
            a.id != current.id
                && !a.same_home(current)
                && !skip(a)
                && !a.sign_in_required
                && !a.review
                && usage.peak(a, now).is_some_and(|p| p < self.threshold(a) as f64)
        })
    }
}
