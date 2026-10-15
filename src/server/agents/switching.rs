use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::super::attention;
use super::super::panes::ProgramStop;
use super::super::ui::{ConfirmAction, Mode, PickAction, PickItem, Picker, PickerKind};
use super::super::{ClientId, Server};
use super::choices::cannot_switch_mode;
use super::{AMBIGUOUS, AccountSwitch, SwitchStage};
use crate::account_status::{self, Status};
use crate::accounts::{self, Account};
use crate::agent::AgentState;
use crate::layout::PaneId;
use crate::pane::PaneSpec;
use crate::theme::NoticeKind;

pub(super) fn switch_timing(hooked: bool, state: AgentState, exited: bool) -> SwitchStage {
    if exited || hooked && matches!(state, AgentState::Idle | AgentState::Done) {
        SwitchStage::Stopping
    } else if hooked && matches!(state, AgentState::Working | AgentState::Blocked) {
        SwitchStage::Waiting
    } else {
        SwitchStage::Confirm
    }
}

/// Whether a conversation can move between accounts of this agent (they share its storage).
fn conversations_move(agent: &str) -> bool {
    crate::agent_registry::find(agent).is_some_and(|a| a.cross_account_resume)
}

impl Server {
    pub(in crate::server) fn undo_account_change(&mut self, pane: PaneId, client: ClientId) -> Result<()> {
        let saved =
            self.panes.get(&pane).and_then(|p| p.spec.account_undo.as_deref()).context("no account change to undo")?;
        let to = self.spec_account(saved).context("the previous account was removed; open Change account")?;
        self.begin_switch(pane, &to, Some(client), false, true)
    }

    /// Reserve the pane before the asynchronous check, so attached clients share one change.
    pub(in crate::server) fn switch_account(
        &mut self,
        pane: PaneId,
        to: &Account,
        client: Option<ClientId>,
        ask: bool,
    ) -> Result<()> {
        self.begin_switch(pane, to, client, ask, false)
    }

    pub(super) fn begin_switch(
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

    pub(super) fn switch_checked(&mut self, pane: PaneId, token: u32, status: Status, approved: bool) {
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

    pub(super) fn switch_failed(&mut self, pane: PaneId, reason: &str, sign_in: bool) {
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

    pub(super) fn prepare_switch(&mut self, pane: PaneId, approved: bool) -> Result<()> {
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

    pub(in crate::server) fn pending_switch_mode(&self, pane: PaneId) -> Mode {
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

    pub(in crate::server) fn cancel_switch(&mut self, pane: PaneId, token: u32) {
        if self.switches.get(&pane).is_some_and(|s| {
            s.token == token && matches!(s.stage, SwitchStage::Checking | SwitchStage::Waiting | SwitchStage::Confirm)
        }) {
            self.switches.remove(&pane);
            self.refresh_switch_views(pane);
            self.mark_dirty();
        }
    }

    pub(in crate::server) fn confirm_switch(&mut self, pane: PaneId, token: u32, client: ClientId, yes: bool) {
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

    pub(in crate::server) fn check_switches(&mut self) {
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
    pub(in crate::server) fn finish_switch(&mut self, pane: PaneId) {
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

    pub(super) fn start_switch_spec(&mut self, pane: PaneId, spec: PaneSpec) -> Result<()> {
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

    pub(in crate::server) fn switch_start_failed(&mut self, pane: PaneId, reason: &str) {
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

    pub(super) fn switch_started(&mut self, pane: PaneId) {
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
                self.notify_action(NoticeKind::Notice, &message, pane, attention::ToastAction::UndoAccount);
            }
        }
        self.refresh_switch_views(pane);
        self.mark_persist();
        self.mark_dirty();
    }
}
