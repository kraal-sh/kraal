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
    pub stage: SwitchStage,
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
                    Some((
                        id,
                        p.kind,
                        p.query.clone(),
                        p.cursor,
                        p.current().map(|i| i.label.clone()),
                        p.message.clone(),
                        p.min_items,
                    ))
                } else {
                    None
                }
            })
            .collect();
        for (id, kind, query, cursor, selected, message, min_items) in clients {
            let mut p =
                if kind == PickerKind::Accounts { self.accounts_picker(None) } else { self.agent_picker(id, None) };
            p.min_items = p.min_items.max(min_items);
            p.message = message;
            p.query = query;
            p.cursor = cursor;
            p.refilter();
            if let Some(label) = selected {
                p.selected = p.filtered.iter().position(|&i| p.items[i].label == label).unwrap_or(0);
            }
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

    /// The account of an agent pane kraal launched.
    pub fn pane_account(&self, pane: PaneId) -> Option<Account> {
        self.spec_account(&self.panes.get(&pane)?.spec)
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
                let mut fresh =
                    PaneSpec { worktree: spec.worktree.clone(), close_on_exit: spec.close_on_exit, ..fresh };
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
        let p = self.panes.get(&pane).context("no such pane")?;
        let from = self.pane_account(pane).context("this pane was not started from the agent manager")?;
        anyhow::ensure!(from.agent == to.agent, "{} cannot continue on a {} account", from.agent, to.agent);
        if from.id == to.id || from.same_home(to) {
            return Ok(());
        }
        anyhow::ensure!(!self.switches.contains_key(&pane), "an account change is already pending for this pane");
        let kind = accounts::agent_kind(&to.agent).context("unknown agent")?;
        accounts::resumable_args(kind, &accounts::launch_args(kind, &p.spec.command))
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
        let client = switch.client.or(self.primary);
        if let Some(c) = client.and_then(|id| self.clients.get_mut(&id)).filter(|c| matches!(c.mode, Mode::Normal)) {
            let action = if sign_in {
                ConfirmAction::SignInAccount(switch.to)
            } else {
                ConfirmAction::RetryAccountSwitch { pane, account: switch.to }
            };
            c.mode = Mode::confirm(format!("Could not change account\n{reason}"), action);
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
        let from = self.pane_account(pane).context("the pane's account was removed")?;
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
        let session = p.agent.session_id.clone().or_else(|| p.spec.session.clone());
        if conversations_move(&from.agent) {
            let id = session.as_deref().context("could not verify the Claude conversation id")?;
            accounts::check_carry(&from, &to, id)?;
        }
        let timing = switch_timing(p.agent.hooked, p.agent.state, p.exit_code.is_some());
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
            SwitchStage::Stopping if exited => self.finish_switch(pane),
            SwitchStage::Stopping => self.panes.get_mut(&pane).unwrap().kill(),
            SwitchStage::Checking => unreachable!(),
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
        if switch.stage == SwitchStage::Stopping {
            return Mode::Normal;
        }
        if switch.stage == SwitchStage::Confirm {
            let effect =
                if conversations_move(&switch.to.agent) { "Continue conversation" } else { "New conversation" };
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
            SwitchStage::Stopping | SwitchStage::Confirm => unreachable!(),
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
        if self.switches.get(&pane).is_some_and(|s| s.token == token && s.stage != SwitchStage::Stopping) {
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
    }

    /// The agent of a pane switching accounts has stopped (its pane shows it ended): carry its
    /// conversation over and start it again on the new account. When the conversation cannot
    /// follow, or the new agent cannot start, the pane stays as it is: Enter starts it again on
    /// its account, rather than start a new conversation. When its command must be shown first
    /// (a word kraal could not tell from a prompt), it waits for Enter on the new account.
    pub(super) fn finish_switch(&mut self, pane: PaneId) {
        if !self.switches.get(&pane).is_some_and(|s| s.stage == SwitchStage::Stopping) {
            return;
        }
        let Some(AccountSwitch { from, to, session, .. }) = self.switches.remove(&pane) else { return };
        self.refresh_switch_views(pane);
        if !self.panes.contains_key(&pane) {
            return;
        }
        let result = (|| -> Result<()> {
            let p = self.panes.get(&pane).context("no such pane")?;
            let kind = accounts::agent_kind(&to.agent).context("unknown agent")?;
            let resume = match &session {
                Some(id) => accounts::carry_conversation(&from, &to, id)
                    .with_context(|| format!("the conversation could not follow to {}", accounts::label(&to)))?
                    .then(|| id.clone()),
                None => None,
            };
            let cwd = p.current_cwd().or_else(|| p.spec.cwd.clone());
            let worktree = p.spec.worktree.clone();
            // The options carry over; a prompt given on the command line is not sent again.
            let (args, exact) = accounts::resumable_args(kind, &accounts::launch_args(kind, &p.spec.command))
                .context("a one-shot run (-p, exec) is not a conversation to continue")?;
            let mut spec = PaneSpec { worktree, ..self.agent_spec(&to, cwd, resume.as_deref(), &args)? };
            crate::worktree_setup::carry_env(&p.spec.env, &mut spec.env);
            if !exact {
                return self.suspend_pane(pane, spec, AMBIGUOUS);
            }
            let pn = self.panes.get_mut(&pane).context("no such pane")?;
            let old = std::mem::replace(&mut pn.spec, spec);
            pn.respawn(&self.env, &self.tx).inspect_err(|_| pn.spec = old)
        })();
        if let Err(e) = result {
            self.warnings.push(format!("{e:#}; the pane stays on {} (Enter starts it again)", accounts::label(&from)));
        }
        self.mark_persist();
        self.relayout();
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
                    // The key from the keymap in use (`ctrl+b A c` by default).
                    let key = self.keymap.hint(crate::keymap::Action::SwitchAccount);
                    let hint = match (&alt, key) {
                        (Some(alt), Some(key)) => format!("{key} to continue on {}", accounts::label(alt)),
                        (Some(alt), None) => format!("{} is free to continue on", accounts::label(alt)),
                        (None, _) => "no other account under its threshold".to_string(),
                    };
                    self.notify(&what, &hint, Some(id));
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
        picker.worktree = self.client_focus(client).and_then(|p| self.pane_git(p)).is_some()
            && self.client_focus(client).and_then(|p| self.panes.get(&p)).is_some_and(|p| p.ssh.is_none());
        picker
    }

    fn check_switch_row(&mut self, client: ClientId, pane: PaneId, to: &Account) {
        let Some(from) = self.pane_account(pane) else { return };
        let session = self.panes.get(&pane).and_then(|p| p.agent.session_id.clone().or_else(|| p.spec.session.clone()));
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
    }

    impl SwitchTest {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("kraal-switch-{name}-{}", std::process::id()));
            std::fs::create_dir_all(dir.join("from/projects/work")).unwrap();
            std::fs::create_dir_all(dir.join("to/projects/work")).unwrap();
            std::fs::write(dir.join("from/projects/work/session.jsonl"), "{\"message\":1}\n").unwrap();
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
            let (tx, _rx) = super::super::queue::channel(16);
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
                    stage: SwitchStage::Checking,
                },
            );
            Self { server, dir }
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
}
