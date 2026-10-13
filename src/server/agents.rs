//! The agent manager in the server: launching agents on accounts, the agent picker, usage
//! thresholds, and continuing a conversation on another account.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use super::ui::{ConfirmAction, Mode, PickAction, PickItem, Picker, PickerKind};
use super::{ClientId, Server};
use crate::account_status::{self, Status};
use crate::accounts::{self, Account, Accounts, Mode as LimitMode, UsageMap};
use crate::agent::AgentState;
use crate::layout::PaneId;
use crate::pane::{Login, LoginState, PaneSpec};
use crate::protocol::Placement;

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
    stopping: Option<SwitchStop>,
    pub stage: SwitchStage,
}

/// Keep the terminal's session and the agent's children identifiable after its shell exits.
struct SwitchStop {
    process: crate::pane::ProcessHold,
    members: Vec<(u32, u64)>,
    since: Instant,
}

impl SwitchStop {
    fn new(pane: &crate::pane::Pane) -> Option<Self> {
        let mut stop = Self { process: pane.process_hold()?, members: Vec::new(), since: Instant::now() };
        stop.refresh();
        Some(stop)
    }

    fn refresh(&mut self) {
        self.members.retain(|&(pid, started)| crate::process::still_running(pid, started));
        let mut pids = self.process.session_members();
        pids.extend(self.members.iter().map(|&(pid, _)| pid));
        // Remember children before signalling their parents, including ones in another session.
        let roots = pids.clone();
        for pid in roots {
            pids.extend(crate::process::descendants(pid));
        }
        pids.sort_unstable();
        pids.dedup();
        for pid in pids {
            if !self.members.iter().any(|&(seen, _)| seen == pid)
                && let Some(started) = crate::process::start_time(pid)
            {
                self.members.push((pid, started));
            }
        }
    }

    fn signal(&self, signal: i32) {
        self.process.signal(signal);
        for &(pid, started) in &self.members {
            if crate::process::still_running(pid, started) {
                // SAFETY: this member of our stopped job still has the identity we retained.
                unsafe { libc::kill(pid as libc::pid_t, signal) };
            }
        }
    }

    fn finished(&mut self) -> bool {
        self.refresh();
        if self.members.iter().all(|&(pid, started)| !crate::process::still_running(pid, started)) {
            return true;
        }
        let elapsed = self.since.elapsed();
        if elapsed >= Duration::from_secs(2) {
            self.signal(libc::SIGKILL);
        } else if elapsed >= Duration::from_secs(1) {
            self.signal(libc::SIGTERM);
        }
        false
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct ObservedAgent {
    pid: u32,
    started: u64,
    command: Vec<String>,
    account: Account,
    session: String,
    cwd: PathBuf,
    path: String,
}

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
    accounts::agent_kind(agent).is_some_and(|k| k.shared.contains(&"projects"))
}

/// Why a pane waits for Enter before its agent starts again.
pub(super) const AMBIGUOUS: &str =
    "kraal cannot tell this command's options from a prompt it would send again: check it, Enter runs it as shown";

impl Server {
    pub(super) fn reload_accounts(&mut self) {
        match Accounts::load() {
            Ok(a) => {
                self.follow_renamed_accounts();
                self.accounts = a;
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
        for a in self.accounts.unlisted() {
            if self.existing_checks.contains_key(&a.agent) {
                continue;
            }
            self.existing_checks.insert(a.agent.clone(), ExistingCheck::Checking);
            let checked = a.clone();
            let agent = a.agent.clone();
            let started = self.background(
                move || {
                    let result = account_status::inspect(&checked);
                    let saved = if result.status == Status::SignedIn {
                        Accounts::update(|all| all.adopt_checked(&checked, result.identity.as_deref()))
                    } else {
                        Ok(())
                    };
                    (result.status, saved)
                },
                move |server, (status, saved)| {
                    server.existing_checks.insert(a.agent.clone(), ExistingCheck::Checked(status));
                    match saved {
                        Ok(()) => {
                            server.reload_accounts();
                            server.reload_usage();
                        }
                        Err(e) => {
                            server.existing_checks.insert(a.agent, ExistingCheck::SaveFailed);
                            server
                                .warnings
                                .push(format!("Could not adopt the existing account: {e:#}. Open Check again."));
                        }
                    }
                    server.refresh_agent_pickers();
                    server.mark_dirty();
                },
            );
            if !started {
                self.existing_checks.insert(agent, ExistingCheck::Checked(Status::CouldNotCheck));
            }
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
                ExistingCheck::Checking | ExistingCheck::Checked(Status::CouldNotCheck) | ExistingCheck::SaveFailed
            )
        });
        self.check_existing_accounts();
    }

