use super::super::ui::{Mode, PickAction, PickItem, Picker, PickerKind};
use super::super::{ClientId, Server};
use crate::account_status::{self, Status};
use crate::accounts::{self, Account};
use crate::agent::AgentState;
use crate::layout::PaneId;
use crate::theme::NoticeKind;

pub(super) fn switch_effect(agent: &str, status: Status, carry: bool) -> &'static str {
    match status {
        Status::NotSignedIn => "Sign-in required",
        Status::SignedIn if agent == "codex" => "New conversation",
        Status::SignedIn if carry => "Continue conversation",
        _ => "Could not verify",
    }
}

pub(super) fn cannot_switch_mode(pane: PaneId) -> Mode {
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

impl Server {
    pub(super) fn refresh_agent_pickers(&mut self) {
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

    pub(in crate::server) fn review_account_picker(&self, existing: &Account) -> Picker {
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

    pub(in crate::server) fn change_account_mode(&mut self, client: ClientId, pane: PaneId) -> Mode {
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
        if self.switches.contains(pane) {
            return self.pending_switch_mode(pane);
        }
        let managed = self.panes.get(&pane).is_some_and(|p| p.spec.account.is_some() || p.spec.account_id.is_some());
        if !managed {
            match self.observed_agent(pane) {
                Ok(Some(a)) => {
                    self.account_discovery.observe(pane, a.clone());
                    // The transcript may be large: read it off the event loop, then take back the
                    // picker it still applies to.
                    let (account, session) = (a.account.clone(), a.session.clone());
                    self.background(
                        move || accounts::check_resume(&account, &session).is_ok(),
                        move |server, resumable| {
                            if resumable || server.account_discovery.observed(&pane) != Some(&a) {
                                return;
                            }
                            if let Some(c) = server.clients.get_mut(&client)
                                && let Mode::Picker(p) = &c.mode
                                && p.items
                                    .iter()
                                    .any(|i| matches!(i.action, PickAction::AgentSwitch { pane: target, .. } if target == pane))
                            {
                                c.mode = cannot_switch_mode(pane);
                                server.mark_dirty();
                            }
                        },
                    );
                }
                Ok(_) => {
                    self.account_discovery.forget_pane(pane);
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

    pub(in crate::server) fn agent_picker(&mut self, client: ClientId, switch_for: Option<PaneId>) -> Picker {
        self.agent_picker_on_path(client, switch_for, &std::env::var_os("PATH").unwrap_or_default())
    }

    pub(super) fn agent_picker_on_path(
        &mut self,
        client: ClientId,
        switch_for: Option<PaneId>,
        path: &std::ffi::OsStr,
    ) -> Picker {
        let cwd = self
            .clients
            .get(&client)
            .and_then(|c| self.agent_cwd(c.workspace, self.client_focus(client), None))
            .unwrap_or_else(|| crate::paths::home_dir().unwrap_or_else(|| "/".into()));
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
                let mut detail = super::super::account_ui::usage_summary(&a, None, self.usage.get(&a), now);
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
                    label: if switch_for.is_some() { accounts::label(&a) } else { super::super::account_ui::label(&a) },
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
}
