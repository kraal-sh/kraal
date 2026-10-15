use anyhow::{Context, Result};

use super::super::{ClientId, Server};
use crate::account_status::{self, Status};
use crate::accounts::{self, Account};
use crate::layout::PaneId;
use crate::pane::{Login, LoginState, PaneSpec};
use crate::protocol::Placement;
use crate::theme::NoticeKind;

impl Server {
    /// Create an account and open its login in a floating pane.
    pub(in crate::server) fn add_account(&mut self, client: ClientId, agent: &str, name: &str) -> Result<PaneId> {
        let kind = accounts::agent_kind(agent).context("Unknown agent")?;
        anyhow::ensure!(
            crate::transfer::have_program(kind.command),
            "{} is not installed. Install {}, then try again.",
            super::super::account_ui::agent_name(agent),
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

    pub(in crate::server) fn open_login(&mut self, client: ClientId, a: &Account) -> Result<PaneId> {
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

    pub(in crate::server) fn login_exited(&mut self, id: PaneId, code: Option<i32>) {
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

    pub(in crate::server) fn check_login(&mut self, id: PaneId) -> Result<()> {
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

    pub(in crate::server) fn retry_login(&mut self, id: PaneId) -> Result<()> {
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
}

pub(in crate::server) fn login_start_failed(command: &[String]) -> LoginState {
    if command.first().is_some_and(|c| crate::transfer::have_program(c)) {
        LoginState::Failed
    } else {
        LoginState::Checked(Status::NotInstalled)
    }
}
