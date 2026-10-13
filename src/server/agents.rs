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
                self.accounts = a;
                self.accounts_error = None;
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

    /// Panes started on an account renamed since go on under its new name.
    fn follow_renamed_accounts(&mut self) {
        let mut renamed = false;
        for pane in self.panes.values_mut() {
            let Some(key) = &pane.spec.account else { continue };
            let Some(a) = self.accounts.launched_on(key, &pane.spec.env) else { continue };
            let now = accounts::key(&a.agent, &a.name);
            if &now != key {
                pane.spec.account = Some(now);
                renamed = true;
            }
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
        self.usage = UsageMap::load(&self.accounts.all());
    }

    /// The account of an agent pane kraal launched.
    pub fn pane_account(&self, pane: PaneId) -> Option<Account> {
        let key = self.panes.get(&pane)?.spec.account.clone()?;
        let (agent, name) = key.split_once(':')?;
        self.accounts.get(agent, name)
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
        let Some(a) = spec.account.as_deref().and_then(|k| self.accounts.launched_on(k, &spec.env)) else {
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
            Some(name) => {
                self.accounts.get(agent, name).with_context(|| format!("{agent} has no account named {name:?}"))?
            }
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
        if from.same_home(to) {
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
        anyhow::ensure!(from.same_home(&switch.from), "the pane's account changed; choose an account again");
        let to = self.accounts.get(&switch.to.agent, &switch.to.name).context("the target account was removed")?;
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
                let _ = accounts::save_usage(&a.agent, &a.name, &u);
                self.usage.0.insert(accounts::key(&a.agent, &a.name), u);
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
        let ws_name = self
            .clients
            .get(&client)
            .and_then(|c| self.ws_index(c.workspace))
            .map(|wi| self.workspaces[wi].name.clone());
        let cwd = self.client_focus(client).and_then(|p| self.panes.get(&p)).and_then(|p| p.current_cwd());
        let current = switch_for.and_then(|p| self.pane_account(p));
        let mut items = Vec::new();
        for kind in accounts::AGENTS {
            if current.as_ref().is_some_and(|c| c.agent != kind.name) {
                continue;
            }
            // Not marked before git has said which repository rules see here.
            let preferred = (!self.accounts.undecided(kind.name, cwd.as_deref()))
                .then(|| self.accounts.choose(kind.name, ws_name.as_deref(), cwd.as_deref(), &self.usage, now))
                .flatten();
            for a in self.accounts.for_agent(kind.name) {
                let u = self.usage.get(&a);
                let mut detail = match u {
                    _ if a.sign_in_required => "sign-in required".to_string(),
                    Some(u) => usage_text(u, now),
                    None => "no usage seen yet".to_string(),
                };
                if preferred.as_ref().is_some_and(|p| p.name == a.name) {
                    detail.push_str(" · picked here");
                }
                let is_current = current.as_ref().is_some_and(|c| c.same_home(&a));
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
                    Some(pane) => PickAction::AgentSwitch {
                        pane,
                        agent: a.agent.clone(),
                        account: a.name.clone(),
                        current: is_current,
                    },
                    None => PickAction::AgentLaunch { agent: a.agent.clone(), account: a.name.clone() },
                };
                items.push(PickItem {
                    label: accounts::label(&a),
                    detail,
                    action,
                    state: full.then_some(AgentState::Blocked),
                    dim: false,
                });
            }
            if switch_for.is_none() {
                items.push(PickItem {
                    label: format!("+ add a {} account", kind.name),
                    detail: "new subscription: a separate config dir and the agent's own login".into(),
                    action: PickAction::AgentAdd { agent: kind.name.to_string() },
                    state: None,
                    dim: false,
                });
            }
        }
        if switch_for.is_none() {
            items.extend(self.worktree_items(client));
        }
        let title = if switch_for.is_some() { "Change account" } else { "Start an agent" };
        Picker::new(PickerKind::Agent, title, items)
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
                if !server.accounts.get(&to.agent, &to.name).is_some_and(|a| a.same_home(&to)) {
                    return;
                }
                if let Some(c) = server.clients.get_mut(&client)
                    && let Mode::Picker(picker) = &mut c.mode
                {
                    for item in &mut picker.items {
                        if matches!(&item.action, PickAction::AgentSwitch { pane: p, agent, account, current: false }
                            if *p == pane && agent == &to.agent && account == &to.name)
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
    pub(super) fn add_account(&mut self, client: ClientId, agent: &str, name: &str) -> Result<()> {
        let home = accounts::default_home_for(agent, name);
        let a = Account {
            name: name.into(),
            agent: agent.into(),
            home: home.display().to_string(),
            threshold: None,
            note: None,
            sign_in_required: true,
        };
        Accounts::update(|all| {
            all.add(a.clone())?;
            accounts::prepare_home(&a)
        })?;
        self.reload_accounts();
        self.open_login(client, &a)
    }

    pub(super) fn open_login(&mut self, client: ClientId, a: &Account) -> Result<()> {
        if let Some(id) =
            self.panes.values().find_map(|p| p.spec.login.as_ref().filter(|l| l.account.same_home(a)).map(|_| p.id))
        {
            self.reveal(client, id);
            return Ok(());
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
        self.spawn_pane_in(ws, None, spec, Placement::Floating, Some(format!("login {}", accounts::label(a))))?;
        Ok(())
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
                    let _ = accounts::save_usage(&a.agent, &a.name, &u);
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
                agent: "claude".into(),
                name: name.into(),
                home: dir.join(name).display().to_string(),
                threshold: None,
                note: None,
                sign_in_required: false,
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
        for changed in ["transcript", "session", "target"] {
            let mut t = SwitchTest::new(&format!("changed-{changed}"));
            t.checked(Status::SignedIn);
            match changed {
                "transcript" => std::fs::remove_file(t.dir.join("from/projects/work/session.jsonl")).unwrap(),
                "session" => t.server.panes.get_mut(&1).unwrap().agent.session_id = Some("new-session".into()),
                _ => {
                    t.server.accounts.remove("claude", "to");
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
                    "claude:to".into(),
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
        t.server.usage.0.insert("claude:from".into(), usage(95.0));
        t.server.usage.0.insert("claude:to".into(), usage(0.0));
        t.server.limit_alerts.insert(1, LimitAlert::Attempted);
        t.checked(Status::NotSignedIn);
        t.server.panes.get_mut(&1).unwrap().agent.state = AgentState::Done;
        for _ in 0..10 {
            t.server.check_limits();
        }
        assert!(t.server.switches.is_empty());
        assert!(t.server.jobs.is_empty());
        assert_eq!(t.server.limit_alerts.get(&1), Some(&LimitAlert::Attempted));
        t.server.usage.0.insert("claude:from".into(), usage(10.0));
        t.server.check_limits();
        assert!(!t.server.limit_alerts.contains_key(&1));
    }
}
