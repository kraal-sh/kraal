//! The agent manager in the server: launching agents on accounts, the agent picker, usage
//! thresholds, and continuing a conversation on another account.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::panes::ProgramStop;
use super::ui::{ConfirmAction, Mode, PickAction, PickItem, Picker, PickerKind};
use super::{ClientId, Server};
use crate::account_status::{self, Status};
use crate::accounts::{self, Account, Accounts, Mode as LimitMode, UsageMap};
use crate::agent::AgentState;
use crate::layout::PaneId;
use crate::pane::{Login, LoginState, PaneSpec};
use crate::protocol::Placement;
use crate::theme::NoticeKind;

/// What kraal last did about a pane being over its threshold, so it does not repeat itself.
#[derive(Debug, Clone, PartialEq)]
pub enum LimitAlert {
    Notified,
    Asked,
    Attempted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SwitchStage {
    Checking,
    Waiting,
    Confirm,
    Stopping,
    Starting,
    Restoring,
    Recovering,
    RecoveryStopping,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ExistingCheck {
    Checking,
    Checked(Status),
    SaveFailed,
}

pub(super) struct AccountSwitch {
    token: u32,
    from: Account,
    to: Account,
    session: Option<String>,
    process: Option<crate::pane::ProcessHold>,
    client: Option<ClientId>,
    ask: bool,
    original: PaneSpec,
    undo: bool,
    observed: Option<ObservedAgent>,
    started: Option<Instant>,
    stopping: Option<ProgramStop>,
    pub stage: SwitchStage,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct ObservedAgent {
    pid: u32,
    started: u64,
    command: Vec<String>,
    pub(super) account: Account,
    session: String,
    cwd: PathBuf,
    path: String,
}

const PROCESS_UNVERIFIED: &str =
    "Could not verify the running agent. The current pane is unchanged. Open Change account to try again.";

/// Only a direct executable or a runtime's script has arguments we can safely reuse.
fn observed_command(argv: &[String], agent: &str) -> Option<Vec<String>> {
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

fn switch_timing(hooked: bool, state: AgentState, exited: bool) -> SwitchStage {
    if exited || hooked && matches!(state, AgentState::Idle | AgentState::Done) {
        SwitchStage::Stopping
    } else if hooked && matches!(state, AgentState::Working | AgentState::Blocked) {
        SwitchStage::Waiting
    } else {
        SwitchStage::Confirm
    }
}

fn switch_effect(agent: &str, status: Status, carry: bool) -> &'static str {
    match status {
        Status::NotSignedIn => "Sign-in required",
        Status::SignedIn if agent == "codex" => "New conversation",
        Status::SignedIn if carry => "Continue conversation",
        _ => "Could not verify",
    }
}

fn cannot_switch_mode(pane: PaneId) -> Mode {
    Mode::Picker(Picker::new(
        PickerKind::Agent,
        "Can't switch this process.",
        vec![PickItem::new(
            "Open with another account",
            "The current pane keeps running",
            PickAction::OpenAnotherAccount(pane),
        )],
    ))
}

/// Whether a conversation can move between accounts of this agent (they share its storage).
fn conversations_move(agent: &str) -> bool {
    crate::agent_registry::find(agent).is_some_and(|a| a.cross_account_resume)
}

/// Why a pane waits for Enter before its agent starts again.
pub(super) const AMBIGUOUS: &str =
    "kraal cannot tell this command's options from a prompt it would send again: check it, Enter runs it as shown";

impl Server {
    pub(super) fn update_accounts<T>(&mut self, f: impl FnOnce(&mut Accounts) -> Result<T>) -> Result<T> {
        Accounts::update_with_sources(&Accounts::path(), f, |written| self.record_source_write(written))
    }

    pub(super) fn reload_accounts(&mut self) {
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

    fn check_existing_accounts(&mut self) {
        for a in self.accounts.unlisted().into_iter().chain(self.accounts.reviews()) {
            if self.existing_checks.contains_key(&a.agent)
                || self.panes.values().any(|p| {
                    p.spec.login.as_ref().is_some_and(|l| {
                        l.account.agent == a.agent && matches!(l.state, LoginState::Running | LoginState::Checking)
                    })
                })
            {
                continue;
            }
            self.existing_checks.insert(a.agent.clone(), ExistingCheck::Checking);
            let checked = a.clone();
            let named = self.accounts.for_agent(&a.agent);
            let mut cached: Vec<_> = named
                .iter()
                .filter_map(|a| self.account_identities.get(&a.id).filter(|(b, _)| a == b).cloned())
                .collect();
            let missing: Vec<_> = named.into_iter().filter(|a| !cached.iter().any(|(b, _)| a == b)).collect();
            let agent = a.agent.clone();
            let generation = self.account_identity_generations.get(&agent).copied().unwrap_or_default();
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
                    if server.account_identity_generations.get(&a.agent).copied().unwrap_or_default() != generation {
                        return;
                    }
                    if matches!(result.status, Status::SignedIn | Status::CouldNotCheck) {
                        server.save_existing_account(a, result, cached, generation);
                    } else {
                        server.existing_checks.insert(a.agent, ExistingCheck::Checked(result.status));
                        server.reload_accounts();
                        server.reload_usage();
                        server.refresh_agent_pickers();
                        server.mark_dirty();
                    }
                },
            );
            if !started {
                self.existing_checks.insert(agent, ExistingCheck::Checked(Status::CouldNotCheck));
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
                let mut written = Vec::new();
                let saved = account_status::save_discovery(&[(a.clone(), result)], &cached, |source| {
                    written.push(source.clone());
                });
                (a, cached, saved, written)
            },
            move |server, (a, cached, saved, written)| {
                // Migrations may have succeeded even when saving failed or a login started.
                for source in written {
                    server.record_source_write(&source);
                }
                if server.account_identity_generations.get(&a.agent).copied().unwrap_or_default() != generation {
                    server.reload_accounts();
                    return;
                }
                server.account_identities.extend(cached.into_iter().map(|(a, c)| (a.id.clone(), (a, c))));
                server.existing_checks.insert(a.agent.clone(), ExistingCheck::Checked(status));
                match saved {
                    Ok(notices) => {
                        for notice in notices {
                            server.queue_account_notice(notice);
                        }
                        server.reload_accounts();
                        server.reload_usage();
                    }
                    Err(e) => {
                        server.existing_checks.insert(a.agent, ExistingCheck::SaveFailed);
                        server.warnings.push(format!("Could not adopt the existing account: {e:#}. Open Check again."));
                    }
                }
                server.refresh_agent_pickers();
                server.mark_dirty();
            },
        );
        if !started {
            self.existing_checks.insert(agent, ExistingCheck::SaveFailed);
        }
    }

    fn refresh_agent_pickers(&mut self) {
        let forms: Vec<_> = self
            .clients
            .iter()
            .filter_map(|(&id, c)| match &c.mode {
                Mode::AccountForm(form) if form.checking.is_none() => Some((id, form.clone())),
                _ => None,
            })
            .collect();
        for (id, mut form) in forms {
            self.fill_account_form(&mut form);
            self.clients.get_mut(&id).unwrap().mode = Mode::AccountForm(form);
        }
        let clients: Vec<_> = self
            .clients
            .iter()
            .filter_map(|(&id, c)| {
                if let Mode::Picker(p) = &c.mode
                    && ((p.kind == PickerKind::Agent && p.title == "Start an agent") || p.kind == PickerKind::Accounts)
                    && p.actions.is_none()
                {
                    Some((id, p.kind, p.state()))
                } else {
                    None
                }
            })
            .collect();
        for (id, kind, state) in clients {
            let mut p =
                if kind == PickerKind::Accounts { self.accounts_picker(None) } else { self.agent_picker(id, None) };
            p.restore(state);
            self.clients.get_mut(&id).unwrap().mode = Mode::Picker(p);
        }
    }

    pub(super) fn refresh_existing_accounts(&mut self) {
        self.existing_checks.retain(|_, status| {
            matches!(
                status,
                ExistingCheck::Checking
                    | ExistingCheck::Checked(Status::SignedIn | Status::CouldNotCheck)
                    | ExistingCheck::SaveFailed
            )
        });
        self.check_existing_accounts();
    }

    fn invalidate_account_identities(&mut self, agent: &str) {
        *self.account_identity_generations.entry(agent.into()).or_default() += 1;
        self.account_identities.retain(|_, (a, _)| a.agent != agent);
        self.existing_checks.remove(agent);
    }

    pub(super) fn retry_existing_account(&mut self, agent: &str) {
        if self.existing_checks.get(agent).is_some_and(|state| *state != ExistingCheck::Checking) {
            self.invalidate_account_identities(agent);
            self.check_existing_accounts();
        }
    }

    pub(super) fn review_account_picker(&self, existing: &Account) -> Picker {
        let mut items: Vec<_> = self
            .accounts
            .for_agent(&existing.agent)
            .into_iter()
            .map(|a| {
                PickItem::new(
                    a.name.clone(),
                    "",
                    PickAction::UseExisting { existing: Box::new(existing.clone()), target: Some(a) },
                )
            })
            .collect();
        items.push(PickItem::new(
            "A different account",
            "",
            PickAction::UseExisting { existing: Box::new(existing.clone()), target: None },
        ));
        Picker::new(PickerKind::AccountReview, &accounts::existing_label(&existing.agent), items)
    }

    pub(super) fn confirm_existing_account(
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
    fn follow_renamed_accounts(&mut self) {
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
    pub(super) fn rename_account(&mut self, agent: &str, old: &str, new: &str) -> Result<()> {
        self.update_accounts(|all| all.rename(agent, old, new))?;
        self.reload_accounts();
        self.reload_usage();
        Ok(())
    }

    pub(super) fn reload_usage(&mut self) {
        self.usage = UsageMap::load(&self.accounts);
        self.await_usage_reset();
    }

    /// Wake when the usage window that resets next does: what the clients show of the accounts
    /// changes then, with nothing else happening. Called when the usage changed and when that
    /// moment comes; a reset awaited that has come makes every client's frame due, whichever
    /// call sees it first (the clock may have been set, or the machine asleep meanwhile).
    pub(super) fn await_usage_reset(&mut self) {
        let now = accounts::now();
        if self.usage_reset.is_some_and(|(at, _)| at <= now) {
            self.mark_dirty();
        }
        // Whole seconds from a clock that counts them: the wake is not before the reset. A reset
        // said to come later than the clock can tell (a usage file may say anything) is not
        // waited for.
        let wake = |at: i64| Instant::now().checked_add(Duration::from_secs(u64::try_from(at - now).unwrap_or(0)));
        self.usage_reset = self.usage.next_end(now).and_then(|at| Some((at, wake(at)?)));
    }

    /// The account of a managed pane, or of an unambiguously identified foreground agent.
    pub fn pane_account(&self, pane: PaneId) -> Option<Account> {
        let spec = &self.panes.get(&pane)?.spec;
        if spec.account.is_some() || spec.account_id.is_some() {
            self.spec_account(spec)
        } else {
            self.observed_agents.get(&pane).map(|a| a.account.clone())
        }
    }

    fn observed_agent(&self, pane: PaneId) -> Result<Option<ObservedAgent>> {
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

    pub(super) fn refresh_observed_accounts(&mut self) {
        let observed = self
            .panes
            .keys()
            .filter(|id| {
                let spec = &self.panes[id].spec;
                spec.account.is_none() && spec.account_id.is_none()
            })
            .filter_map(|&id| {
                // Keep the label through an unreadable process table. Switching still requires a fresh check.
                self.observed_agent(id).unwrap_or_else(|_| self.observed_agents.get(&id).cloned()).map(|a| (id, a))
            })
            .collect();
        if self.observed_agents != observed {
            self.observed_agents = observed;
            self.mark_dirty();
        }
    }

    pub(super) fn change_account_mode(&mut self, client: ClientId, pane: PaneId) -> Mode {
        if let Some(agent) = self
            .panes
            .get(&pane)
            .and_then(|p| p.agent.kind.as_deref())
            .and_then(crate::agent_registry::find)
            .filter(|a| a.accounts.is_none())
        {
            self.toast(client, NoticeKind::Notice, agent.account_message());
            return Mode::Normal;
        }
        if self.switches.contains_key(&pane) {
            return self.pending_switch_mode(pane);
        }
        let managed = self.panes.get(&pane).is_some_and(|p| p.spec.account.is_some() || p.spec.account_id.is_some());
        if !managed {
            match self.observed_agent(pane) {
                Ok(Some(a)) if accounts::check_resume(&a.account, &a.session).is_ok() => {
                    self.observed_agents.insert(pane, a);
                }
                Ok(_) => {
                    self.observed_agents.remove(&pane);
                }
                Err(error) => {
                    self.toast(client, NoticeKind::Error, error.to_string());
                    return Mode::Normal;
                }
            }
        }
        if self.pane_account(pane).is_some() {
            return Mode::Picker(self.agent_picker(client, Some(pane)));
        }
        cannot_switch_mode(pane)
    }

    pub(super) fn undo_account_change(&mut self, pane: PaneId, client: ClientId) -> Result<()> {
        let saved =
            self.panes.get(&pane).and_then(|p| p.spec.account_undo.as_deref()).context("no account change to undo")?;
        let to = self.spec_account(saved).context("the previous account was removed; open Change account")?;
        self.begin_switch(pane, &to, Some(client), false, true)
    }

    fn spec_account(&self, spec: &PaneSpec) -> Option<Account> {
        match spec.account_id.as_deref() {
            Some(id) => self.accounts.by_id(id).map(|a| self.accounts.at_launch_home(a, &spec.env)),
            None => self.accounts.launched_on(spec.account.as_deref()?, &spec.env),
        }
    }

    /// Spec of a pane running `agent` on account `a`, resuming `resume` if given.
    pub fn agent_spec(
        &self,
        a: &Account,
        cwd: Option<PathBuf>,
        resume: Option<&str>,
        args: &[String],
    ) -> Result<PaneSpec> {
        let kind = accounts::agent_kind(&a.agent).context("unknown agent")?;
        let session = match resume {
            Some(id) => Some(id.to_string()),
            None => kind.session_flag.map(|_| accounts::new_uuid()),
        };
        let new_session = if resume.is_none() { session.as_deref() } else { None };
        let l = accounts::launch(a, args, resume, new_session)?;
        Ok(PaneSpec {
            command: l.argv,
            env: l.env,
            env_remove: l.env_remove,
            cwd,
            account: Some(accounts::key(&a.agent, &a.name)),
            account_id: Some(a.id.clone()),
            session,
            ..Default::default()
        })
    }

    /// Rebuild an agent pane's spec so that starting it again resumes its conversation, with
    /// the options it was started with (never a prompt given on its command line: it would be
    /// sent again). `session` is the conversation seen running; the id the pane was started
    /// with counts only once a conversation exists under it (a pane that never ran starts the
    /// conversation it was meant to). The flag says whether that may happen by itself (a
    /// restore): not for a one-shot run (`claude -p`), which stays as it was, nor when a prompt
    /// was left out or would be sent; those wait for Enter.
    pub fn resume_spec(&self, spec: &PaneSpec, session: Option<&str>) -> (PaneSpec, bool) {
        let mut bound = spec.clone();
        self.accounts.bind_pane(&mut bound);
        let spec = &bound;
        let Some(a) = self.spec_account(spec) else {
            return (spec.clone(), false);
        };
        let Some(kind) = accounts::agent_kind(&a.agent) else {
            return (spec.clone(), false);
        };
        let Some((args, exact)) = accounts::resumable_args(kind, &accounts::launch_args(kind, &spec.command)) else {
            return (spec.clone(), false);
        };
        // When its files cannot be read, the conversation may exist: resume it, but only on Enter.
        let (reserved, readable) = match spec.session.as_deref().map(|id| (id, accounts::conversation_exists(&a, id))) {
            Some((id, Ok(seen))) => ((seen != Some(false)).then_some(id), true),
            Some((id, Err(_))) => (Some(id), false),
            None => (None, true),
        };
        let exact = exact && readable;
        let Some(session) = session.or(reserved) else { return (spec.clone(), exact) };
        match self.agent_spec(&a, spec.cwd.clone(), Some(session), &args) {
            Ok(fresh) => {
                let mut fresh = PaneSpec {
                    worktree: spec.worktree.clone(),
                    close_on_exit: spec.close_on_exit,
                    account_undo: spec.account_undo.clone(),
                    ..fresh
                };
                if let Some(program) = spec.command.first().filter(|p| std::path::Path::new(p).is_absolute()) {
                    fresh.command[0] = program.clone();
                }
                fresh.env.extend(spec.env.iter().filter(|(k, _)| k == "PATH").cloned());
                crate::worktree_setup::carry_env(&spec.env, &mut fresh.env);
                (fresh, exact)
            }
            Err(_) => (spec.clone(), false),
        }
    }

    /// The spec to start pane `p` with again, if it is an agent that had a conversation: the
    /// conversation is resumed rather than a new one started. Agents run as plain commands
    /// (`kraal run -- claude`) too, when their conversation is known. The flag is false when the
    /// command must be shown before it runs (see [`accounts::resumable_args`]).
    pub fn restart_spec(&self, p: PaneId) -> Option<(PaneSpec, bool)> {
        let pane = self.panes.get(&p)?;
        let session = pane.agent.session_id.as_deref();
        if pane.spec.account.is_some() {
            return Some(self.resume_spec(&pane.spec, session));
        }
        let program = pane.spec.command.first()?.rsplit('/').next()?;
        let kind = accounts::agents().find(|k| k.command == program)?;
        let (command, exact) = accounts::resume_argv(kind, &pane.spec.command, session?)?;
        Some((PaneSpec { command, ..pane.spec.clone() }, exact))
    }

    /// Whether pane `p`'s agent can be started again on its own, without showing its command
    /// first: false when kraal could not tell its options from a prompt it would send again.
    pub(super) fn restarts_by_itself(&self, p: PaneId) -> bool {
        let Some(pane) = self.panes.get(&p) else { return false };
        let Some(kind) =
            pane.spec.account.as_deref().and_then(|k| k.split_once(':')).and_then(|(a, _)| accounts::agent_kind(a))
        else {
            return false;
        };
        accounts::resumable_args(kind, &accounts::launch_args(kind, &pane.spec.command)).is_some_and(|(_, exact)| exact)
    }

    /// Start `agent` in a new pane, on `account` or the one the rules and thresholds pick.
    pub fn launch_agent(
        &mut self,
        ws: u32,
        target: Option<PaneId>,
        agent: &str,
        account: Option<&str>,
        placement: Placement,
        cwd: Option<PathBuf>,
    ) -> Result<PaneId> {
        self.launch_agent_with(ws, target, agent, account, placement, cwd, &[])
    }

    /// [`Server::launch_agent`], giving the agent `args` too.
    #[expect(clippy::too_many_arguments, reason = "the arguments of `launch_agent`, and the agent's own")]
    pub fn launch_agent_with(
        &mut self,
        ws: u32,
        target: Option<PaneId>,
        agent: &str,
        account: Option<&str>,
        placement: Placement,
        cwd: Option<PathBuf>,
        args: &[String],
    ) -> Result<PaneId> {
        let registered = crate::agent_registry::find(agent).with_context(|| format!("unknown agent {agent:?}"))?;
        if registered.accounts.is_none() {
            let cwd = self.agent_cwd(ws, target, cwd);
            let launch = registered.native_launch(account, args, cwd.as_deref())?;
            let spec = PaneSpec { command: launch.argv, cwd, ..Default::default() };
            return self.spawn_pane_in(ws, target, spec, placement, Some(agent.into()));
        }
        let wi = self.ws_index(ws).context("no such workspace")?;
        let ws_name = self.workspaces[wi].name.clone();
        let cwd = self.agent_cwd(ws, target, cwd);
        let a = match account {
            Some(name) => self.accounts.get(agent, name).with_context(|| accounts::missing_account(agent, name))?,
            None => {
                // Never a pick among accounts a rule may forbid here: git says first which
                // repository this directory belongs to (callers that can wait do so).
                if let Some(dir) = cwd.clone().filter(|d| self.accounts.undecided(agent, Some(d))) {
                    self.ask_git_about(dir);
                    bail!(
                        "which account the rules allow here depends on this directory's repository; try again in a moment"
                    );
                }
                self.accounts
                    .choose(agent, Some(&ws_name), cwd.as_deref(), &self.usage, accounts::now())
                    .context("no account allowed here: the matching rule in accounts.toml names none")?
            }
        };
        let spec = self.agent_spec(&a, cwd, None, args)?;
        self.spawn_pane_in(ws, target, spec, placement, Some(a.agent.clone()))
    }

    /// Where an agent started in workspace `ws` next to `target` works: `cwd` if given, else the
    /// directory of that pane (or the focused one), when it is on this machine.
    pub(super) fn agent_cwd(&self, ws: u32, target: Option<PaneId>, cwd: Option<PathBuf>) -> Option<PathBuf> {
        cwd.or_else(|| {
            let wi = self.ws_index(ws)?;
            let w = &self.workspaces[wi];
            let focus = target.or_else(|| w.tabs.get(w.active).and_then(|t| t.focus));
            focus.and_then(|p| self.panes.get(&p)).filter(|p| p.ssh.is_none()).and_then(|p| p.current_cwd())
        })
    }

    /// Reserve the pane before the asynchronous check, so attached clients share one change.
    pub(super) fn switch_account(
        &mut self,
        pane: PaneId,
        to: &Account,
        client: Option<ClientId>,
        ask: bool,
    ) -> Result<()> {
        self.begin_switch(pane, to, client, ask, false)
    }

    fn begin_switch(
        &mut self,
        pane: PaneId,
        to: &Account,
        client: Option<ClientId>,
        ask: bool,
        undo: bool,
    ) -> Result<()> {
        let p = self.panes.get(&pane).context("no such pane")?;
        let observed = if p.spec.account.is_none() && p.spec.account_id.is_none() {
            Some(self.observed_agent(pane)?.context("Can't switch this process. Open with another account.")?)
        } else {
            None
        };
        let from = observed
            .as_ref()
            .map(|a| a.account.clone())
            .or_else(|| self.pane_account(pane))
            .context("Can't switch this process. Open with another account.")?;
        anyhow::ensure!(from.agent == to.agent, "{} cannot continue on a {} account", from.agent, to.agent);
        if from.id == to.id || from.same_home(to) {
            return Ok(());
        }
        anyhow::ensure!(!self.switches.contains_key(&pane), "an account change is already pending for this pane");
        anyhow::ensure!(!self.pane_edits.contains_key(&pane), "a pane edit is already being applied");
        let kind = accounts::agent_kind(&to.agent).context("unknown agent")?;
        let mut original = match &observed {
            Some(a) => PaneSpec {
                command: a.command.clone(),
                cwd: Some(a.cwd.clone()),
                session: Some(a.session.clone()),
                account: Some(accounts::key(&from.agent, &from.name)),
                account_id: Some(from.id.clone()),
                env: vec![(kind.home_env.into(), from.home.clone()), ("PATH".into(), a.path.clone())],
                ..Default::default()
            },
            None => p.spec.clone(),
        };
        original.account = Some(accounts::key(&from.agent, &from.name));
        original.account_id = Some(from.id.clone());
        accounts::resumable_args(kind, &accounts::launch_args(kind, &original.command))
            .context("a one-shot run (-p, exec) is not a conversation to continue")?;
        let process = p.process_hold();
        let token = self.alloc_id();
        self.switches.insert(
            pane,
            AccountSwitch {
                token,
                from,
                to: to.clone(),
                session: None,
                process,
                client,
                ask,
                original,
                observed,
                undo,
                started: None,
                stopping: None,
                stage: SwitchStage::Checking,
            },
        );
        if let Some(client) = client {
            self.toast(client, NoticeKind::Notice, format!("Checking sign-in for {}", accounts::label(to)));
        }
        self.check_switch(pane, false);
        Ok(())
    }

    fn check_switch(&mut self, pane: PaneId, approved: bool) {
        let Some(switch) = self.switches.get_mut(&pane) else { return };
        switch.stage = SwitchStage::Checking;
        let (token, to) = (switch.token, switch.to.clone());
        if !self.background(
            move || account_status::check(&to),
            move |server, status| server.switch_checked(pane, token, status, approved),
        ) {
            self.switch_checked(pane, token, Status::CouldNotCheck, approved);
        }
        self.refresh_switch_views(pane);
        self.mark_dirty();
    }

    fn switch_checked(&mut self, pane: PaneId, token: u32, status: Status, approved: bool) {
        let Some(switch) = self.switches.get(&pane).filter(|s| s.token == token && s.stage == SwitchStage::Checking)
        else {
            return;
        };
        if !self.panes.contains_key(&pane) {
            self.switches.remove(&pane);
            self.refresh_switch_views(pane);
            return;
        }
        if status != Status::SignedIn {
            let message =
                format!("{}: {}.\nThe current agent keeps running.", accounts::label(&switch.to), status.label());
            self.switch_failed(pane, &message, status == Status::NotSignedIn);
        } else if let Err(e) = self.prepare_switch(pane, approved) {
            self.switch_failed(pane, &format!("{e:#}. The current pane is unchanged."), false);
        }
    }

    fn switch_failed(&mut self, pane: PaneId, reason: &str, sign_in: bool) {
        let Some(switch) = self.switches.remove(&pane) else { return };
        self.refresh_switch_views(pane);
        let unsupported = switch
            .observed
            .as_ref()
            .is_some_and(|before| self.observed_agent(pane).is_ok_and(|now| now.as_ref() != Some(before)));
        let client = switch.client.or(self.primary);
        if let Some(c) = client.and_then(|id| self.clients.get_mut(&id)).filter(|c| matches!(c.mode, Mode::Normal)) {
            let action = if sign_in {
                ConfirmAction::SignInAccount(switch.to)
            } else {
                ConfirmAction::RetryAccountSwitch { pane, account: switch.to, undo: switch.undo }
            };
            c.mode = if unsupported {
                cannot_switch_mode(pane)
            } else {
                Mode::confirm(format!("Could not change account\n{reason}"), action)
            };
            self.mark_dirty();
        } else {
            self.notify(
                NoticeKind::Error,
                "Could not change account",
                &format!("{reason} Open Change account to try again."),
                Some(pane),
            );
        }
    }

    fn prepare_switch(&mut self, pane: PaneId, approved: bool) -> Result<()> {
        let switch = self.switches.get(&pane).context("no pending account change")?;
        let p = self.panes.get(&pane).context("the pane was closed")?;
        let same_process = match (&switch.process, p.process_hold()) {
            (Some(before), Some(now)) => before.is(&now),
            (None, None) => true,
            _ => false,
        };
        anyhow::ensure!(same_process, "the pane's process changed; choose an account again");
        if let Some(observed) = &switch.observed {
            anyhow::ensure!(
                self.observed_agent(pane)?.as_ref() == Some(observed),
                "Can't switch this process. Open with another account."
            );
        }
        let from = switch
            .observed
            .as_ref()
            .map(|a| a.account.clone())
            .or_else(|| self.pane_account(pane))
            .context("the pane's account was removed")?;
        anyhow::ensure!(
            from.id == switch.from.id && from.same_home(&switch.from),
            "the pane's account changed; choose an account again"
        );
        let to = self.accounts.by_id(&switch.to.id).context("the target account was removed")?;
        anyhow::ensure!(to.same_home(&switch.to), "the target account changed; choose an account again");
        if switch.client.is_none() {
            anyhow::ensure!(self.restarts_by_itself(pane), "this command now needs confirmation before restarting");
            anyhow::ensure!(
                !to.sign_in_required
                    && self.usage.peak(&to, accounts::now()).is_some_and(|p| p < self.accounts.threshold(&to) as f64),
                "the target account no longer has verified usage below its threshold"
            );
        }
        let session = if switch.observed.is_some() {
            switch.original.session.clone()
        } else {
            p.agent.session_id.clone().or_else(|| switch.original.session.clone())
        };
        if conversations_move(&from.agent) {
            let id = session.as_deref().context("could not verify the Claude conversation id")?;
            accounts::check_carry(&from, &to, id)?;
        } else {
            // A fresh Codex launch has no hook id until its first completed turn. Undo can
            // already return to the saved conversation; any new transcript stays in this home.
            if let Some(id) = session.as_deref() {
                accounts::check_resume(&from, id)?;
            } else {
                anyhow::ensure!(switch.undo, "conversation id is unavailable; finish a turn and try again");
            }
            if switch.undo {
                let saved = switch.original.account_undo.as_deref().context("no account change to undo")?;
                accounts::check_resume(
                    &to,
                    saved.session.as_deref().context("the original conversation id is unavailable")?,
                )?;
            }
        }
        let hooked = p.agent.hooked
            && switch.observed.as_ref().is_none_or(|a| p.agent.session_process == Some((a.pid, a.started)));
        let timing = switch_timing(hooked, p.agent.state, p.exit_code.is_some());
        let stage = if approved {
            SwitchStage::Stopping
        } else if switch.ask || !conversations_move(&to.agent) && timing == SwitchStage::Stopping {
            SwitchStage::Confirm
        } else {
            timing
        };
        let exited = p.exit_code.is_some();
        let client = switch.client;
        let switch = self.switches.get_mut(&pane).unwrap();
        switch.original.session = session.clone();
        if switch.observed.is_none() {
            switch.original.cwd = p.current_cwd().or_else(|| switch.original.cwd.clone());
        }
        switch.session = session;
        switch.stage = stage;
        match stage {
            SwitchStage::Waiting => {
                self.notify(
                    NoticeKind::Notice,
                    "Switch after this turn",
                    &format!("{}; open Change account to cancel", accounts::label(&to)),
                    Some(pane),
                );
            }
            SwitchStage::Confirm => {
                if let Some(client) = client.or(self.primary) {
                    let mode = self.pending_switch_mode(pane);
                    if let Some(c) = self.clients.get_mut(&client).filter(|c| matches!(c.mode, Mode::Normal)) {
                        c.mode = mode;
                    } else {
                        self.notify(
                            NoticeKind::Warning,
                            "Stop and switch?",
                            "Open Change account to confirm or cancel",
                            Some(pane),
                        );
                    }
                } else {
                    self.switches.remove(&pane);
                    self.notify(
                        NoticeKind::Warning,
                        "Account change needs confirmation",
                        "Open Change account to try again",
                        Some(pane),
                    );
                }
            }
            SwitchStage::Stopping => {
                self.stop_switch(pane, SwitchStage::Stopping);
                if exited {
                    self.finish_switch(pane);
                }
            }
            SwitchStage::Checking
            | SwitchStage::Starting
            | SwitchStage::Restoring
            | SwitchStage::Recovering
            | SwitchStage::RecoveryStopping => {
                unreachable!()
            }
        }
        self.refresh_switch_views(pane);
        self.mark_dirty();
        Ok(())
    }

    fn refresh_switch_views(&mut self, pane: PaneId) {
        let clients: Vec<ClientId> = self
            .clients
            .iter()
            .filter_map(|(&id, c)| {
                let pending = match &c.mode {
                    Mode::Confirm(q) => matches!(q.action, ConfirmAction::StopAndSwitch { pane: p, .. } if p == pane),
                    Mode::Picker(p) => p
                        .items
                        .iter()
                        .any(|i| matches!(i.action, PickAction::CancelSwitch { pane: p, .. } if p == pane)),
                    _ => false,
                };
                pending.then_some(id)
            })
            .collect();
        for id in clients {
            let mode = self.pending_switch_mode(pane);
            self.clients.get_mut(&id).unwrap().mode = mode;
        }
    }

    pub(super) fn pending_switch_mode(&self, pane: PaneId) -> Mode {
        let Some(switch) = self.switches.get(&pane) else { return Mode::Normal };
        if matches!(
            switch.stage,
            SwitchStage::Stopping
                | SwitchStage::Starting
                | SwitchStage::Restoring
                | SwitchStage::Recovering
                | SwitchStage::RecoveryStopping
        ) {
            return Mode::Normal;
        }
        if switch.stage == SwitchStage::Confirm {
            let effect = if conversations_move(&switch.to.agent) || switch.undo {
                "Continue conversation"
            } else {
                "New conversation"
            };
            let reason = if switch.ask {
                format!("{} reached its usage threshold.\n", accounts::label(&switch.from))
            } else {
                String::new()
            };
            return Mode::confirm(
                format!(
                    "Stop and switch?\n{reason}{}: {effect} on {}. This stops the current agent.",
                    self.pane_label(pane),
                    accounts::label(&switch.to)
                ),
                ConfirmAction::StopAndSwitch { pane, token: switch.token },
            );
        }
        let detail = match switch.stage {
            SwitchStage::Waiting => "Switch after this turn",
            SwitchStage::Checking => "Checking sign-in",
            SwitchStage::Stopping
            | SwitchStage::Confirm
            | SwitchStage::Starting
            | SwitchStage::Restoring
            | SwitchStage::Recovering
            | SwitchStage::RecoveryStopping => unreachable!(),
        };
        Mode::Picker(Picker::new(
            PickerKind::Agent,
            "Change account",
            vec![PickItem {
                label: "Cancel account change".into(),
                detail: format!("{detail} · {}", accounts::label(&switch.to)),
                action: PickAction::CancelSwitch { pane, token: switch.token },
                state: None,
                dim: false,
            }],
        ))
    }

    pub(super) fn cancel_switch(&mut self, pane: PaneId, token: u32) {
        if self.switches.get(&pane).is_some_and(|s| {
            s.token == token && matches!(s.stage, SwitchStage::Checking | SwitchStage::Waiting | SwitchStage::Confirm)
        }) {
            self.switches.remove(&pane);
            self.refresh_switch_views(pane);
            self.mark_dirty();
        }
    }

    pub(super) fn confirm_switch(&mut self, pane: PaneId, token: u32, client: ClientId, yes: bool) {
        if !self.switches.get(&pane).is_some_and(|s| s.token == token && s.stage == SwitchStage::Confirm) {
            return;
        }
        if yes {
            self.switches.get_mut(&pane).unwrap().client = Some(client);
            self.check_switch(pane, true);
        } else {
            self.cancel_switch(pane, token);
        }
    }

    pub(super) fn check_switches(&mut self) {
        self.switches.retain(|p, _| self.panes.contains_key(p));
        let ready: Vec<PaneId> = self
            .switches
            .iter()
            .filter_map(|(&id, s)| {
                let p = self.panes.get(&id)?;
                (s.stage == SwitchStage::Waiting
                    && switch_timing(p.agent.hooked, p.agent.state, p.exit_code.is_some()) != SwitchStage::Waiting)
                    .then_some(id)
            })
            .collect();
        for pane in ready {
            self.check_switch(pane, false);
        }
        let stopping: Vec<_> = self
            .switches
            .iter()
            .filter_map(|(&id, s)| {
                matches!(s.stage, SwitchStage::Stopping | SwitchStage::Recovering | SwitchStage::RecoveryStopping)
                    .then_some((id, s.stage))
            })
            .collect();
        for (id, stage) in stopping {
            if stage == SwitchStage::Stopping {
                self.finish_switch(id);
            } else {
                self.switch_start_failed(id, "the agent did not start");
            }
        }
        let starting: Vec<_> = self
            .switches
            .iter()
            .filter_map(|(&id, s)| matches!(s.stage, SwitchStage::Starting | SwitchStage::Restoring).then_some(id))
            .collect();
        for id in starting {
            let Some(p) = self.panes.get(&id).filter(|p| !p.suspended) else { continue };
            let s = self.switches.get_mut(&id).unwrap();
            let elapsed = s.started.get_or_insert_with(Instant::now).elapsed();
            let expected = if s.stage == SwitchStage::Restoring { &s.from.agent } else { &s.to.agent };
            let running = p.pid().is_some_and(|pid| crate::process::running(pid as libc::pid_t));
            if running
                && p.has_printed()
                && p.agent.kind.as_ref() == Some(expected)
                && elapsed >= Duration::from_secs(1)
            {
                self.switch_started(id);
            } else if elapsed >= Duration::from_secs(10) {
                if running {
                    // Wait for the last output and exit before another process takes this pane.
                    let stage = if s.stage == SwitchStage::Restoring {
                        SwitchStage::RecoveryStopping
                    } else {
                        SwitchStage::Recovering
                    };
                    self.stop_switch(id, stage);
                } else {
                    self.switch_start_failed(id, "the agent did not start");
                }
            }
        }
    }

    fn stop_switch(&mut self, pane: PaneId, stage: SwitchStage) {
        let switch = self.switches.get_mut(&pane).unwrap();
        switch.stage = stage;
        let p = &self.panes[&pane];
        switch.stopping = ProgramStop::new(p);
        if let Some(stop) = &switch.stopping {
            stop.signal(libc::SIGHUP);
        }
    }

    fn switch_stopped(&mut self, pane: PaneId) -> bool {
        let Some(stop) = self.switches.get_mut(&pane).and_then(|s| s.stopping.as_mut()) else { return true };
        stop.finished() && self.panes.get(&pane).is_none_or(|p| p.exit_code.is_some())
    }

    /// Keep the source until the replacement reports a usable start, including an early exit.
    pub(super) fn finish_switch(&mut self, pane: PaneId) {
        if !self.switch_stopped(pane) {
            return;
        }
        let Some(switch) = self.switches.get(&pane).filter(|s| s.stage == SwitchStage::Stopping) else { return };
        let (from, to, session, original, undo) =
            (switch.from.clone(), switch.to.clone(), switch.session.clone(), switch.original.clone(), switch.undo);
        let result = (|| -> Result<()> {
            anyhow::ensure!(
                self.accounts.by_id(&to.id).is_some_and(|a| a.same_home(&to)),
                "the target account changed while the agent was stopping"
            );
            let kind = accounts::agent_kind(&to.agent).context("unknown agent")?;
            let resume = if undo && to.agent == "codex" {
                original.account_undo.as_ref().and_then(|s| s.session.clone())
            } else {
                match &session {
                    Some(id) => accounts::carry_conversation(&from, &to, id)?.then(|| id.clone()),
                    None => None,
                }
            };
            let source = if undo && to.agent == "codex" {
                original.account_undo.as_deref().context("no account change to undo")?
            } else {
                &original
            };
            let (args, exact) = accounts::resumable_args(kind, &accounts::launch_args(kind, &source.command))
                .context("a one-shot run is not a conversation to continue")?;
            let mut spec = PaneSpec {
                worktree: original.worktree.clone(),
                close_on_exit: original.close_on_exit,
                ..self.agent_spec(&to, source.cwd.clone(), resume.as_deref(), &args)?
            };
            if let Some(program) = source.command.first().filter(|p| std::path::Path::new(p).is_absolute()) {
                spec.command[0] = program.clone();
            }
            spec.env.extend(source.env.iter().filter(|(k, _)| k == "PATH").cloned());
            crate::worktree_setup::carry_env(&original.env, &mut spec.env);
            self.switches.get_mut(&pane).unwrap().stage = SwitchStage::Starting;
            if !undo {
                let mut previous = original.clone();
                previous.account_undo = None;
                spec.account_undo = Some(Box::new(previous));
            }
            if !exact {
                self.suspend_pane(pane, spec, AMBIGUOUS)?;
                let p = self.panes.get_mut(&pane).unwrap();
                p.agent.session_id = p.spec.session.clone();
                return Ok(());
            }
            self.start_switch_spec(pane, spec)
        })();
        if let Err(e) = result {
            self.switch_start_failed(pane, &format!("{e:#}"));
        }
        self.refresh_switch_views(pane);
        self.mark_persist();
        self.relayout();
    }

    fn start_switch_spec(&mut self, pane: PaneId, spec: PaneSpec) -> Result<()> {
        let p = self.panes.get_mut(&pane).context("the pane was closed")?;
        p.spec = spec;
        p.respawn(&self.env, &self.tx)?;
        // A new Codex conversation must not inherit the source conversation's hook identity.
        p.agent.session_id = p.spec.session.clone();
        let switch = self.switches.get_mut(&pane).unwrap();
        switch.started = Some(Instant::now());
        switch.stopping = None;
        Ok(())
    }

    pub(super) fn switch_start_failed(&mut self, pane: PaneId, reason: &str) {
        if let Some(switch) = self.switches.get(&pane)
            && switch.stopping.is_none()
        {
            let stage = if matches!(switch.stage, SwitchStage::Restoring | SwitchStage::RecoveryStopping) {
                SwitchStage::RecoveryStopping
            } else {
                SwitchStage::Recovering
            };
            self.stop_switch(pane, stage);
        }
        if !self.switch_stopped(pane) {
            return;
        }
        let Some(switch) = self.switches.get_mut(&pane) else { return };
        if matches!(switch.stage, SwitchStage::Restoring | SwitchStage::RecoveryStopping) {
            self.switch_recovery_failed(pane, reason);
            return;
        }
        switch.stage = SwitchStage::Restoring;
        let original = switch.original.clone();
        let (spec, exact) = self.resume_spec(&original, original.session.as_deref());
        if !exact {
            self.switch_recovery_failed(pane, "the previous command needs review before it can run again");
            return;
        }
        if let Err(e) = self.start_switch_spec(pane, spec) {
            self.switch_recovery_failed(pane, &format!("{e:#}"));
        }
        self.mark_persist();
        self.relayout();
    }

    fn switch_recovery_failed(&mut self, pane: PaneId, reason: &str) {
        let Some(switch) = self.switches.remove(&pane) else { return };
        let saved = switch.original.session.as_deref().is_some_and(|s| accounts::check_resume(&switch.from, s).is_ok());
        let message = if saved {
            "Couldn't resume. Your conversation is saved. Enter retries on the previous account."
        } else {
            "Couldn't resume. The previous command and pane output are available. Enter retries."
        };
        let (spec, _) = self.resume_spec(&switch.original, switch.original.session.as_deref());
        if let Some(p) = self.panes.get_mut(&pane) {
            p.spec = spec;
            p.agent = crate::agent::AgentTracker { session_id: p.spec.session.clone(), ..Default::default() };
            p.exit_code = Some(None);
        }
        self.notify(NoticeKind::Error, "Could not change account", &format!("{message} {reason}"), Some(pane));
        self.refresh_switch_views(pane);
        self.mark_persist();
    }

    fn switch_started(&mut self, pane: PaneId) {
        let Some(switch) = self.switches.remove(&pane) else { return };
        if switch.stage == SwitchStage::Restoring {
            self.notify(
                NoticeKind::Error,
                "Could not change account",
                &format!("Resumed on {}. Open Change account to try again.", switch.from.name),
                Some(pane),
            );
        } else {
            self.limit_alerts.remove(&pane);
            self.limit_events.remove(&pane);
            self.screen_limits.remove(&pane);
            let mut original = switch.original;
            original.account_undo = None;
            if let Some(p) = self.panes.get_mut(&pane) {
                p.spec.account_undo = (!switch.undo).then(|| Box::new(original));
            }
            let message = format!("Using {}", switch.to.name);
            if switch.undo {
                self.notify(NoticeKind::Notice, &message, "Account change undone", Some(pane));
            } else {
                self.notify_action(NoticeKind::Notice, &message, pane, super::attention::ToastAction::UndoAccount);
            }
        }
        self.refresh_switch_views(pane);
        self.mark_persist();
        self.mark_dirty();
    }

    /// Thresholds: alert, offer or switch when an agent's account is nearly used up.
    pub(super) fn check_limits(&mut self) {
        let now = accounts::now();
        let mode = self.accounts.limits.mode;
        let ids: Vec<PaneId> = self.panes.keys().copied().collect();
        let panes = &self.panes;
        self.screen_limits.retain(|p, _| panes.contains_key(p));
        self.switches.retain(|p, _| panes.contains_key(p));
        for id in ids {
            let Some(a) = self.pane_account(id) else { continue };
            // Limit messages on screen are the freshest signal there is (one saved before a
            // restart is not).
            let screen_limit = self
                .panes
                .get(&id)
                .filter(|p| p.exit_code.is_none())
                .and_then(|p| accounts::limit_from_screen(&p.live_text()));
            // A message counts once, when it appears or changes: one left on screen neither
            // renews a window that has reset nor holds a lower figure seen since.
            let fresh = match screen_limit {
                Some(seen) => self.screen_limits.insert(id, seen) != Some(seen),
                None => {
                    self.screen_limits.remove(&id);
                    false
                }
            };
            if let (true, Some((weekly, pct))) = (fresh, screen_limit) {
                let mut u = self.usage.get(&a).cloned().unwrap_or_default();
                u.observe_screen(weekly, pct, now);
                let _ = accounts::save_usage(&a, &u);
                self.usage.0.insert(a.key(), u);
                self.await_usage_reset();
            }
            if self.switches.contains_key(&id) {
                continue;
            }
            let Some(peak) = self.usage.peak(&a, now) else { continue };
            let threshold = self.accounts.threshold(&a) as f64;
            if peak < threshold {
                self.limit_alerts.remove(&id);
                self.limit_events.remove(&id);
                continue;
            }
            let Some(p) = self.panes.get(&id) else { continue };
            let idle = matches!(p.agent.state, AgentState::Done | AgentState::Idle) || p.exit_code.is_some();
            // Restarting an agent mid-turn loses its work: switch by itself only when the agent's
            // own hooks said it finished, never on a guess from the screen; and never when its
            // command must be shown before it runs again (it may send a prompt again).
            let auto_ok = (p.agent.hooked || p.exit_code.is_some()) && self.restarts_by_itself(id);
            let mode = if mode == LimitMode::Auto && !auto_ok { LimitMode::Ask } else { mode };
            let ws_name = self.workspace_of(id).map(|(wi, _)| self.workspaces[wi].name.clone());
            let cwd = p.current_cwd();
            // Which accounts a rule allows here waits for git (asked now): the next check decides.
            if let Some(dir) = cwd.clone().filter(|d| self.accounts.undecided(&a.agent, Some(d))) {
                self.ask_git_about(dir);
                continue;
            }
            let alt = self.accounts.alternative(&a, ws_name.as_deref(), cwd.as_deref(), &self.usage, now);
            let reset = accounts::describe_reset(self.usage.get(&a).and_then(|u| u.next_reset(now)), now);
            let what = format!(
                "{} is at {peak:.0}%{}",
                accounts::label(&a),
                if reset.is_empty() { String::new() } else { format!(" ({reset})") }
            );
            let already = self.limit_alerts.get(&id).cloned();
            if self.limit_events.insert(id) {
                let fields = serde_json::json!({
                    "agent": a.agent,
                    "account": a.name,
                    "account_id": a.id,
                    "percent": peak.round(),
                    "threshold": threshold,
                    "alternative": alt.as_ref().map(crate::accounts::label),
                });
                self.emit_pane("account.limit", id, fields);
            }
            match (mode, &alt) {
                (LimitMode::Auto, Some(alt)) if idle && already != Some(LimitAlert::Attempted) => {
                    self.limit_alerts.insert(id, LimitAlert::Attempted);
                    if let Err(e) = self.switch_account(id, alt, None, false) {
                        self.notify(NoticeKind::Error, &what, &format!("could not switch: {e:#}"), Some(id));
                    }
                }
                (LimitMode::Ask, Some(alt))
                    if idle && !matches!(already, Some(LimitAlert::Asked | LimitAlert::Attempted)) =>
                {
                    let client =
                        self.primary.filter(|c| self.clients.get(c).is_some_and(|c| matches!(c.mode, Mode::Normal)));
                    if let Some(c) = client {
                        self.limit_alerts.insert(id, LimitAlert::Asked);
                        if let Err(e) = self.switch_account(id, alt, Some(c), true) {
                            self.notify(NoticeKind::Error, &what, &format!("could not switch: {e:#}"), Some(id));
                        }
                    } else if already.is_none() {
                        self.notify(NoticeKind::Warning, &what, "open kraal to continue on another account", Some(id));
                        self.limit_alerts.insert(id, LimitAlert::Notified);
                    }
                }
                // In ask and auto modes, wait for the end of the turn rather than interrupting.
                (LimitMode::Ask | LimitMode::Auto, Some(_)) => {}
                _ if already.is_none() => {
                    self.notify_action(
                        NoticeKind::Warning,
                        &format!("{} is at {peak:.0}% used", a.name),
                        id,
                        super::attention::ToastAction::ChangeAccount,
                    );
                    self.limit_alerts.insert(id, LimitAlert::Notified);
                }
                _ => {}
            }
        }
    }

    pub(super) fn agent_picker(&mut self, client: ClientId, switch_for: Option<PaneId>) -> Picker {
        self.agent_picker_on_path(client, switch_for, &std::env::var_os("PATH").unwrap_or_default())
    }

    fn agent_picker_on_path(&mut self, client: ClientId, switch_for: Option<PaneId>, path: &std::ffi::OsStr) -> Picker {
        let cwd = self
            .clients
            .get(&client)
            .and_then(|c| self.agent_cwd(c.workspace, self.client_focus(client), None))
            .unwrap_or_else(|| dirs::home_dir().unwrap_or_else(|| "/".into()));
        let now = accounts::now();
        let current = switch_for.and_then(|p| self.pane_account(p));
        let mut items = Vec::new();
        for registered in crate::agent_registry::AGENTS {
            let Some(kind) = registered.accounts.as_ref() else {
                if switch_for.is_none() && registered.command_on_path(path, &cwd).is_some() {
                    items.push(PickItem::new(
                        registered.label,
                        registered.account_message(),
                        PickAction::AgentLaunch { agent: registered.name.into(), account: None },
                    ));
                }
                continue;
            };
            if current.as_ref().is_some_and(|c| c.agent != kind.name) {
                continue;
            }
            let installed = registered.command_on_path(path, &cwd).is_some();
            for a in self.accounts.for_agent(kind.name) {
                if switch_for.is_none() && (!installed || a.sign_in_required) {
                    continue;
                }
                let mut detail = super::account_ui::usage_summary(&a, self.usage.get(&a), now);
                let is_current = current.as_ref().is_some_and(|c| c.id == a.id || c.same_home(&a));
                if is_current {
                    detail = format!("current · {detail}");
                }
                let full = self.usage.peak(&a, now).is_some_and(|p| p >= self.accounts.threshold(&a) as f64);
                if let Some(pane) = switch_for {
                    detail = if is_current {
                        "current · Continue conversation".into()
                    } else {
                        self.check_switch_row(client, pane, &a);
                        if a.sign_in_required { "Sign-in required" } else { "Could not verify · checking sign-in" }
                            .into()
                    };
                }
                let action = match switch_for {
                    Some(pane) => {
                        PickAction::AgentSwitch { pane, agent: a.agent.clone(), account: a.key(), current: is_current }
                    }
                    None => PickAction::AgentLaunch { agent: a.agent.clone(), account: Some(a.key()) },
                };
                items.push(PickItem {
                    label: if switch_for.is_some() { accounts::label(&a) } else { super::account_ui::label(&a) },
                    detail,
                    action,
                    state: full.then_some(AgentState::Blocked),
                    dim: false,
                });
            }
        }
        let title = if switch_for.is_some() { "Change account" } else { "Start an agent" };
        let mut picker = Picker::new(PickerKind::Agent, title, items);
        if current.as_ref().is_some_and(|a| a.agent == "codex") {
            picker.message = Some("Undo returns to this conversation.".into());
            picker.message_kind = NoticeKind::Notice;
        }
        picker.worktree = self.client_focus(client).and_then(|p| self.pane_git(p)).is_some()
            && self.client_focus(client).and_then(|p| self.panes.get(&p)).is_some_and(|p| p.ssh.is_none());
        picker
    }

    fn check_switch_row(&mut self, client: ClientId, pane: PaneId, to: &Account) {
        let Some(from) = self.pane_account(pane) else { return };
        let session = self.observed_agent(pane).ok().flatten().map(|a| a.session).or_else(|| {
            self.panes.get(&pane).and_then(|p| p.agent.session_id.clone().or_else(|| p.spec.session.clone()))
        });
        let to = to.clone();
        let checked = to.clone();
        self.background(
            move || {
                let status = account_status::check(&checked);
                let carry = status == Status::SignedIn
                    && session.as_deref().is_some_and(|id| accounts::check_carry(&from, &checked, id).is_ok());
                switch_effect(&checked.agent, status, carry)
            },
            move |server, effect| {
                if !server.accounts.by_id(&to.id).is_some_and(|a| a.same_home(&to)) {
                    return;
                }
                if let Some(c) = server.clients.get_mut(&client)
                    && let Mode::Picker(picker) = &mut c.mode
                {
                    for item in &mut picker.items {
                        if matches!(&item.action, PickAction::AgentSwitch { pane: p, agent, account, current: false }
                            if *p == pane && agent == &to.agent && account == &to.key())
                        {
                            item.detail = effect.into();
                        }
                    }
                }
                server.mark_dirty();
            },
        );
    }

    /// Create an account and open its login in a floating pane.
    pub(super) fn add_account(&mut self, client: ClientId, agent: &str, name: &str) -> Result<PaneId> {
        let kind = accounts::agent_kind(agent).context("Unknown agent")?;
        anyhow::ensure!(
            crate::transfer::have_program(kind.command),
            "{} is not installed. Install {}, then try again.",
            super::account_ui::agent_name(agent),
            kind.command
        );
        let home = accounts::default_home_for(agent, name);
        let a = Account {
            id: crate::accounts::new_uuid(),
            name: name.into(),
            agent: agent.into(),
            home: home.display().to_string(),
            threshold: None,
            note: None,
            sign_in_required: true,
            login_revision: String::new(),
            review: false,
            public_identity: None,
            linked_to: None,
            references: Vec::new(),
        };
        let a = self.update_accounts(|all| {
            let a = all.add(a)?;
            accounts::prepare_home(&a)?;
            Ok(a)
        })?;
        self.reload_accounts();
        self.open_login(client, &a)
            .context("Account saved. Press Esc to return to Accounts, then choose Sign in to retry")
    }

    pub(super) fn open_login(&mut self, client: ClientId, a: &Account) -> Result<PaneId> {
        if let Some(id) =
            self.panes.values().find_map(|p| p.spec.login.as_ref().filter(|l| l.account.same_home(a)).map(|_| p.id))
        {
            self.reveal(client, id);
            return Ok(id);
        }
        let l = accounts::login(a)?;
        let ws = self.clients.get(&client).map(|c| c.workspace).context("no client")?;
        let lock = accounts::lock_login(a)?;
        self.update_accounts(|all| {
            all.set_sign_in_required(a, true);
            Ok(())
        })?;
        self.invalidate_account_identities(&a.agent);
        let spec = PaneSpec {
            command: l.argv,
            env: l.env,
            env_remove: l.env_remove,
            login: Some(Login { account: a.clone(), state: LoginState::Running, lock: Some(lock) }),
            ..Default::default()
        };
        let result =
            self.spawn_pane_in(ws, None, spec, Placement::Floating, Some(format!("login {}", accounts::label(a))));
        self.reload_accounts();
        result
    }

    pub(super) fn login_exited(&mut self, id: PaneId, code: Option<i32>) {
        let Some(pane) = self.panes.get_mut(&id) else { return };
        let Some(login) = &mut pane.spec.login else { return };
        pane.exit_code = Some(code);
        let was_running = login.state == LoginState::Running;
        login.state = login.state.exited(code);
        if was_running && login.state == LoginState::Checking {
            if self.check_login(id).is_err() {
                self.login_checked(id, Status::CouldNotCheck);
            }
        } else if login.state != LoginState::Checking {
            login.lock = None;
        }
        self.mark_dirty();
    }

    pub(super) fn check_login(&mut self, id: PaneId) -> Result<()> {
        let Some(login) = self.panes.get_mut(&id).and_then(|p| p.spec.login.as_mut()) else { return Ok(()) };
        if login.lock.is_none() {
            login.lock = Some(accounts::lock_login(&login.account)?);
        }
        login.state = LoginState::Checking;
        let account = login.account.clone();
        self.invalidate_account_identities(&account.agent);
        if !self.background(
            move || account_status::check(&account),
            move |server, status| {
                server.login_checked(id, status);
            },
        ) {
            self.login_checked(id, Status::CouldNotCheck);
        }
        self.mark_dirty();
        Ok(())
    }

    fn login_checked(&mut self, id: PaneId, status: Status) {
        let Some(login) = self.panes.get_mut(&id).and_then(|p| p.spec.login.as_mut()) else { return };
        // Closing or cancelling a check must not publish its late result.
        if login.state != LoginState::Checking {
            return;
        }
        login.state = LoginState::Checked(status);
        let account = login.account.clone();
        self.invalidate_account_identities(&account.agent);
        if status == Status::SignedIn {
            if self
                .update_accounts(|all| {
                    all.set_sign_in_required(&account, false);
                    Ok(())
                })
                .is_err()
            {
                let login = self.panes.get_mut(&id).unwrap().spec.login.as_mut().unwrap();
                login.state = LoginState::SaveFailed;
                login.lock = None;
            } else {
                self.reload_accounts();
                self.added_login_finished(id, &account);
                self.close_pane(id);
                let message = format!("{} is signed in", accounts::label(&account));
                for client in self.clients.keys().copied().collect::<Vec<_>>() {
                    self.toast(client, NoticeKind::Notice, &message);
                }
            }
        } else {
            self.panes.get_mut(&id).unwrap().spec.login.as_mut().unwrap().lock = None;
        }
        self.mark_dirty();
    }

    pub(super) fn retry_login(&mut self, id: PaneId) -> Result<()> {
        let Some(login) = self.panes.get(&id).and_then(|p| p.spec.login.as_ref()) else { return Ok(()) };
        let account = login.account.clone();
        let lock = accounts::lock_login(&account)?;
        self.update_accounts(|all| {
            all.set_sign_in_required(&account, true);
            Ok(())
        })?;
        self.invalidate_account_identities(&account.agent);
        let Some(pane) = self.panes.get_mut(&id) else { return Ok(()) };
        let Some(login) = &mut pane.spec.login else { return Ok(()) };
        login.lock = Some(lock);
        login.state = LoginState::Running;
        if pane.respawn(&self.env, &self.tx).is_err() {
            let login = pane.spec.login.as_mut().unwrap();
            login.state = login_start_failed(&pane.spec.command);
            login.lock = None;
        }
        self.reload_accounts();
        self.mark_dirty();
        self.relayout();
        Ok(())
    }

    /// Ask every Codex account's app-server for its limits, in the background.
    pub(super) fn poll_codex_usage(&mut self) {
        self.last_codex_poll = Instant::now();
        let list: Vec<Account> = self.accounts.for_agent("codex");
        if list.is_empty() || !crate::transfer::have_program("codex") {
            return;
        }
        let tx = self.tx.clone();
        let polling = crate::threads::named("codex-usage").spawn(move || {
            for a in list {
                let home = crate::paths::expand_tilde(&a.home);
                if !home.is_dir() {
                    continue;
                }
                let limits = crate::codex_usage::fetch("codex", &home, Duration::from_secs(20))
                    .ok()
                    .or_else(|| crate::codex_usage::from_rollouts(&home));
                if let Some(l) = limits {
                    let u = accounts::Usage::from(l);
                    let _ = accounts::save_usage(&a, &u);
                }
            }
            let _ = tx.wake();
        });
        if let Err(e) = polling {
            self.warnings.push(format!("cannot ask Codex for its limits: {e}"));
        }
    }
}

pub(super) fn login_start_failed(command: &[String]) -> LoginState {
    if command.first().is_some_and(|c| crate::transfer::have_program(c)) {
        LoginState::Failed
    } else {
        LoginState::Checked(Status::NotInstalled)
    }
}

/// "5h ▓▓▓▓░░░░ 48% · 7d ▓▓▓▓▓▓░░ 71% · resets in 1h20"
pub fn usage_text(u: &accounts::Usage, now: i64) -> String {
    let mut parts = Vec::new();
    for (name, pct) in ["5h", "7d"].into_iter().zip(u.percentages(now)) {
        if let Some(pct) = pct {
            parts.push(format!("{name} {} {pct:.0}%", crate::theme::current().bar(pct)));
        }
    }
    let reset = accounts::describe_reset(u.next_reset(now), now);
    if !reset.is_empty() {
        parts.push(reset);
    }
    if parts.is_empty() { "no usage seen yet".into() } else { parts.join(" · ") }
}

#[cfg(test)]
mod tests;