    pub(super) fn retry_existing_account(&mut self, agent: &str) {
        if self.existing_checks.get(agent).is_some_and(|state| *state != ExistingCheck::Checking) {
            self.existing_checks.remove(agent);
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
                    PickAction::UseExisting { existing: existing.clone(), target: Some(a) },
                )
            })
            .collect();
        items.push(PickItem::new(
            "A different account",
            "",
            PickAction::UseExisting { existing: existing.clone(), target: None },
        ));
        Picker::new(PickerKind::AccountReview, &accounts::existing_label(&existing.agent), items)
    }

    pub(super) fn confirm_existing_account(
        &mut self,
        client: ClientId,
        existing: &Account,
        target: Option<&Account>,
    ) -> Result<()> {
        Accounts::update(|all| all.review(existing, target))?;
        self.reload_accounts();
        self.reload_usage();
        self.refresh_agent_pickers();
        self.mark_dirty();
        self.toast(client, "Account updated");
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
        for ws in &mut self.unrestored {
            renamed |= ws.bind_accounts(&self.accounts);
        }
        if renamed {
            self.mark_dirty();
            self.mark_persist();
        }
    }

    /// Rename account `old` of `agent` to `new` ([`accounts::rename`]).
    pub(super) fn rename_account(&mut self, agent: &str, old: &str, new: &str) -> Result<()> {
        accounts::rename(agent, old, new)?;
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

    fn observed_agent(&self, pane: PaneId) -> Option<ObservedAgent> {
        let p = self.panes.get(&pane)?;
        if p.exit_code.is_some() || p.ssh.is_some() || p.spec.login.is_some() {
            return None;
        }
        let kind = accounts::agent_kind(p.agent.kind.as_deref()?)?;
        let pid = p.foreground_pid()?;
        let started = crate::process::start_time(pid)?;
        let mut names = vec!["HOME", "PATH", kind.home_env];
        names.extend(kind.clear_env);
        let (argv, env) = crate::process::command_env(pid, &names)?;
        let mut command = observed_command(&argv, kind.name)?;
        // A shell's other jobs and opaque wrappers must not be stopped with its foreground job.
        if !crate::process::sole_foreground_job(p.pid()?, pid)
            || !p.spec.command.is_empty() && observed_command(&p.spec.command, kind.name).is_none()
            || pid != p.pid()? && crate::process::ppid(pid) != p.pid()
        {
            return None;
        }
        let account = self.accounts.process_account(kind, &env)?;
        let session = p
            .agent
            .session_process
            .filter(|process| *process == (pid, started))
            .and(p.agent.session_id.clone())
            .or_else(|| {
                let selector = if kind.name == "claude" { &["--resume", "--session-id"][..] } else { &["resume"][..] };
                selector.contains(&command.get(1)?.as_str()).then(|| command.get(2).cloned()).flatten()
            })?;
        let cwd = crate::process::cwd(pid)?;
        let program = std::path::Path::new(&command[0]);
        let executable = |path: &std::path::Path| {
            use std::os::unix::fs::PermissionsExt;
            path.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        };
        let program = if program.components().count() > 1 {
            cwd.join(program)
        } else {
            std::env::split_paths(env.get("PATH")?).map(|dir| cwd.join(dir).join(program)).find(|p| executable(p))?
        };
        if !executable(&program) {
            return None;
        }
        command[0] = program.to_str()?.into();
        if !crate::process::still_running(pid, started) {
            return None;
        }
        Some(ObservedAgent { pid, started, command, account, session, cwd, path: env.get("PATH")?.clone() })
    }

    pub(super) fn refresh_observed_accounts(&mut self) {
        let observed = self
            .panes
            .keys()
            .filter(|id| {
                let spec = &self.panes[id].spec;
                spec.account.is_none() && spec.account_id.is_none()
            })
            .filter_map(|&id| self.observed_agent(id).map(|a| (id, a)))
            .collect();
        if self.observed_agents != observed {
            self.observed_agents = observed;
            self.mark_dirty();
        }
    }

    pub(super) fn change_account_mode(&mut self, client: ClientId, pane: PaneId) -> Mode {
        if self.switches.contains_key(&pane) {
            return self.pending_switch_mode(pane);
        }
        let managed = self.panes.get(&pane).is_some_and(|p| p.spec.account.is_some() || p.spec.account_id.is_some());
        if !managed {
            self.observed_agents.remove(&pane);
            if let Some(a) =
                self.observed_agent(pane).filter(|a| accounts::check_resume(&a.account, &a.session).is_ok())
            {
                self.observed_agents.insert(pane, a);
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
        let kind = accounts::AGENTS.iter().find(|k| k.command == program)?;
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
        accounts::agent_kind(agent)
            .with_context(|| format!("unknown agent {agent:?} (known: {})", accounts::agent_names()))?;
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
            Some(self.observed_agent(pane).context("Can't switch this process. Open with another account.")?)
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
            self.toast(client, format!("Checking sign-in for {}", accounts::label(to)));
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
        let unsupported =
            switch.observed.as_ref().is_some_and(|before| self.observed_agent(pane).as_ref() != Some(before));
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
            self.notify("Could not change account", &format!("{reason} Open Change account to try again."), Some(pane));
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
                self.observed_agent(pane).as_ref() == Some(observed),
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
                        self.notify("Stop and switch?", "Open Change account to confirm or cancel", Some(pane));
                    }
                } else {
                    self.switches.remove(&pane);
                    self.notify("Account change needs confirmation", "Open Change account to try again", Some(pane));
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
        switch.stopping = SwitchStop::new(p);
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
        self.notify("Could not change account", &format!("{message} {reason}"), Some(pane));
        self.refresh_switch_views(pane);
        self.mark_persist();
    }

    fn switch_started(&mut self, pane: PaneId) {
        let Some(switch) = self.switches.remove(&pane) else { return };
        if switch.stage == SwitchStage::Restoring {
            self.notify(
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
                self.notify(&message, "Account change undone", Some(pane));
            } else {
                self.notify_action(&message, pane, super::attention::ToastAction::UndoAccount);
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
                        self.notify(&what, &format!("could not switch: {e:#}"), Some(id));
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
                            self.notify(&what, &format!("could not switch: {e:#}"), Some(id));
                        }
                    } else if already.is_none() {
                        self.notify(&what, "open kraal to continue on another account", Some(id));
                        self.limit_alerts.insert(id, LimitAlert::Notified);
                    }
                }
                // In ask and auto modes, wait for the end of the turn rather than interrupting.
                (LimitMode::Ask | LimitMode::Auto, Some(_)) => {}
                _ if already.is_none() => {
                    self.notify_action(
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
        let now = accounts::now();
        let current = switch_for.and_then(|p| self.pane_account(p));
        let mut items = Vec::new();
        for kind in accounts::AGENTS {
            if current.as_ref().is_some_and(|c| c.agent != kind.name) {
                continue;
            }
            let installed = crate::transfer::have_program(kind.command);
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
                    None => PickAction::AgentLaunch { agent: a.agent.clone(), account: a.key() },
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
        }
        picker.worktree = self.client_focus(client).and_then(|p| self.pane_git(p)).is_some()
            && self.client_focus(client).and_then(|p| self.panes.get(&p)).is_some_and(|p| p.ssh.is_none());
        picker
    }

    fn check_switch_row(&mut self, client: ClientId, pane: PaneId, to: &Account) {
        let Some(from) = self.pane_account(pane) else { return };
        let session = self.observed_agent(pane).map(|a| a.session).or_else(|| {
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
            review: false,
            public_identity: None,
            linked_to: None,
            references: Vec::new(),
        };
        let a = Accounts::update(|all| {
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
        Accounts::update(|all| {
            all.set_sign_in_required(a, true);
            Ok(())
        })?;
        self.reload_accounts();
        let spec = PaneSpec {
            command: l.argv,
            env: l.env,
            env_remove: l.env_remove,
            login: Some(Login { account: a.clone(), state: LoginState::Running, lock: Some(lock) }),
            ..Default::default()
        };
        self.spawn_pane_in(ws, None, spec, Placement::Floating, Some(format!("login {}", accounts::label(a))))
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
        if status == Status::SignedIn {
            let account = login.account.clone();
            if Accounts::update(|all| {
                all.set_sign_in_required(&account, false);
                Ok(())
            })
            .is_err()
            {
                login.state = LoginState::SaveFailed;
                login.lock = None;
            } else {
                self.reload_accounts();
                self.added_login_finished(id, &account);
                self.close_pane(id);
                let message = format!("{} is signed in", accounts::label(&account));
                for client in self.clients.keys().copied().collect::<Vec<_>>() {
                    self.toast(client, &message);
                }
            }
        } else {
            login.lock = None;
        }
        self.mark_dirty();
    }

    pub(super) fn retry_login(&mut self, id: PaneId) -> Result<()> {
        let Some(login) = self.panes.get(&id).and_then(|p| p.spec.login.as_ref()) else { return Ok(()) };
        let account = login.account.clone();
        let lock = accounts::lock_login(&account)?;
        Accounts::update(|all| {
            all.set_sign_in_required(&account, true);
            Ok(())
        })?;
        self.reload_accounts();
        let Some(pane) = self.panes.get_mut(&id) else { return Ok(()) };
        let Some(login) = &mut pane.spec.login else { return Ok(()) };
        login.lock = Some(lock);
        login.state = LoginState::Running;
        if pane.respawn(&self.env, &self.tx).is_err() {
            let login = pane.spec.login.as_mut().unwrap();
            login.state = login_start_failed(&pane.spec.command);
            login.lock = None;
        }
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
                    let conv = |w: Option<crate::codex_usage::Window>| {
                        w.map(|w| accounts::Window { used_percent: w.used_percent, resets_at: w.resets_at })
                    };
                    let u = accounts::Usage {
                        five_hour: conv(l.five_hour),
                        weekly: conv(l.weekly),
                        observed_at: l.observed_at,
                        source: "codex".into(),
                    };
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
    for (name, w) in [("5h", &u.five_hour), ("7d", &u.weekly)] {
        if let Some(w) = w {
            let pct = if w.resets_at.is_some_and(|r| r <= now) { 0.0 } else { w.used_percent };
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
mod tests {
    use super::*;

    #[test]
    fn finishing_discovery_keeps_the_clicked_account_in_place() {
        for (cols, rows) in [(80, 24), (124, 36)] {
            for finished in [&["claude"][..], &["codex"], &["claude", "codex"], &["codex", "claude"]] {
                let (tx, _) = super::super::queue::channel(16);
                let mut server = super::super::tests::server(tx);
                let (client, _peer) = super::super::tests::client(&mut server);
                let c = server.clients.get_mut(&client).unwrap();
                c.cols = cols;
                c.rows = rows;
                let [_, second] = ["work", "second"].map(|name| {
                    server
                        .accounts
                        .add(Account {
                            id: accounts::new_uuid(),
                            agent: "codex".into(),
                            name: name.into(),
                            home: format!("/nonexistent/{name}"),
                            threshold: None,
                            note: None,
                            sign_in_required: false,
                            review: false,
                            public_identity: None,
                            linked_to: None,
                            references: Vec::new(),
                        })
                        .unwrap()
                });
                for agent in ["claude", "codex"] {
                    server.existing_checks.insert(agent.into(), ExistingCheck::Checking);
                }
                let picker = server.accounts_picker(None);
                let layout = super::super::overlays::picker_layout(&picker, server.client_screen(client));
                let mut buffer = ratatui::buffer::Buffer::empty(server.client_screen(client));
                super::super::overlays::draw_picker(&picker, buffer.area, &mut buffer);
                let (col, row) = (layout.list.x + 2, layout.list.y + 2);
                let label: String = (col..col + 14).map(|x| buffer[(x, row)].symbol()).collect();
                assert_eq!(label, "Codex · second");
                server.clients.get_mut(&client).unwrap().mode = Mode::Picker(picker);
                // The user clicks the row they saw while the background checks finish.
                for &agent in finished {
                    server.existing_checks.insert(agent.into(), ExistingCheck::Checked(Status::NotSignedIn));
                    server.refresh_agent_pickers();
                }
                server.client_input(client, format!("\x1b[<0;{};{}M", col + 1, row + 1).as_bytes());
                let Mode::AccountDetails(details) = &server.clients[&client].mode else {
                    panic!("the click did not open details");
                };
                assert_eq!(details.account, second.id, "{cols}x{rows}: the clicked row changed accounts");
                server.client_input(client, b"\t\r");
                let Mode::Prompt(prompt) = &server.clients[&client].mode else {
                    panic!("Rename did not open");
                };
                assert_eq!(prompt.title, "Rename Codex · second");
            }
        }
    }

    #[test]
    fn account_discovery_starts_each_check_once_and_handles_results_without_a_tick() {
        use super::super::{Event, queue::Wait};

        for reverse in [false, true] {
            let (tx, rx) = super::super::queue::channel(16);
            let mut server = super::super::tests::server(tx);
            server.check_existing_accounts();
            assert_eq!(server.jobs.len(), accounts::AGENTS.len());
            assert!(server.existing_checks.values().all(|s| *s == ExistingCheck::Checking));
            server.refresh_existing_accounts();
            assert_eq!(server.jobs.len(), accounts::AGENTS.len(), "opening Accounts shares the pending checks");

            // The isolated PATH has neither agent. Keep their completed jobs, not a status
            // subprocess racing its watchdog, and deliver them in each order.
            let mut results = Vec::new();
            for _ in accounts::AGENTS {
                let Wait::Event(Event::Job(id, result)) = rx.wait(crate::testing::PATIENCE) else {
                    panic!("the check did not publish its result");
                };
                results.push((id, result));
            }
            results.sort_by_key(|(id, _)| *id);
            if reverse {
                results.reverse();
            }
            let (client, _peer) = super::super::tests::client(&mut server);
            server.clients.get_mut(&client).unwrap().mode = Mode::Picker(server.accounts_picker(None));
            let last_tick = server.last_tick;
            for (i, (id, result)) in results.into_iter().enumerate() {
                server.handle(Event::Job(id, result));
                assert_eq!(server.jobs.len(), accounts::AGENTS.len() - i - 1);
                assert_eq!(
                    server
                        .existing_checks
                        .values()
                        .filter(|s| **s == ExistingCheck::Checked(Status::NotInstalled))
                        .count(),
                    i + 1,
                    "each result is published while the other is still pending"
                );
                let Mode::Picker(picker) = &server.clients[&client].mode else { panic!("Accounts stays open") };
                for (agent, state) in &server.existing_checks {
                    let item = picker.items.iter().find(|item| item.label == accounts::existing_label(agent));
                    if *state == ExistingCheck::Checking {
                        assert_eq!(item.unwrap().detail, "Checking sign-in");
                    } else {
                        assert!(item.is_none(), "a completed missing agent no longer shows as checking");
                    }
                }
                if i == 0 {
                    server.handle(Event::Warning("server-responsive".into()));
                    assert_eq!(server.warnings, ["server-responsive"]);
                }
            }
            assert!(server.jobs.is_empty());
            assert!(server.existing_checks.values().all(|s| *s == ExistingCheck::Checked(Status::NotInstalled)));
            assert_eq!(server.last_tick, last_tick);
            assert!(server.panes.is_empty(), "discovery never starts a login");
        }
    }

    #[test]
    fn a_verified_account_opens_review_without_rechecking_or_waiting_for_another_agent() {
        let mut t = SwitchTest::new("review-checked");
        let original = t.server.accounts.get("claude", "default").unwrap();
        t.server.accounts.adopt_checked(&original, None).unwrap();
        t.server.existing_checks.insert("codex".into(), ExistingCheck::Checking);
        for _ in 0..3 {
            t.server.refresh_existing_accounts();
            let picker = t.server.accounts_picker(None);
            let item = picker.items.iter().find(|i| matches!(i.action, PickAction::ReviewAccount(_))).unwrap();
            assert_eq!(item.detail, "Review");
            let Mode::Picker(review) = t.server.pick(99, &picker, item.clone()) else { panic!("review") };
            assert!(review.kind == PickerKind::AccountReview);
            assert!(t.server.jobs.is_empty(), "a verified home needs no new status command to open Review");
        }
    }

    #[test]
    fn review_questions_and_confirmations_fit_both_terminal_sizes() {
        use ratatui::{buffer::Buffer, layout::Rect};
        let mut t = SwitchTest::new("review-ui");
        t.server.accounts.rename("claude", "from", "patron.bzh").unwrap();
        t.server.accounts.rename("claude", "to", "gmail").unwrap();
        let original = t.server.accounts.get("claude", "default").unwrap();
        t.server.accounts.adopt_checked(&original, None).unwrap();
        let existing = t.server.accounts.reviews().pop().unwrap();
        for (cols, rows) in [(124, 36), (80, 24)] {
            let screen = Rect::new(0, 0, cols, rows);
            let picker = t.server.review_account_picker(&existing);
            assert_eq!(picker.action_keys(), &[("enter", "Choose")]);
            let mut buf = Buffer::empty(screen);
            super::super::overlays::draw_picker(&picker, screen, &mut buf);
            let text: String = buf.content.iter().map(|c| c.symbol()).collect();
            for label in [
                "Existing Claude account",
                "Which account is this?",
                "patron.bzh",
                "gmail",
                "A different account",
                "Back",
            ] {
                assert!(text.contains(label), "{cols}x{rows}: {label}");
            }
            let mode = t.server.pick(99, &picker, picker.items[0].clone());
            let Mode::Confirm(question) = mode else { panic!("choosing only opens confirmation") };
            assert!(!question.yes);
            assert_eq!(question.action.verb(), "Use account");
            super::super::overlays::draw_confirm(&question, screen, &mut buf);
            let text: String = buf.content.iter().map(|c| c.symbol()).collect();
            for label in [
                "Use patron.bzh?",
                "Your running agents will keep working.",
                "This account will appear once.",
                "Cancel",
                "Use account",
            ] {
                assert!(text.contains(label), "{cols}x{rows}: {label}");
            }
            assert_eq!(t.server.accounts.reviews().len(), 1, "choosing does not commit the review");
        }
    }

    #[test]
    fn a_linked_live_pane_keeps_its_home_and_selecting_its_identity_does_not_switch() {
        let mut t = SwitchTest::new("review-pane");
        t.server.switches.clear();
        let old = t.server.accounts.get("claude", "default").unwrap();
        t.server.accounts.adopt_checked(&old, None).unwrap();
        let pending = t.server.accounts.reviews().pop().unwrap();
        let target = t.server.accounts.get("claude", "to").unwrap();
        let spec = &mut t.server.panes.get_mut(&1).unwrap().spec;
        spec.account = Some("claude:default".into());
        spec.account_id = Some(old.id.clone());
        spec.env = accounts::launch(&old, &[], None, None).unwrap().env;
        let command = spec.command.clone();
        t.server.accounts.review(&pending, Some(&target)).unwrap();
        t.server.follow_renamed_accounts();
        assert_eq!(t.server.pane_account(1).unwrap().home, old.home);
        assert_eq!(t.server.pane_account(1).unwrap().id, target.id);
        assert_eq!(t.server.panes[&1].spec.command, command);
        assert!(t.server.panes[&1].exit_code.is_none());
        t.server.switch_account(1, &target, None, false).unwrap();
        assert!(t.server.switches.is_empty());
        assert_eq!(t.server.panes[&1].agent.state, AgentState::Working);
    }

    #[test]
    fn unrestored_accounts_follow_renames_but_never_replacements() {
        let mut t = SwitchTest::new("unrestored");
        let old = t.server.accounts.get("claude", "from").unwrap();
        let pane = serde_json::json!({ "name": "agent", "agent": "claude", "agent_session": null,
            "spec": { "account": "claude:from", "env": [["CLAUDE_CONFIG_DIR", old.home]] } });
        let ws = serde_json::json!({ "name": "failed", "definition": null, "cwd": null, "active": 0,
            "tabs": [{ "name": "", "root": { "Split": { "axis": "Row", "children": [[{ "Pane": pane }, 1.0]] } },
                "floating": [[pane, { "x": 0.1, "y": 0.1, "w": 0.5, "h": 0.5 }]],
                "focus": null, "show_floating": true, "zoomed": false, "sync": false }] });
        let ws: crate::persist::WorkspaceSnap = serde_json::from_value(ws).unwrap();
        t.server.env.shell = t.dir.join("missing-shell").display().to_string();
        crate::persist::apply(
            &mut t.server,
            crate::persist::SessionSnapshot {
                version: 1,
                revision: 1,
                workspaces: vec![ws.clone()],
                tunnels: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(t.server.unrestored.len(), 1, "the missing shell prevents restoration");
        assert_eq!(
            t.server.unrestored[0].tabs[0].floating[0].0.spec.account_id.as_deref(),
            Some(old.id.as_str()),
            "even panes not reached by restore must already be bound"
        );
        // A legacy workspace retained before a reload is bound too.
        t.server.unrestored.push(ws);
        t.server.follow_renamed_accounts();
        t.server.accounts.remove("claude", "from");
        t.server.accounts.add(Account { name: "renamed".into(), ..old.clone() }).unwrap();
        t.server.follow_renamed_accounts();
        assert!(t.server.persist_dirty);
        let check = |server: &Server| {
            for ws in &server.unrestored {
                let crate::persist::NodeSnap::Split { children, .. } = ws.tabs[0].root.as_ref().unwrap() else {
                    panic!("split")
                };
                let crate::persist::NodeSnap::Pane(tiled) = &children[0].0 else { panic!("pane") };
                for pane in [tiled.as_ref(), &ws.tabs[0].floating[0].0] {
                    assert_eq!(pane.spec.account_id.as_deref(), Some(old.id.as_str()));
                    assert_eq!(pane.spec.account.as_deref(), Some("claude:renamed"));
                    assert!(pane.spec.env.contains(&("KRAAL_ACCOUNT_ID".into(), old.id.clone())));
                }
            }
        };
        check(&t.server);
        t.server.accounts.remove("claude", "renamed");
        t.server.accounts.add(Account { id: accounts::new_uuid(), name: "renamed".into(), ..old.clone() }).unwrap();
        t.server.follow_renamed_accounts();
        check(&t.server);
        assert!(t.server.accounts.by_id(&old.id).is_none());
    }

    struct SwitchTest {
        server: Server,
        dir: PathBuf,
        rx: super::super::queue::EventReceiver,
    }

    impl SwitchTest {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("kraal-switch-{name}-{}", std::process::id()));
            std::fs::create_dir_all(dir.join("from/projects/work")).unwrap();
            std::fs::create_dir_all(dir.join("to/projects/work")).unwrap();
            std::fs::write(dir.join("from/projects/work/session.jsonl"), "{\"message\":1}\n").unwrap();
            let dir = dir.canonicalize().unwrap();
            let account = |name: &str| Account {
                id: accounts::new_uuid(),
                agent: "claude".into(),
                name: name.into(),
                home: dir.join(name).display().to_string(),
                threshold: None,
                note: None,
                sign_in_required: false,
                review: false,
                public_identity: None,
                linked_to: None,
                references: Vec::new(),
            };
            let (from, to) = (account("from"), account("to"));
            let (tx, rx) = super::super::queue::channel(64);
            let mut server = super::super::tests::server(tx);
            server.config.notifications.mode = crate::config::NotifyMode::Off;
            server.accounts.add(from.clone()).unwrap();
            server.accounts.add(to.clone()).unwrap();
            let spec = PaneSpec {
                command: vec!["claude".into()],
                account: Some("claude:from".into()),
                account_id: Some(from.id.clone()),
                session: Some("session".into()),
                text: Some(crate::pane::ShownText { title: String::new(), text: "still running".into() }),
                ..Default::default()
            };
            let mut pane = crate::pane::Pane::spawn(1, None, spec, 80, 24, &server.env, &server.tx).unwrap();
            pane.agent.hooked = true;
            pane.agent.state = AgentState::Working;
            server.panes.insert(1, pane);
            server.next_id = 10;
            server.switches.insert(
                1,
                AccountSwitch {
                    token: 2,
                    from,
                    to,
                    session: None,
                    process: None,
                    client: Some(99),
                    ask: false,
                    original: server.panes[&1].spec.clone(),
                    undo: false,
                    observed: None,
                    started: None,
                    stopping: None,
                    stage: SwitchStage::Checking,
                },
            );
            Self { server, dir, rx }
        }

        fn pump(&mut self) {
            while let Ok(event) = self.rx.try_recv() {
                self.server.handle(event);
            }
            while let Some(data) = self.server.panes.get(&1).and_then(|p| p.take_output()) {
                self.server.pane_output(1, &data);
            }
            self.server.finish_exits();
        }

        fn until(&mut self, what: &str, check: impl Fn(&Self) -> bool) {
            let start = Instant::now();
            loop {
                self.pump();
                if check(self) {
                    return;
                }
                assert!(start.elapsed() < crate::testing::PATIENCE, "{what}");
                std::thread::sleep(Duration::from_millis(5));
            }
        }

        fn spawn(&mut self, spec: PaneSpec) {
            let pane = crate::pane::Pane::spawn(1, None, spec, 80, 24, &self.server.env, &self.server.tx).unwrap();
            self.server.panes.insert(1, pane);
        }

        fn checked(&mut self, status: Status) {
            self.server.switch_checked(1, 2, status, false);
        }

        fn unchanged(&self) {
            let p = &self.server.panes[&1];
            assert_eq!(p.spec.account.as_deref(), Some("claude:from"));
            assert!(p.exit_code.is_none());
            assert_eq!(p.agent.state, AgentState::Working);
        }
    }

    impl Drop for SwitchTest {
        fn drop(&mut self) {
            for switch in self.server.switches.values() {
                if let Some(stop) = &switch.stopping {
                    stop.signal(libc::SIGKILL);
                }
            }
            for pane in self.server.panes.values() {
                if let Some(stop) = SwitchStop::new(pane) {
                    stop.signal(libc::SIGKILL);
                }
            }
            std::fs::remove_dir_all(&self.dir).unwrap();
        }
    }

    #[test]
    fn only_reliable_turn_boundaries_allow_an_unconfirmed_switch() {
        for state in [AgentState::Unknown, AgentState::Working, AgentState::Blocked, AgentState::Idle, AgentState::Done]
        {
            assert_eq!(switch_timing(false, state, false), SwitchStage::Confirm);
            assert_eq!(switch_timing(false, state, true), SwitchStage::Stopping);
        }
        for state in [AgentState::Working, AgentState::Blocked] {
            assert_eq!(switch_timing(true, state, false), SwitchStage::Waiting);
        }
        for state in [AgentState::Idle, AgentState::Done] {
            assert_eq!(switch_timing(true, state, false), SwitchStage::Stopping);
        }
        assert_eq!(switch_timing(true, AgentState::Unknown, false), SwitchStage::Confirm);
    }

    #[test]
    fn picker_effects_distinguish_sign_in_continuation_and_uncertainty() {
        for agent in ["claude", "codex"] {
            assert_eq!(switch_effect(agent, Status::NotSignedIn, true), "Sign-in required");
            for status in [Status::NotInstalled, Status::CouldNotCheck] {
                assert_eq!(switch_effect(agent, status, true), "Could not verify");
            }
        }
        assert_eq!(switch_effect("claude", Status::SignedIn, true), "Continue conversation");
        assert_eq!(switch_effect("claude", Status::SignedIn, false), "Could not verify");
        assert_eq!(switch_effect("codex", Status::SignedIn, false), "New conversation");
    }

    #[test]
    fn failed_verification_leaves_the_current_agent_untouched() {
        for (i, status) in [Status::NotSignedIn, Status::NotInstalled, Status::CouldNotCheck].into_iter().enumerate() {
            let mut t = SwitchTest::new(&format!("status-{i}"));
            t.checked(status);
            t.unchanged();
            assert!(t.server.switches.is_empty());
        }
    }

    #[test]
    fn missing_unreadable_and_divergent_transcripts_refuse_before_stopping() {
        use std::os::unix::fs::PermissionsExt;
        for reason in ["missing", "empty", "unreadable", "diverged", "unknown-id"] {
            let mut t = SwitchTest::new(reason);
            let src = t.dir.join("from/projects/work/session.jsonl");
            match reason {
                "missing" => std::fs::remove_file(&src).unwrap(),
                "empty" => std::fs::write(&src, "").unwrap(),
                "unreadable" => {
                    std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o000)).unwrap();
                    if std::fs::read(&src).is_ok() {
                        continue;
                    }
                }
                "diverged" => std::fs::write(t.dir.join("to/projects/work/session.jsonl"), "different\n").unwrap(),
                _ => t.server.panes.get_mut(&1).unwrap().agent.session_id = Some("../invalid".into()),
            }
            t.checked(Status::SignedIn);
            t.unchanged();
            assert!(t.server.switches.is_empty(), "{reason}");
        }
    }

    #[test]
    fn a_queued_switch_is_shared_cancellable_and_ignores_late_checks() {
        let mut t = SwitchTest::new("cancel");
        let to = t.server.switches[&1].to.clone();
        assert!(t.server.switch_account(1, &to, Some(99), false).unwrap_err().to_string().contains("already pending"));
        t.checked(Status::SignedIn);
        assert_eq!(t.server.switches[&1].stage, SwitchStage::Waiting);
        t.unchanged();
        let Mode::Picker(p) = t.server.pending_switch_mode(1) else { panic!("cancellable") };
        assert!(p.items[0].detail.contains("Switch after this turn"));
        assert!(matches!(p.items[0].action, PickAction::CancelSwitch { pane: 1, token: 2 }));
        t.server.cancel_switch(1, 2);
        t.checked(Status::SignedIn);
        assert!(t.server.switches.is_empty());
        t.unchanged();
    }

    #[test]
    fn queued_changes_recheck_the_current_conversation_and_account() {
        for changed in ["transcript", "session", "target", "replaced"] {
            let mut t = SwitchTest::new(&format!("changed-{changed}"));
            t.checked(Status::SignedIn);
            match changed {
                "transcript" => std::fs::remove_file(t.dir.join("from/projects/work/session.jsonl")).unwrap(),
                "session" => t.server.panes.get_mut(&1).unwrap().agent.session_id = Some("new-session".into()),
                _ => {
                    let old = t.server.accounts.get("claude", "to").unwrap();
                    t.server.accounts.remove("claude", "to");
                    if changed == "replaced" {
                        t.server.accounts.add(Account { id: accounts::new_uuid(), ..old }).unwrap();
                    }
                }
            }
            t.server.switches.get_mut(&1).unwrap().stage = SwitchStage::Checking;
            t.checked(Status::SignedIn);
            t.unchanged();
            assert!(t.server.switches.is_empty(), "{changed}");
        }
    }

    #[test]
    fn an_automatic_change_rechecks_headroom_before_stopping() {
        for reason in ["unknown", "full", "sign-in", "command"] {
            let mut t = SwitchTest::new(&format!("auto-headroom-{reason}"));
            t.server.switches.get_mut(&1).unwrap().client = None;
            if reason != "unknown" {
                t.server.usage.0.insert(
                    t.server.accounts.get("claude", "to").unwrap().key(),
                    accounts::Usage {
                        five_hour: Some(accounts::Window {
                            used_percent: if reason == "full" { 100.0 } else { 0.0 },
                            resets_at: Some(accounts::now() + 3600),
                        }),
                        ..Default::default()
                    },
                );
            }
            if reason == "sign-in" {
                t.server.accounts.set_sign_in_required(&t.server.switches[&1].to, true);
            }
            if reason == "command" {
                t.server.panes.get_mut(&1).unwrap().spec.command = vec!["claude".into(), "a prompt".into()];
            }
            t.checked(Status::SignedIn);
            assert!(t.server.switches.is_empty(), "{reason}");
            t.unchanged();
        }
    }

    #[test]
    fn ask_and_unreliable_turns_require_one_confirmation_before_stopping() {
        for ask in [false, true] {
            let mut t = SwitchTest::new(if ask { "ask" } else { "unreliable" });
            t.server.switches.get_mut(&1).unwrap().ask = ask;
            let p = t.server.panes.get_mut(&1).unwrap();
            p.agent.hooked = ask;
            p.agent.state = AgentState::Idle;
            t.checked(Status::SignedIn);
            assert_eq!(t.server.switches[&1].stage, SwitchStage::Confirm);
            let Mode::Confirm(q) = t.server.pending_switch_mode(1) else { panic!("confirmation") };
            assert!(!q.yes);
            t.server.prepare_switch(1, true).unwrap();
            assert_eq!(t.server.switches[&1].stage, SwitchStage::Stopping);
            t.server.cancel_switch(1, 2);
            assert_eq!(t.server.switches[&1].stage, SwitchStage::Stopping);
        }
    }

    #[test]
    fn a_late_check_does_not_touch_a_closed_pane_or_a_replacement_change() {
        let mut t = SwitchTest::new("late-check");
        t.server.switches.get_mut(&1).unwrap().token = 3;
        t.checked(Status::SignedIn);
        assert_eq!(t.server.switches[&1].stage, SwitchStage::Checking);
        t.server.panes.remove(&1);
        t.server.switch_checked(1, 3, Status::SignedIn, false);
        assert!(t.server.switches.is_empty());
    }

    #[test]
    fn another_name_for_the_current_home_is_also_a_no_op() {
        let mut t = SwitchTest::new("same-home");
        let mut alias = t.server.switches.remove(&1).unwrap().from;
        alias.name = "alias".into();
        t.server.switch_account(1, &alias, Some(99), false).unwrap();
        assert!(t.server.switches.is_empty());
        assert!(t.server.jobs.is_empty());
        t.unchanged();
    }

    #[test]
    fn stop_confirmation_defaults_to_cancel_and_codex_names_the_new_conversation() {
        let mut t = SwitchTest::new("confirm");
        let switch = t.server.switches.get_mut(&1).unwrap();
        switch.stage = SwitchStage::Confirm;
        switch.to.agent = "codex".into();
        let Mode::Confirm(q) = t.server.pending_switch_mode(1) else { panic!("confirmation") };
        assert!(!q.yes);
        assert!(q.message.starts_with("Stop and switch?"));
        assert!(q.message.contains("New conversation"));
        t.server.confirm_switch(1, 2, 99, false);
        assert!(t.server.switches.is_empty());
        t.unchanged();
    }

    #[test]
    fn an_automatic_failure_is_remembered_until_usage_falls_below_the_threshold() {
        let mut t = SwitchTest::new("auto-failure");
        t.server.accounts.limits.mode = LimitMode::Auto;
        let usage = |percent| accounts::Usage {
            five_hour: Some(accounts::Window { used_percent: percent, resets_at: Some(accounts::now() + 3600) }),
            ..Default::default()
        };
        t.server.usage.0.insert(t.server.accounts.get("claude", "from").unwrap().key(), usage(95.0));
        t.server.usage.0.insert(t.server.accounts.get("claude", "to").unwrap().key(), usage(0.0));
        t.server.limit_alerts.insert(1, LimitAlert::Attempted);
        t.checked(Status::NotSignedIn);
        t.server.panes.get_mut(&1).unwrap().agent.state = AgentState::Done;
        for _ in 0..10 {
            t.server.check_limits();
        }
        assert!(t.server.switches.is_empty());
        assert!(t.server.jobs.is_empty());
        assert_eq!(t.server.limit_alerts.get(&1), Some(&LimitAlert::Attempted));
        t.server.usage.0.insert(t.server.accounts.get("claude", "from").unwrap().key(), usage(10.0));
        t.server.check_limits();
        assert!(!t.server.limit_alerts.contains_key(&1));
    }
    #[test]
    fn observed_commands_require_exact_interactive_arguments() {
        let words = |s: &str| s.split_whitespace().map(str::to_string).collect::<Vec<_>>();
        for command in [
            "claude --model opus",
            "/usr/bin/node /opt/claude --resume session --model opus",
            "sh /bin/codex resume original -m model",
        ] {
            let kind = if command.contains("codex") { "codex" } else { "claude" };
            assert!(observed_command(&words(command), kind).is_some(), "{command}");
        }
        for command in [
            "sh -c claude",
            "wrapper claude",
            "node --eval claude",
            "claude --new-flag value",
            "claude -p prompt",
            "claude --settings other",
            "claude --resume session --settings other",
            "claude --continue",
            "claude --fork-session",
        ] {
            assert!(observed_command(&words(command), "claude").is_none(), "{command}");
        }
        for command in ["codex exec prompt", "codex -c auth=other", "codex --profile other", "codex resume --last"] {
            assert!(observed_command(&words(command), "codex").is_none(), "{command}");
        }
    }

    #[test]
    fn undo_is_saved_only_with_a_started_change_and_survives_resuming_the_pane() {
        let mut t = SwitchTest::new("undo-saved");
        t.server.limit_alerts.insert(1, LimitAlert::Notified);
        t.server.switches.get_mut(&1).unwrap().stage = SwitchStage::Stopping;
        t.server.switches.get_mut(&1).unwrap().session = Some("session".into());
        t.server.finish_switch(1);
        assert_eq!(t.server.switches[&1].stage, SwitchStage::Starting);
        assert!(t.server.messages.is_empty(), "preparing a command is not success");
        t.server.switch_started(1);
        assert!(t.server.switches.is_empty());
        assert!(!t.server.limit_alerts.contains_key(&1), "the new account has its own threshold crossing");
        let spec = &t.server.panes[&1].spec;
        let saved = spec.account_undo.as_ref().unwrap();
        assert_eq!(saved.session.as_deref(), Some("session"));
        assert_eq!(saved.account.as_deref(), Some("claude:from"));
        assert!(saved.account_undo.is_none());
        let json = serde_json::to_string(spec).unwrap();
        let restored: PaneSpec = serde_json::from_str(&json).unwrap();
        let (resumed, _) = t.server.resume_spec(&restored, Some("session"));
        assert_eq!(resumed.account_undo.unwrap().account_id, saved.account_id);
        assert!(
            serde_json::from_str::<PaneSpec>(r#"{"command":["claude"],"account":"claude:from"}"#)
                .unwrap()
                .account_undo
                .is_none()
        );
    }

    #[test]
    fn claude_undo_carries_the_latest_transcript_and_refuses_divergence() {
        for diverged in [false, true] {
            let mut t = SwitchTest::new(if diverged { "undo-diverged" } else { "undo-latest" });
            t.server.switches.get_mut(&1).unwrap().stage = SwitchStage::Stopping;
            t.server.switches.get_mut(&1).unwrap().session = Some("session".into());
            t.server.finish_switch(1);
            t.server.switch_started(1);
            let original = t.server.panes[&1].spec.clone();
            let from = t.server.accounts.get("claude", "to").unwrap();
            let to = t.server.accounts.get("claude", "from").unwrap();
            std::fs::write(t.dir.join("to/projects/work/session.jsonl"), "{\"message\":1}\n{\"message\":2}\n").unwrap();
            if diverged {
                std::fs::write(t.dir.join("from/projects/work/session.jsonl"), "different\n").unwrap();
            }
            t.server.switches.insert(
                1,
                AccountSwitch {
                    token: 3,
                    from,
                    to,
                    original,
                    session: None,
                    process: None,
                    client: Some(99),
                    ask: false,
                    undo: true,
                    observed: None,
                    started: None,
                    stopping: None,
                    stage: SwitchStage::Checking,
                },
            );
            let result = t.server.prepare_switch(1, true);
            if diverged {
                assert!(result.is_err());
                assert_eq!(t.server.panes[&1].spec.account.as_deref(), Some("claude:to"));
            } else {
                result.unwrap();
                t.server.finish_switch(1);
                t.server.switch_started(1);
                assert_eq!(t.server.panes[&1].spec.account.as_deref(), Some("claude:from"));
                assert!(t.server.panes[&1].spec.account_undo.is_none());
                assert_eq!(
                    std::fs::read(t.dir.join("from/projects/work/session.jsonl")).unwrap(),
                    std::fs::read(t.dir.join("to/projects/work/session.jsonl")).unwrap()
                );
            }
        }
    }

    #[test]
    fn failed_start_restores_the_source_and_a_failed_recovery_keeps_one_retry() {
        let mut t = SwitchTest::new("restore-source");
        t.server.switches.get_mut(&1).unwrap().stage = SwitchStage::Starting;
        let target = t.server.switches[&1].to.clone();
        t.server.panes.get_mut(&1).unwrap().spec = t.server.agent_spec(&target, None, Some("session"), &[]).unwrap();
        t.server.switch_start_failed(1, "target exited");
        assert_eq!(t.server.switches[&1].stage, SwitchStage::Restoring);
        assert_eq!(t.server.panes[&1].spec.account.as_deref(), Some("claude:from"));
        assert!(t.server.panes[&1].spec.command.windows(2).any(|w| w == ["--resume", "session"]));
        assert!(t.server.panes[&1].spec.account_undo.is_none());
        t.server.switch_start_failed(1, "source exited too");
        assert!(t.server.switches.is_empty());
        assert!(t.server.panes[&1].exit_code.is_some());
        assert_eq!(t.server.panes[&1].spec.account.as_deref(), Some("claude:from"));
        t.server.check_switches();
        assert!(t.server.switches.is_empty(), "no retry loop");
    }

    #[test]
    fn an_unassociated_process_offers_a_new_pane_without_changing_the_old_one() {
        let mut t = SwitchTest::new("opaque");
        t.server.switches.clear();
        t.server.panes.get_mut(&1).unwrap().spec.account = None;
        t.server.panes.get_mut(&1).unwrap().spec.account_id = None;
        let Mode::Picker(p) = t.server.change_account_mode(99, 1) else { panic!("a way out") };
        assert_eq!(p.title, "Can't switch this process.");
        assert!(matches!(p.items[0].action, PickAction::OpenAnotherAccount(1)));
        assert_eq!(p.items[0].label, "Open with another account");
        assert_eq!(t.server.panes[&1].agent.state, AgentState::Working);
        assert!(t.server.switches.is_empty());
    }

    #[test]
    fn notify_threshold_has_a_change_action_without_stopping_or_requiring_an_alternative() {
        let mut t = SwitchTest::new("notify-action");
        t.server.switches.clear();
        let (client, _peer) = super::super::tests::client(&mut t.server);
        let a = t.server.accounts.get("claude", "from").unwrap();
        t.server.usage.0.insert(
            a.key(),
            accounts::Usage {
                five_hour: Some(accounts::Window { used_percent: 94.6, resets_at: Some(accounts::now() + 3600) }),
                ..Default::default()
            },
        );
        t.server.check_limits();
        let c = &t.server.clients[&client];
        assert_eq!(c.toast.as_ref().unwrap().0, "from is at 95% used · Change account");
        assert_eq!(c.toast_action, Some((1, super::super::attention::ToastAction::ChangeAccount)));
        t.unchanged();
        assert!(t.server.switches.is_empty());
        t.server.toast(client, "Dismissed");
        t.server.check_limits();
        assert_eq!(t.server.clients[&client].toast.as_ref().unwrap().0, "Dismissed", "one notification per crossing");
    }
    #[test]
    fn codex_undo_resumes_the_original_id_and_keeps_both_histories() {
        for completed in [true, false] {
            let mut t = SwitchTest::new(if completed { "codex-return" } else { "codex-unused-return" });
            let s = t.server.switches.get_mut(&1).unwrap();
            s.from.agent = "codex".into();
            s.to.agent = "codex".into();
            let (from, to) = (s.from.clone(), s.to.clone());
            t.server.accounts = Accounts::empty();
            t.server.accounts.add(from.clone()).unwrap();
            t.server.accounts.add(to.clone()).unwrap();
            for (home, session) in [("from", "original"), ("to", "new")] {
                let dir = t.dir.join(home).join("sessions/2026/10/13");
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(
                    dir.join(format!("rollout-today-{session}.jsonl")),
                    format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{session}\"}}}}\n"),
                )
                .unwrap();
            }
            let spec = t.server.agent_spec(&from, None, Some("original"), &[]).unwrap();
            t.server.panes.get_mut(&1).unwrap().spec = spec.clone();
            let s = t.server.switches.get_mut(&1).unwrap();
            s.original = spec;
            s.session = Some("original".into());
            s.stage = SwitchStage::Stopping;
            t.server.finish_switch(1);
            assert_eq!(t.server.panes[&1].agent.session_id, None, "new Codex never inherits the original id");
            t.server.switch_started(1);
            if completed {
                t.server.panes.get_mut(&1).unwrap().agent.session_id = Some("new".into());
            }
            let original = t.server.panes[&1].spec.clone();
            t.server.switches.insert(
                1,
                AccountSwitch {
                    token: 3,
                    from: to.clone(),
                    to: from.clone(),
                    original,
                    session: None,
                    process: None,
                    client: Some(99),
                    ask: false,
                    undo: true,
                    observed: None,
                    started: None,
                    stopping: None,
                    stage: SwitchStage::Checking,
                },
            );
            t.server.prepare_switch(1, true).unwrap();
            t.server.finish_switch(1);
            assert!(t.server.panes[&1].spec.command.windows(2).any(|w| w == ["resume", "original"]));
            t.server.switch_started(1);
            accounts::check_resume(&from, "original").unwrap();
            accounts::check_resume(&to, "new").unwrap();
            assert!(t.server.panes[&1].spec.account_undo.is_none());
        }
    }

    #[test]
    fn a_failed_undo_sign_in_keeps_the_return_action_and_running_account() {
        let mut t = SwitchTest::new("undo-sign-in");
        t.server.switches.get_mut(&1).unwrap().undo = true;
        t.server.panes.get_mut(&1).unwrap().spec.account_undo = Some(Box::new(PaneSpec {
            account: Some("claude:to".into()),
            session: Some("original".into()),
            ..Default::default()
        }));
        t.checked(Status::NotSignedIn);
        t.unchanged();
        assert!(t.server.panes[&1].spec.account_undo.is_some());
        assert!(t.server.switches.is_empty());
    }

    #[test]
    fn a_live_detected_process_is_rechecked_before_any_stop() {
        let mut t = SwitchTest::new("observed-process");
        let program = t.dir.join("claude");
        crate::testing::write_script(&program, "#!/bin/sh\nwhile IFS= read -r line; do :; done\n");
        let from = t.server.switches[&1].from.clone();
        let spec = PaneSpec {
            command: vec![program.display().to_string(), "--session-id".into(), "session".into()],
            env: vec![("CLAUDE_CONFIG_DIR".into(), from.home.clone())],
            cwd: Some(t.dir.clone()),
            ..Default::default()
        };
        let mut pane = crate::pane::Pane::spawn(1, None, spec, 80, 24, &t.server.env, &t.server.tx).unwrap();
        pane.agent.kind = Some("claude".into());
        t.server.panes.insert(1, pane);
        let started = Instant::now();
        let observed = loop {
            if let Some(a) = t.server.observed_agent(1) {
                break a;
            }
            assert!(started.elapsed() < crate::testing::PATIENCE);
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(observed.account.id, from.id);
        assert_eq!(observed.session, "session");
        let hold = t.server.panes[&1].process_hold();
        let s = t.server.switches.get_mut(&1).unwrap();
        s.process = hold;
        s.observed = Some(ObservedAgent { started: observed.started.wrapping_add(1), ..observed });
        let result = t.server.prepare_switch(1, true);
        assert!(result.unwrap_err().to_string().contains("Can't switch this process."));
        let pid = t.server.panes[&1].pid().unwrap();
        assert!(crate::process::running(pid as libc::pid_t));
        t.server.panes.get_mut(&1).unwrap().kill();
    }

    #[test]
    fn a_shell_switch_waits_for_the_agent_and_its_children_and_keeps_the_agents_directory() {
        let mut t = SwitchTest::new("shell-survivor");
        let program = t.dir.join("claude");
        crate::testing::write_script(
            &program,
            r#"#!/bin/sh
test "$*" = 'auth status' && exit 0
root="$(dirname "$CLAUDE_CONFIG_DIR")"
if test "$CLAUDE_CONFIG_DIR" = "$root/from"; then
    trap '' HUP TERM
    sh -c 'trap "" HUP TERM
        echo $$ > "$1/child"
        while test ! -f "$1/release-child"; do sleep 0.02; done
        printf "{\"message\":2}\n" >> "$2"' sh "$root" "$CLAUDE_CONFIG_DIR/projects/work/session.jsonl" &
    echo $$ > "$root/agent"
    while test ! -f "$root/release-agent"; do sleep 0.02; done
    exit 0
fi
pwd > "$root/replacement-cwd"
echo READY
while IFS= read -r line; do :; done
"#,
        );
        let project = t.dir.join("project-B");
        std::fs::create_dir(&project).unwrap();
        let (from, to) = (t.server.switches[&1].from.clone(), t.server.switches[&1].to.clone());
        t.server.switches.clear();
        t.server.env.shell = "bash".into();
        t.spawn(PaneSpec {
            cwd: Some(t.dir.clone()),
            env: vec![
                ("CLAUDE_CONFIG_DIR".into(), from.home.clone()),
                ("PROJECT_B".into(), project.display().to_string()),
                ("FAKE_AGENT".into(), program.display().to_string()),
            ],
            ..Default::default()
        });
        let pane = t.server.panes.get_mut(&1).unwrap();
        pane.agent.kind = Some("claude".into());
        pane.write_input(b"(cd \"$PROJECT_B\"; exec \"$FAKE_AGENT\" --resume session)\n".to_vec());
        t.until("the shell's foreground agent is observed", |t| {
            t.dir.join("child").exists() && t.server.observed_agent(1).is_some()
        });
        let observed = t.server.observed_agent(1).unwrap();
        assert_eq!(observed.cwd, project);
        assert_eq!(t.server.panes[&1].current_cwd(), Some(t.dir.clone()));
        t.server.begin_switch(1, &to, Some(99), false, false).unwrap();
        let token = t.server.switches[&1].token;
        t.server.switch_checked(1, token, Status::SignedIn, false);
        assert_eq!(t.server.switches[&1].stage, SwitchStage::Confirm);
        assert_eq!(t.server.switches[&1].original.cwd, Some(project.clone()));
        t.server.prepare_switch(1, true).unwrap();
        // Keep the survivors alive past the shell's output-drain deadline.
        t.server.switches.get_mut(&1).unwrap().stopping.as_mut().unwrap().since =
            Instant::now() + crate::testing::PATIENCE;
        t.until("the shell exits while its agent survives", |t| t.server.panes[&1].exit_code.is_some());
        assert!(crate::process::still_running(observed.pid, observed.started));
        let child: u32 = std::fs::read_to_string(t.dir.join("child")).unwrap().trim().parse().unwrap();
        let child_started = crate::process::start_time(child).unwrap();
        let copied = t.dir.join("to/projects/work/session.jsonl");
        t.server.check_switches();
        assert_eq!(t.server.switches[&1].stage, SwitchStage::Stopping);
        assert!(!copied.exists(), "no transfer while the foreground agent runs");
        assert!(!t.dir.join("replacement-cwd").exists());

        std::fs::write(t.dir.join("release-agent"), "").unwrap();
        t.until("the foreground agent exits", |_| !crate::process::still_running(observed.pid, observed.started));
        t.server.check_switches();
        assert!(crate::process::still_running(child, child_started));
        assert_eq!(t.server.switches[&1].stage, SwitchStage::Stopping);
        assert!(!copied.exists(), "the agent's orphaned child can still write the transcript");
        assert!(!t.dir.join("replacement-cwd").exists());

        std::fs::write(t.dir.join("release-child"), "").unwrap();
        t.until("the child finishes writing", |_| !crate::process::still_running(child, child_started));
        t.server.check_switches();
        t.until("the replacement runs", |t| t.dir.join("replacement-cwd").exists());
        assert_eq!(std::fs::read_to_string(copied).unwrap(), "{\"message\":1}\n{\"message\":2}\n");
        assert_eq!(t.server.panes[&1].spec.cwd, Some(project.clone()));
        assert_eq!(std::fs::read_to_string(t.dir.join("replacement-cwd")).unwrap().trim(), project.to_str().unwrap());
        // Recovery uses the same verified directory, even though the original pane was a shell.
        std::fs::remove_file(t.dir.join("release-agent")).unwrap();
        std::fs::remove_file(t.dir.join("release-child")).unwrap();
        t.server.panes.get_mut(&1).unwrap().kill();
        t.until("the source is restored", |t| t.server.switches[&1].stage == SwitchStage::Restoring);
        assert_eq!(t.server.panes[&1].spec.cwd, Some(project.clone()));
        let pid = t.server.panes[&1].pid().unwrap();
        t.until("recovery runs in the agent's directory", |_| crate::process::cwd(pid) == Some(project.clone()));
    }

    #[test]
    fn silent_replacement_and_recovery_that_ignore_hangup_are_stopped() {
        let mut t = SwitchTest::new("silent-hangup");
        let program = t.dir.join("claude");
        crate::testing::write_script(
            &program,
            "#!/bin/sh\ntrap '' HUP TERM\necho $$ > \"$CLAUDE_CONFIG_DIR/started\"\nwhile :; do sleep 1; done\n",
        );
        let original = PaneSpec {
            command: vec![program.display().to_string(), "--resume".into(), "session".into()],
            env: vec![("CLAUDE_CONFIG_DIR".into(), t.server.switches[&1].from.home.clone())],
            text: None,
            ..t.server.panes[&1].spec.clone()
        };
        t.spawn(original.clone());
        t.until("the source starts", |t| t.dir.join("from/started").exists());
        t.server.panes[&1].process_hold().unwrap().signal(libc::SIGKILL);
        t.until("the source stops", |t| t.server.panes[&1].exit_code.is_some());
        std::fs::remove_file(t.dir.join("from/started")).unwrap();
        t.server.switches.get_mut(&1).unwrap().original = original;
        t.server.switches.get_mut(&1).unwrap().stage = SwitchStage::Stopping;
        t.server.switches.get_mut(&1).unwrap().session = Some("session".into());
        t.server.finish_switch(1);

        for (home, starting, stopping) in [
            ("to", SwitchStage::Starting, SwitchStage::Recovering),
            ("from", SwitchStage::Restoring, SwitchStage::RecoveryStopping),
        ] {
            t.until("the silent agent starts", |t| t.dir.join(home).join("started").exists());
            assert_eq!(t.server.switches[&1].stage, starting);
            let pid = t.server.panes[&1].pid().unwrap();
            let started = crate::process::start_time(pid).unwrap();
            assert!(!t.server.panes[&1].has_printed());
            t.server.switches.get_mut(&1).unwrap().started = Some(Instant::now() - Duration::from_secs(11));
            t.server.check_switches();
            assert_eq!(t.server.switches[&1].stage, stopping);
            assert!(crate::process::still_running(pid, started), "the agent ignores hangup");
            t.server.switches.get_mut(&1).unwrap().stopping.as_mut().unwrap().since =
                Instant::now() - Duration::from_secs(3);
            t.server.check_switches();
            t.until("the watchdog kills the unresponsive agent", |_| !crate::process::still_running(pid, started));
            t.until("the stopped launch releases its reservation", |t| {
                t.server.switches.get(&1).is_none_or(|s| s.stage != stopping)
            });
        }
        assert!(t.server.switches.is_empty());
        assert!(t.server.panes[&1].exit_code.is_some());
        assert_eq!(t.server.panes[&1].spec.account.as_deref(), Some("claude:from"));
        t.server.check_switches();
        assert!(t.server.switches.is_empty(), "a failed recovery does not retry forever");
    }

    #[test]
    fn a_deferred_switch_launch_failure_recovers_without_an_exit_event() {
        let mut t = SwitchTest::new("deferred-switch-failure");
        let program = t.dir.join("claude");
        crate::testing::write_script(&program, "#!/bin/sh\necho READY\nread -r line\n");
        let mut original = t.server.panes[&1].spec.clone();
        original.command[0] = program.display().to_string();
        original.text = None;
        t.server.switches.get_mut(&1).unwrap().original = original;
        t.server.switches.get_mut(&1).unwrap().stage = SwitchStage::Starting;
        let missing_interpreter = t.dir.join("broken-claude");
        crate::testing::write_script(&missing_interpreter, "#!/nonexistent-interpreter\n");
        let batch = crate::pane::batch();
        t.spawn(PaneSpec { command: vec![missing_interpreter.display().to_string()], ..Default::default() });
        drop(batch);
        let pending = t.server.panes.get_mut(&1).unwrap().take_pending().unwrap();
        let crate::pane::Started::Failed(id, process, why) = pending.start() else { panic!("cannot start") };
        t.server.pane_failed(id, &process, &why);
        assert_eq!(t.server.switches[&1].stage, SwitchStage::Restoring);
        assert_eq!(t.server.panes[&1].spec.account.as_deref(), Some("claude:from"));
        t.until("the source starts without waiting for a nonexistent process to exit", |t| {
            t.server.panes[&1].has_printed()
        });
    }

    #[test]
    fn enter_after_a_synchronous_recovery_failure_resumes_the_source_conversation() {
        for undo in [false, true] {
            let mut t = SwitchTest::new(if undo { "undo-recovery-id" } else { "switch-recovery-id" });
            let switch = t.server.switches.get_mut(&1).unwrap();
            switch.from.agent = "codex".into();
            switch.to.agent = "codex".into();
            switch.undo = undo;
            let (from, to) = (switch.from.clone(), switch.to.clone());
            t.server.accounts = Accounts::empty();
            for (a, session) in [(&from, "source"), (&to, "destination")] {
                t.server.accounts.add(a.clone()).unwrap();
                let dir = PathBuf::from(&a.home).join("sessions/2026/10/13");
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(
                    dir.join(format!("rollout-today-{session}.jsonl")),
                    format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{session}\"}}}}\n"),
                )
                .unwrap();
            }
            let program = t.dir.join("codex");
            let script = "#!/bin/sh\necho \"$*\" > \"$CODEX_HOME/args\"\necho READY\nread -r line\nexit 23\n";
            crate::testing::write_script(&program, script);
            let mut original = t.server.agent_spec(&from, Some(t.dir.clone()), Some("source"), &[]).unwrap();
            original.command[0] = program.display().to_string();
            let mut destination = t.server.agent_spec(&to, Some(t.dir.clone()), Some("destination"), &[]).unwrap();
            destination.command[0] = program.display().to_string();
            t.spawn(PaneSpec { command: vec!["sh".into(), "-c".into(), "exit".into()], ..Default::default() });
            t.until("the initial process ends", |t| t.server.panes[&1].exit_code.is_some());
            let switch = t.server.switches.get_mut(&1).unwrap();
            switch.original = original;
            switch.stage = SwitchStage::Starting;
            t.server.start_switch_spec(1, destination).unwrap();
            assert_eq!(t.server.panes[&1].agent.session_id.as_deref(), Some("destination"));
            t.until("the destination runs", |t| t.dir.join("to/args").exists());
            std::fs::remove_file(&program).unwrap();
            t.server.panes.get_mut(&1).unwrap().write_input(b"exit\n".to_vec());
            t.until("recovery fails synchronously", |t| t.server.switches.is_empty());
            let pane = &t.server.panes[&1];
            assert!(pane.exit_code.is_some());
            assert_eq!(pane.spec.session.as_deref(), Some("source"));
            assert_eq!(pane.agent.session_id.as_deref(), Some("source"));
            assert_eq!(pane.agent.session_process, None);
            let (retry, exact) = t.server.restart_spec(1).unwrap();
            assert!(exact);
            assert!(retry.command.windows(2).any(|args| args == ["resume", "source"]));

            crate::testing::write_script(&program, script);
            let (client, _peer) = super::super::tests::client(&mut t.server);
            let tab = &mut t.server.workspaces.last_mut().unwrap().tabs[0];
            tab.root = Some(crate::layout::Node::Leaf(1));
            tab.set_focus(1);
            t.server.client_input(client, b"\r");
            t.until("Enter resumes the source", |t| t.dir.join("from/args").exists());
            assert_eq!(std::fs::read_to_string(t.dir.join("from/args")).unwrap().trim(), "resume source");
            assert_eq!(t.server.panes[&1].spec.account_id, Some(from.id));
        }
    }
    #[test]
    fn removing_the_destination_after_the_stop_resumes_the_source() {
        let mut t = SwitchTest::new("removed-after-stop");
        t.server.switches.get_mut(&1).unwrap().stage = SwitchStage::Stopping;
        t.server.accounts.remove("claude", "to");
        t.server.finish_switch(1);
        assert_eq!(t.server.switches[&1].stage, SwitchStage::Restoring);
        assert_eq!(t.server.panes[&1].spec.account.as_deref(), Some("claude:from"));
        assert!(t.server.panes[&1].spec.account_undo.is_none());
    }
    #[test]
    fn a_process_that_changes_during_verification_offers_another_pane() {
        let mut t = SwitchTest::new("process-changed-during-check");
        let (client, _peer) = super::super::tests::client(&mut t.server);
        let s = t.server.switches.get_mut(&1).unwrap();
        s.client = Some(client);
        s.observed = Some(ObservedAgent {
            pid: u32::MAX,
            started: 0,
            command: vec!["claude".into()],
            account: s.from.clone(),
            session: "session".into(),
            cwd: t.dir.clone(),
            path: String::new(),
        });
        t.checked(Status::SignedIn);
        let Mode::Picker(p) = &t.server.clients[&client].mode else { panic!("a safe next step") };
        assert_eq!(p.title, "Can't switch this process.");
        assert!(matches!(p.items[0].action, PickAction::OpenAnotherAccount(1)));
        assert!(t.server.switches.is_empty());
        t.unchanged();
    }
}
