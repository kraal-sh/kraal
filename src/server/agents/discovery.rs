use anyhow::{Context, Result};

use super::super::{ClientId, Server};
use super::{Discovery, ExistingCheck, ObservedAgent};
use crate::account_status::{self, Status};
use crate::accounts::{self, Account, Accounts};
use crate::layout::PaneId;
use crate::pane::LoginState;
use crate::theme::NoticeKind;

pub(super) const PROCESS_UNVERIFIED: &str =
    "Could not verify the running agent. The current pane is unchanged. Open Change account to try again.";

/// Only a direct executable or a runtime's script has arguments we can safely reuse.
pub(super) fn observed_command(argv: &[String], agent: &str) -> Option<Vec<String>> {
    fn base(s: &str) -> &str {
        s.rsplit('/').next().unwrap_or(s)
    }
    let first = argv.first()?;
    let command = if base(first) == agent {
        argv
    } else if matches!(base(first), "node" | "bun" | "python" | "python3" | "sh" | "bash" | "zsh" | "dash")
        && argv.get(1).is_some_and(|s| base(s) == agent)
    {
        &argv[1..]
    } else {
        return None;
    };
    let kind = accounts::agent_kind(agent)?;
    if command.get(1).is_some_and(|s| matches!(s.as_str(), "--resume" | "--session-id" | "resume"))
        && command.get(2).is_none_or(|s| {
            s.is_empty() || s.starts_with('-') || !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
    {
        return None;
    }
    let args = accounts::launch_args(kind, command);
    if args.iter().any(|s| {
        [
            "-c",
            "--config",
            "--profile",
            "--settings",
            "--setting-sources",
            "--resume",
            "--session-id",
            "--continue",
            "--fork-session",
            "--oss",
            "--local-provider",
            "-r",
            "-p",
        ]
        .iter()
        .any(|flag| s == flag || s.starts_with(&format!("{flag}=")))
    }) {
        return None;
    }
    let (_, exact) = accounts::resumable_args(kind, &accounts::launch_args(kind, command))?;
    exact.then(|| command.to_vec())
}

impl Discovery {
    pub(in crate::server) fn observed(&self, pane: &PaneId) -> Option<&ObservedAgent> {
        self.observed.get(pane)
    }

    pub(in crate::server) fn forget_pane(&mut self, pane: PaneId) {
        self.observed.remove(&pane);
    }

    pub(super) fn observe(&mut self, pane: PaneId, agent: ObservedAgent) {
        self.observed.insert(pane, agent);
    }

    fn refresh_observed(&mut self, observed: std::collections::HashMap<PaneId, ObservedAgent>) -> bool {
        if self.observed == observed {
            return false;
        }
        self.observed = observed;
        true
    }

    pub(in crate::server) fn checks(&self) -> &std::collections::HashMap<String, ExistingCheck> {
        &self.checks
    }

    fn begin(&mut self, agent: &str) -> u64 {
        self.checks.insert(agent.into(), ExistingCheck::Checking);
        self.generations.get(agent).copied().unwrap_or_default()
    }

    fn cached(&self, account: &Account) -> Option<(Account, account_status::Checked)> {
        self.identities.get(&account.id).filter(|(a, _)| a == account).cloned()
    }

    pub(in crate::server) fn status(&self, account: &Account) -> Option<Status> {
        self.identities
            .get(&account.id)
            .filter(|(a, _)| {
                a.agent == account.agent
                    && a.home == account.home
                    && a.login_revision == account.login_revision
                    && a.sign_in_required == account.sign_in_required
            })
            .map(|(_, checked)| checked.status)
    }

    fn is_current(&self, agent: &str, generation: u64) -> bool {
        self.generations.get(agent).copied().unwrap_or_default() == generation
    }

    fn complete(
        &mut self,
        agent: &str,
        generation: u64,
        status: Status,
        cached: Vec<(Account, account_status::Checked)>,
    ) -> bool {
        if !self.is_current(agent, generation) {
            return false;
        }
        self.identities.extend(cached.into_iter().map(|(a, c)| (a.id.clone(), (a, c))));
        self.checks.insert(agent.into(), ExistingCheck::Checked(status));
        true
    }

    fn save_failed(&mut self, agent: &str) {
        self.checks.insert(agent.into(), ExistingCheck::SaveFailed);
    }

    fn refresh(&mut self) {
        self.checks.retain(|_, status| {
            matches!(
                status,
                ExistingCheck::Checking
                    | ExistingCheck::Checked(Status::SignedIn | Status::CouldNotCheck)
                    | ExistingCheck::SaveFailed
            )
        });
    }

    fn invalidate(&mut self, agent: &str) {
        *self.generations.entry(agent.into()).or_default() += 1;
        self.identities.retain(|_, (a, _)| a.agent != agent);
        self.checks.remove(agent);
    }

    #[cfg(test)]
    pub(in crate::server) fn set_status(&mut self, account: &Account, status: Status) {
        self.identities
            .insert(account.id.clone(), (account.clone(), account_status::Checked { status, identity: None }));
    }

    #[cfg(test)]
    pub(in crate::server) fn set_check(&mut self, agent: &str, state: ExistingCheck) {
        self.checks.insert(agent.into(), state);
    }
}

impl Server {
    pub(in crate::server) fn update_accounts<T>(&mut self, f: impl FnOnce(&mut Accounts) -> Result<T>) -> Result<T> {
        Accounts::update(f)
    }

    pub(in crate::server) fn reload_accounts(&mut self) {
        match Accounts::load() {
            Ok(a) => {
                self.follow_renamed_accounts();
                self.accounts = a;
                self.account_display.get_mut().take();
                self.accounts_error = None;
                self.check_existing_accounts();
                self.follow_renamed_accounts();
            }
            Err(e) => {
                let e = format!("{e:#}");
                if self.accounts_error.as_ref() != Some(&e) {
                    self.warnings.push(e.clone());
                    self.accounts_error = Some(e);
                }
            }
        }
    }

    pub(super) fn check_existing_accounts(&mut self) {
        for a in self.accounts.unlisted().into_iter().chain(self.accounts.reviews()) {
            if self.account_discovery.checks().contains_key(&a.agent)
                || self.panes.values().any(|p| {
                    p.spec.login.as_ref().is_some_and(|l| {
                        l.account.agent == a.agent && matches!(l.state, LoginState::Running | LoginState::Checking)
                    })
                })
            {
                continue;
            }
            let generation = self.account_discovery.begin(&a.agent);
            let checked = a.clone();
            let named = self.accounts.for_agent(&a.agent);
            let mut cached: Vec<_> = named.iter().filter_map(|a| self.account_discovery.cached(a)).collect();
            let missing: Vec<_> = named.into_iter().filter(|a| !cached.iter().any(|(b, _)| a == b)).collect();
            let agent = a.agent.clone();
            let started = self.background(
                move || {
                    let result = account_status::inspect(&checked);
                    // Named homes cannot supply a match without a verified existing identity.
                    if result.status == Status::SignedIn && result.identity.is_some() {
                        cached.extend(account_status::inspect_accounts(&missing));
                    }
                    (result, cached)
                },
                move |server, (result, cached)| {
                    if !server.account_discovery.is_current(&a.agent, generation) {
                        return;
                    }
                    if matches!(result.status, Status::SignedIn | Status::CouldNotCheck) {
                        server.save_existing_account(a, result, cached, generation);
                    } else {
                        server.account_discovery.complete(&a.agent, generation, result.status, Vec::new());
                        server.reload_accounts();
                        server.reload_usage();
                        server.refresh_agent_pickers();
                        server.mark_dirty();
                    }
                },
            );
            if !started {
                self.account_discovery.complete(&agent, generation, Status::CouldNotCheck, Vec::new());
            }
        }
    }

    fn save_existing_account(
        &mut self,
        a: Account,
        result: account_status::Checked,
        cached: Vec<(Account, account_status::Checked)>,
        generation: u64,
    ) {
        let agent = a.agent.clone();
        let status = result.status;
        let started = self.background(
            move || {
                let saved = account_status::save_discovery(&[(a.clone(), result)], &cached);
                (a, cached, saved)
            },
            move |server, (a, cached, saved)| {
                if !server.account_discovery.complete(&a.agent, generation, status, cached) {
                    server.reload_accounts();
                    return;
                }
                match saved {
                    Ok(notices) => {
                        for notice in notices {
                            server.queue_account_notice(notice);
                        }
                        server.reload_accounts();
                        server.reload_usage();
                    }
                    Err(e) => {
                        server.account_discovery.save_failed(&a.agent);
                        server.warnings.push(format!("Could not adopt the existing account: {e:#}. Open Check again."));
                    }
                }
                server.refresh_agent_pickers();
                server.mark_dirty();
            },
        );
        if !started {
            self.account_discovery.save_failed(&agent);
        }
    }

    pub(in crate::server) fn refresh_existing_accounts(&mut self) {
        self.account_discovery.refresh();
        self.check_existing_accounts();
    }

    pub(super) fn invalidate_account_identities(&mut self, agent: &str) {
        self.account_discovery.invalidate(agent);
    }

    pub(in crate::server) fn retry_existing_account(&mut self, agent: &str) {
        if self.account_discovery.checks().get(agent).is_some_and(|state| *state != ExistingCheck::Checking) {
            self.invalidate_account_identities(agent);
            self.check_existing_accounts();
        }
    }

    pub(in crate::server) fn confirm_existing_account(
        &mut self,
        client: ClientId,
        existing: &Account,
        target: Option<&Account>,
    ) -> Result<()> {
        self.update_accounts(|all| all.review(existing, target))?;
        self.reload_accounts();
        self.reload_usage();
        self.refresh_agent_pickers();
        self.mark_dirty();
        self.toast(client, NoticeKind::Notice, "Account updated");
        Ok(())
    }

    /// Panes started on an account renamed since go on under its new name.
    pub(super) fn follow_renamed_accounts(&mut self) {
        let mut renamed = false;
        for pane in self.panes.values_mut() {
            let before = (pane.spec.account.clone(), pane.spec.account_id.clone());
            self.accounts.bind_pane(&mut pane.spec);
            renamed |= before != (pane.spec.account.clone(), pane.spec.account_id.clone());
        }
        for ws in self.unrestored.iter_mut().chain(self.workspaces.iter_mut().filter_map(|w| w.kept.as_mut())) {
            renamed |= ws.bind_accounts(&self.accounts);
        }
        if renamed {
            self.mark_dirty();
            self.mark_persist();
        }
    }

    /// Rename account `old` of `agent` to `new` ([`accounts::rename`]).
    pub(in crate::server) fn rename_account(&mut self, agent: &str, old: &str, new: &str) -> Result<()> {
        self.update_accounts(|all| all.rename(agent, old, new))?;
        self.reload_accounts();
        self.reload_usage();
        Ok(())
    }

    /// The account of a managed pane, or of an unambiguously identified foreground agent.
    pub fn pane_account(&self, pane: PaneId) -> Option<Account> {
        let spec = &self.panes.get(&pane)?.spec;
        if spec.account.is_some() || spec.account_id.is_some() {
            self.spec_account(spec)
        } else {
            self.account_discovery.observed(&pane).map(|a| a.account.clone())
        }
    }

    pub(super) fn observed_agent(&self, pane: PaneId) -> Result<Option<ObservedAgent>> {
        let Some(p) = self.panes.get(&pane) else { return Ok(None) };
        if p.exit_code.is_some() || p.ssh.is_some() || p.spec.login.is_some() {
            return Ok(None);
        }
        let Some(registered) = p.agent.kind.as_deref().and_then(crate::agent_registry::find) else { return Ok(None) };
        if !registered.conversation_id {
            return Ok(None);
        }
        let Some(kind) = registered.accounts.as_ref() else { return Ok(None) };
        let Some(leader) = p.pid() else { return Ok(None) };
        let pid = p.foreground_pid().context(PROCESS_UNVERIFIED)?;
        let started = crate::process::start_time(pid).context(PROCESS_UNVERIFIED)?;
        let mut names = vec!["HOME", "PATH", kind.home_env];
        names.extend(kind.clear_env);
        let (argv, env) = crate::process::command_env(pid, &names).context(PROCESS_UNVERIFIED)?;
        let Some(mut command) = observed_command(&argv, kind.name) else { return Ok(None) };
        // A shell's other jobs and opaque wrappers must not be stopped with its foreground job.
        if !crate::process::sole_foreground_job(leader, pid).context(PROCESS_UNVERIFIED)?
            || !p.spec.command.is_empty() && observed_command(&p.spec.command, kind.name).is_none()
            || pid != leader && crate::process::ppid(pid).context(PROCESS_UNVERIFIED)? != leader
        {
            return Ok(None);
        }
        let Some(account) = self.accounts.process_account(kind, &env) else { return Ok(None) };
        let Some(session) = p
            .agent
            .session_process
            .filter(|process| *process == (pid, started))
            .and(p.agent.session_id.clone())
            .or_else(|| {
                let selector = if kind.name == "claude" { &["--resume", "--session-id"][..] } else { &["resume"][..] };
                selector.contains(&command.get(1)?.as_str()).then(|| command.get(2).cloned()).flatten()
            })
        else {
            return Ok(None);
        };
        let cwd = crate::process::cwd(pid).context(PROCESS_UNVERIFIED)?;
        let Some(path) = env.get("PATH") else { return Ok(None) };
        let program = std::path::Path::new(&command[0]);
        let executable = |path: &std::path::Path| {
            use std::os::unix::fs::PermissionsExt;
            path.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        };
        let program = if program.components().count() > 1 {
            cwd.join(program)
        } else {
            let Some(program) =
                std::env::split_paths(path).map(|dir| cwd.join(dir).join(program)).find(|p| executable(p))
            else {
                return Ok(None);
            };
            program
        };
        if !executable(&program) {
            return Ok(None);
        }
        let Some(program) = program.to_str() else { return Ok(None) };
        command[0] = program.into();
        if !crate::process::running(pid as libc::pid_t) {
            anyhow::ensure!(!crate::process::exists(pid as libc::pid_t), PROCESS_UNVERIFIED);
            return Ok(None);
        }
        if crate::process::start_time(pid).context(PROCESS_UNVERIFIED)? != started {
            return Ok(None);
        }
        anyhow::ensure!(p.foreground_pid() == Some(pid), PROCESS_UNVERIFIED);
        Ok(Some(ObservedAgent { pid, started, command, account, session, cwd, path: path.clone() }))
    }

    pub(in crate::server) fn refresh_observed_accounts(&mut self) {
        let observed = self
            .panes
            .keys()
            .filter(|id| {
                let spec = &self.panes[id].spec;
                spec.account.is_none() && spec.account_id.is_none()
            })
            .filter_map(|&id| {
                // Keep the label through an unreadable process table. Switching still requires a fresh check.
                self.observed_agent(id)
                    .unwrap_or_else(|_| self.account_discovery.observed(&id).cloned())
                    .map(|a| (id, a))
            })
            .collect();
        if self.account_discovery.refresh_observed(observed) {
            self.mark_dirty();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_status_survives_metadata_changes_but_not_identity_home_or_login_changes() {
        let mut discovery = Discovery::default();
        let account = Account::new("claude", "gmail", "/unused/claude".into(), None, false);
        discovery.set_status(&account, Status::NotSignedIn);
        for changed in [
            Account { name: "renamed".into(), ..account.clone() },
            Account { note: Some("Work account".into()), ..account.clone() },
            Account { threshold: Some(75), ..account.clone() },
        ] {
            assert_eq!(discovery.status(&changed), Some(Status::NotSignedIn));
        }
        for changed in [
            Account { id: "replacement".into(), ..account.clone() },
            Account { agent: "codex".into(), ..account.clone() },
            Account { home: "/unused/other".into(), ..account.clone() },
            Account { login_revision: "new-login".into(), ..account.clone() },
            Account { sign_in_required: true, ..account },
        ] {
            assert_eq!(discovery.status(&changed), None);
        }
    }

    #[test]
    fn invalidation_rejects_late_completion_and_keeps_other_agents_cached() {
        let mut discovery = Discovery::default();
        let claude = Account::new("claude", "work", "/unused/claude".into(), None, false);
        let codex = Account::new("codex", "work", "/unused/codex".into(), None, false);
        let checked = account_status::Checked { status: Status::SignedIn, identity: None };
        for a in [&claude, &codex] {
            let generation = discovery.begin(&a.agent);
            assert!(discovery.complete(&a.agent, generation, Status::SignedIn, vec![(a.clone(), checked.clone())]));
        }
        let old = discovery.begin("claude");
        discovery.invalidate("claude");
        let current = discovery.begin("claude");
        assert!(!discovery.complete("claude", old, Status::SignedIn, vec![(claude.clone(), checked.clone())]));
        assert!(matches!(discovery.checks()["claude"], ExistingCheck::Checking));
        assert!(discovery.cached(&claude).is_none());
        assert!(discovery.cached(&codex).is_some());
        assert!(discovery.complete("claude", current, Status::SignedIn, vec![(claude.clone(), checked)]));
        assert!(discovery.cached(&claude).is_some());
        assert!(discovery.cached(&Account { login_revision: "new-login".into(), ..claude }).is_none());
    }

    #[test]
    fn refreshing_retries_only_unsigned_or_uninstalled_checks() {
        let mut discovery = Discovery::default();
        for (agent, status, retained) in [
            ("checking", ExistingCheck::Checking, true),
            ("signed-in", ExistingCheck::Checked(Status::SignedIn), true),
            ("uncertain", ExistingCheck::Checked(Status::CouldNotCheck), true),
            ("save-failed", ExistingCheck::SaveFailed, true),
            ("unsigned", ExistingCheck::Checked(Status::NotSignedIn), false),
            ("uninstalled", ExistingCheck::Checked(Status::NotInstalled), false),
        ] {
            discovery.set_check(agent, status);
            discovery.refresh();
            assert_eq!(discovery.checks().contains_key(agent), retained);
        }
    }
}
