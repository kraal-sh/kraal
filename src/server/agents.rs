//! The agent manager in the server: launching agents on accounts, the agent picker, usage
//! thresholds, and continuing a conversation on another account.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

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

/// Why a pane waits for Enter before its agent starts again.
pub(super) const AMBIGUOUS: &str =
    "kraal cannot tell this command's options from a prompt it would send again: check it, Enter runs it as shown";

impl Server {
    pub(super) fn reload_accounts(&mut self) {
        match Accounts::load() {
            Ok(a) => {
                self.accounts = a;
                self.accounts_error = None;
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

    /// Rebuild an agent pane's spec so that starting it again resumes its conversation, with
    /// the options it was started with (never a prompt given on its command line: it would be
    /// sent again). `session` is the conversation seen running; the id the pane was started
    /// with counts only once a conversation exists under it (a pane that never ran starts the
    /// conversation it was meant to). The flag says whether that may happen by itself (a
    /// restore): not for a one-shot run (`claude -p`), which stays as it was, nor when a prompt
    /// was left out or would be sent; those wait for Enter.
    pub fn resume_spec(&self, spec: &PaneSpec, session: Option<&str>) -> (PaneSpec, bool) {
        let Some((agent, name)) = spec.account.as_deref().and_then(|k| k.split_once(':')) else {
            return (spec.clone(), false);
        };
        let (Some(a), Some(kind)) = (self.accounts.get(agent, name), accounts::agent_kind(agent)) else {
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
    #[allow(clippy::too_many_arguments)]
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

    /// Continue an agent pane's conversation on another account. The agent stops first: its
    /// conversation is carried over once nothing writes it any more.
    pub fn switch_account(&mut self, pane: PaneId, to: &Account) -> Result<()> {
        let p = self.panes.get(&pane).context("no such pane")?;
        let from = self.pane_account(pane).context("this pane was not started from the agent manager")?;
        anyhow::ensure!(from.agent == to.agent, "{} cannot continue on a {} account", from.agent, to.agent);
        let kind = accounts::agent_kind(&to.agent).context("unknown agent")?;
        accounts::resumable_args(kind, &accounts::launch_args(kind, &p.spec.command))
            .context("a one-shot run (-p, exec) is not a conversation to continue")?;
        let session = p.agent.session_id.clone().or_else(|| p.spec.session.clone());
        // Refused before anything stops when the conversation cannot follow whole.
        if let Some(id) = &session {
            accounts::check_carry(&from, to, id)?;
        }
        let exited = p.exit_code.is_some();
        self.switches.insert(pane, (from, to.clone(), session));
        if exited {
            self.finish_switch(pane);
        } else if let Some(pn) = self.panes.get_mut(&pane) {
            pn.kill();
        }
        Ok(())
    }

    /// The agent of a pane switching accounts has stopped (its pane shows it ended): carry its
    /// conversation over and start it again on the new account. When the conversation cannot
    /// follow, or the new agent cannot start, the pane stays as it is: Enter starts it again on
    /// its account, rather than start a new conversation. When its command must be shown first
    /// (a word kraal could not tell from a prompt), it waits for Enter on the new account.
    pub(super) fn finish_switch(&mut self, pane: PaneId) {
        let Some((from, to, session)) = self.switches.remove(&pane) else { return };
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
        self.limit_alerts.remove(&pane);
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
            let peak = self.usage.peak(&a, now);
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
                            cl.mode = Mode::confirm(message, action);
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
            // Not marked before git has said which repository rules see here.
            let preferred = (!self.accounts.undecided(kind.name, cwd.as_deref()))
                .then(|| self.accounts.choose(kind.name, ws_name.as_deref(), cwd.as_deref(), &self.usage, now))
                .flatten();
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
                "Start a new conversation on… (conversations stay with their account)"
            }
        } else {
            "Start an agent"
        };
        Picker::new(PickerKind::Agent, title, items)
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
        };
        Accounts::update(|all| {
            all.add(a.clone())?;
            accounts::prepare_home(&a)
        })?;
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
