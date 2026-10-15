use std::time::{Duration, Instant};

use super::super::Server;
use super::super::ui::Mode;
use super::LimitAlert;
use crate::accounts::{self, Account, Mode as LimitMode, UsageMap};
use crate::agent::AgentState;
use crate::layout::PaneId;
use crate::theme::NoticeKind;

impl Server {
    pub(in crate::server) fn reload_usage(&mut self) {
        self.usage = UsageMap::load(&self.accounts);
        self.await_usage_reset();
    }

    /// Wake when the usage window that resets next does: what the clients show of the accounts
    /// changes then, with nothing else happening. Called when the usage changed and when that
    /// moment comes; a reset awaited that has come makes every client's frame due, whichever
    /// call sees it first (the clock may have been set, or the machine asleep meanwhile).
    pub(in crate::server) fn await_usage_reset(&mut self) {
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

    /// Thresholds: alert, offer or switch when an agent's account is nearly used up.
    pub(in crate::server) fn check_limits(&mut self) {
        let now = accounts::now();
        let mode = self.accounts.limits.mode;
        let ids: Vec<PaneId> = self.panes.keys().copied().collect();
        let panes = &self.panes;
        self.screen_limits.retain(|p, _| panes.contains_key(p));
        self.switches.retain_panes(|p| panes.contains_key(&p));
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
            if self.switches.contains(id) {
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
                        super::super::attention::ToastAction::ChangeAccount,
                    );
                    self.limit_alerts.insert(id, LimitAlert::Notified);
                }
                _ => {}
            }
        }
    }

    /// Ask every Codex account's app-server for its limits, in the background.
    pub(in crate::server) fn poll_codex_usage(&mut self) {
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
