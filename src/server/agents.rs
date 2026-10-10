//! The agent manager in the server: launching agents on accounts, the agent picker, usage
//! thresholds, and continuing a conversation on another account.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::ui::{ConfirmAction, Mode, PickAction, PickItem, Picker, PickerKind};
use super::{ClientId, Server};
use crate::accounts::{self, Account, Accounts, Mode as LimitMode, UsageMap};
use crate::agent::AgentState;
use crate::layout::PaneId;
use crate::pane::PaneSpec;
use crate::protocol::Placement;

/// What kraal last did about a pane being over its threshold, so it does not repeat itself.
#[derive(Debug, Clone, PartialEq)]
pub enum LimitAlert {
    Notified,
    Asked,
}

/// Whether a conversation can move between accounts of this agent (they share its storage).
fn conversations_move(agent: &str) -> bool {
    accounts::agent_kind(agent).is_some_and(|k| k.shared.contains(&"projects"))
}

impl Server {
    pub(super) fn reload_accounts(&mut self) {
        match Accounts::load() {
            Ok(a) => self.accounts = a,
            Err(e) => self.warnings.push(format!("{e:#}")),
        }
    }

    pub(super) fn reload_usage(&mut self) {
        self.last_usage_load = Instant::now();
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

    /// Rebuild an agent pane's spec so that starting it again resumes its conversation.
    pub fn resume_spec(&self, spec: &PaneSpec, session: Option<&str>) -> PaneSpec {
        let Some((agent, name)) = spec.account.as_deref().and_then(|k| k.split_once(':')) else { return spec.clone() };
        let Some(a) = self.accounts.get(agent, name) else { return spec.clone() };
        let session = session.or(spec.session.as_deref());
        self.agent_spec(&a, spec.cwd.clone(), session, &[]).unwrap_or_else(|_| spec.clone())
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
        accounts::agent_kind(agent)
            .with_context(|| format!("unknown agent {agent:?} (known: {})", accounts::agent_names()))?;
        let wi = self.ws_index(ws).context("no such workspace")?;
        let ws_name = self.workspaces[wi].name.clone();
        let cwd = cwd.or_else(|| {
            let focus =
                target.or_else(|| self.workspaces[wi].tabs.get(self.workspaces[wi].active).and_then(|t| t.focus));
            focus.and_then(|p| self.panes.get(&p)).filter(|p| p.ssh.is_none()).and_then(|p| p.current_cwd())
        });
        let a = match account {
            Some(name) => {
                self.accounts.get(agent, name).with_context(|| format!("{agent} has no account named {name:?}"))?
            }
            None => self
                .accounts
                .choose(agent, Some(&ws_name), cwd.as_deref(), &self.usage, accounts::now())
                .context("no account")?,
        };
        let spec = self.agent_spec(&a, cwd, None, &[])?;
        self.spawn_pane_in(ws, target, spec, placement, Some(a.agent.clone()))
    }

    /// Continue an agent pane's conversation on another account.
    pub fn switch_account(&mut self, pane: PaneId, to: &Account) -> Result<()> {
        let p = self.panes.get(&pane).context("no such pane")?;
        let from = self.pane_account(pane).context("this pane was not started from the agent manager")?;
        anyhow::ensure!(from.agent == to.agent, "{} cannot continue on a {} account", from.agent, to.agent);
        let kind = accounts::agent_kind(&to.agent).context("unknown agent")?;
        // Claude conversations live in a shared directory, so they resume on any account.
        let session = p.agent.session_id.clone().or_else(|| p.spec.session.clone());
        let resume = if kind.shared.contains(&"projects") { session } else { None };
        let cwd = p.current_cwd().or_else(|| p.spec.cwd.clone());
        let spec = self.agent_spec(to, cwd, resume.as_deref(), &[])?;
        let pn = self.panes.get_mut(&pane).unwrap();
        if pn.exit_code.is_some() {
            pn.spec = spec;
            pn.respawn(&self.env, &self.tx)?;
        } else {
            pn.restart_with = Some(spec);
            pn.kill();
        }
        self.mark_persist();
        Ok(())
    }

    /// Thresholds: alert, offer or switch when an agent's account is nearly used up.
    pub(super) fn check_limits(&mut self) {
        let now = accounts::now();
        let mode = self.accounts.limits.mode;
        let ids: Vec<PaneId> = self.panes.keys().copied().collect();
        for id in ids {
            let Some(a) = self.pane_account(id) else { continue };
            // Limit messages on screen are the freshest signal there is.
            let screen_limit = self
                .panes
                .get(&id)
                .filter(|p| p.exit_code.is_none())
                .and_then(|p| accounts::limit_from_screen(&p.text(None)));
            if let Some((weekly, pct)) = screen_limit {
                let mut u = self.usage.get(&a).cloned().unwrap_or_default();
                let w = if weekly { &mut u.weekly } else { &mut u.five_hour };
                if w.as_ref().is_none_or(|w| w.used_percent < pct) {
                    *w = Some(accounts::Window { used_percent: pct, resets_at: w.as_ref().and_then(|w| w.resets_at) });
                    u.observed_at = now;
                    u.source = "screen".into();
                    let _ = accounts::save_usage(&a.agent, &a.name, &u);
                    self.usage.0.insert(accounts::key(&a.agent, &a.name), u);
                }
            }
            let peak = self.usage.peak(&a, now);
            let threshold = self.accounts.threshold(&a) as f64;
            if peak < threshold {
                self.limit_alerts.remove(&id);
                continue;
            }
            let Some(p) = self.panes.get(&id) else { continue };
            let idle = matches!(p.agent.state, AgentState::Done | AgentState::Idle) || p.exit_code.is_some();
            let ws_name = self.workspace_of(id).map(|(wi, _)| self.workspaces[wi].name.clone());
            let cwd = p.current_cwd();
            let alt = self.accounts.alternative(&a, ws_name.as_deref(), cwd.as_deref(), &self.usage, now);
            let reset = accounts::describe_reset(self.usage.get(&a).and_then(|u| u.next_reset(now)), now);
            let what = format!(
                "{} is at {peak:.0}%{}",
                accounts::label(&a),
                if reset.is_empty() { String::new() } else { format!(" ({reset})") }
            );
            let already = self.limit_alerts.get(&id).cloned();
            match (mode, &alt) {
                (LimitMode::Auto, Some(alt)) if idle => {
                    let to = accounts::label(alt);
                    match self.switch_account(id, alt) {
                        Ok(()) => {
                            let how =
                                if conversations_move(&alt.agent) { "continuing on" } else { "new conversation on" };
                            self.notify(&what, &format!("{how} {to}"), Some(id))
                        }
                        Err(e) => self.notify(&what, &format!("could not switch: {e:#}"), Some(id)),
                    }
                    self.limit_alerts.insert(id, LimitAlert::Notified);
                }
                (LimitMode::Ask, Some(alt)) if idle && already != Some(LimitAlert::Asked) => {
                    let message = if conversations_move(&alt.agent) {
                        format!("{what}. Continue this conversation on {}?", accounts::label(alt))
                    } else {
                        format!(
                            "{what}. Start a new conversation on {}? ({} conversations stay with their account)",
                            accounts::label(alt),
                            alt.agent
                        )
                    };
                    let action =
                        ConfirmAction::SwitchAccount { pane: id, agent: alt.agent.clone(), account: alt.name.clone() };
                    let client =
                        self.primary.filter(|c| self.clients.get(c).is_some_and(|c| matches!(c.mode, Mode::Normal)));
                    if let Some(c) = client {
                        self.reveal(c, id);
                        if let Some(cl) = self.clients.get_mut(&c) {
                            cl.mode = Mode::Confirm { message, action };
                        }
                        self.limit_alerts.insert(id, LimitAlert::Asked);
                    } else if already.is_none() {
                        self.notify(&what, "open kraal to continue on another account", Some(id));
                        self.limit_alerts.insert(id, LimitAlert::Notified);
                    }
                }
                // In ask and auto modes, wait for the end of the turn rather than interrupting.
                (LimitMode::Ask | LimitMode::Auto, Some(_)) => {}
                _ if already.is_none() => {
                    let hint = match &alt {
                        Some(alt) => format!("ctrl+b A to continue on {}", accounts::label(alt)),
                        None => "no other account under its threshold".to_string(),
                    };
                    self.notify(&what, &hint, Some(id));
                    self.limit_alerts.insert(id, LimitAlert::Notified);
                }
                _ => {}
            }
        }
    }

    pub(super) fn agent_picker(&self, client: ClientId, switch_for: Option<PaneId>) -> Picker {
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
            let preferred = self.accounts.choose(kind.name, ws_name.as_deref(), cwd.as_deref(), &self.usage, now);
            for a in self.accounts.for_agent(kind.name) {
                let u = self.usage.get(&a);
                let mut detail = match u {
                    Some(u) => usage_text(u, now),
                    None => "no usage seen yet".to_string(),
                };
                if preferred.as_ref().is_some_and(|p| p.name == a.name) {
                    detail.push_str(" · picked here");
                }
                if current.as_ref().is_some_and(|c| c.name == a.name) {
                    detail.push_str(" · current");
                }
                let full = self.usage.peak(&a, now) >= self.accounts.threshold(&a) as f64;
                let action = match switch_for {
                    Some(pane) => PickAction::AgentSwitch { pane, agent: a.agent.clone(), account: a.name.clone() },
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
                    dim: true,
                });
            }
        }
        if switch_for.is_none() {
            items.extend(self.worktree_items(client));
        }
        let title = if switch_for.is_some() {
            if current.as_ref().is_some_and(|c| conversations_move(&c.agent)) {
                "Continue this conversation on…"
            } else {
                "Start a new conversation on… (this agent's conversations stay with their account)"
            }
        } else {
            "Agents  (enter start here · ctrl+t new tab · ctrl+l log in again · ctrl+x forget)"
        };
        Picker::new(PickerKind::Agent, title, items)
    }

    /// Create an account and open its login in a floating pane.
    pub(super) fn add_account(&mut self, client: ClientId, agent: &str, name: &str) -> Result<()> {
        let mut all = Accounts::load()?;
        let home = accounts::default_home_for(agent, name);
        let a = Account {
            name: name.into(),
            agent: agent.into(),
            home: home.display().to_string(),
            threshold: None,
            note: None,
        };
        all.add(a.clone())?;
        accounts::prepare_home(&a)?;
        all.save()?;
        self.reload_accounts();
        self.open_login(client, &a)
    }

    pub(super) fn open_login(&mut self, client: ClientId, a: &Account) -> Result<()> {
        let l = accounts::login(a)?;
        let ws = self.clients.get(&client).map(|c| c.workspace).context("no client")?;
        let spec = PaneSpec { command: l.argv, env: l.env, env_remove: l.env_remove, ..Default::default() };
        self.spawn_pane_in(ws, None, spec, Placement::Floating, Some(format!("login {}", accounts::label(a))))?;
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
        let _ = std::thread::Builder::new().name("codex-usage".into()).spawn(move || {
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
            let _ = tx.send(super::Event::Wake);
        });
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
