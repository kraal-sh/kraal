//! Focus and attention: which pane a client sees, agents that need someone, notifications.

use std::time::{Duration, Instant};

use super::{ClientId, Server, frame_bytes};
use crate::agent::AgentState;
use crate::config::NotifyMode;
use crate::layout::PaneId;
use crate::pane::Pane;
/// Warnings kept while no client is attached.
const MAX_PENDING_WARNINGS: usize = 50;

impl Server {
    /// Focused pane of the tab a client is looking at.
    pub fn client_focus(&self, id: ClientId) -> Option<PaneId> {
        let c = self.clients.get(&id)?;
        let wi = self.ws_index(c.workspace)?;
        self.workspaces[wi].tab().focus
    }

    /// Make a pane visible and focused for a client.
    pub fn reveal(&mut self, client: ClientId, pane: PaneId) {
        let Some((wi, ti)) = self.workspace_of(pane) else { return };
        let ws = &mut self.workspaces[wi];
        ws.active = ti;
        let zoom = ws.tabs[ti].zoom;
        ws.tabs[ti].set_focus(pane);
        let zoom_moved = ws.tabs[ti].zoom != zoom;
        let wid = ws.id;
        if let Some(c) = self.clients.get_mut(&client) {
            c.workspace = wid;
        }
        self.mark_seen_focused(client);
        // A zoomed tab zooms on the pane revealed: it takes the whole area.
        if zoom_moved {
            self.relayout();
        }
        self.mark_dirty();
    }

    /// After anything that may have moved the focus: the panes that gained or lost it are told
    /// (mode 1004), and each client's terminal reports pointer motion only while the pane it
    /// shows asks for it (mode 1003). A pane has the focus while an attached client whose
    /// terminal has the focus shows it. Each terminal's keyboard level follows its client's mode
    /// too ([`Server::follow_keyboard`]).
    pub(super) fn follow_focus(&mut self) {
        let shown: std::collections::HashSet<PaneId> = self
            .clients
            .values()
            .filter(|c| c.attached && c.terminal_focused)
            .filter_map(|c| self.client_focus(c.id))
            .collect();
        if shown != self.focus_reported {
            for p in self.focus_reported.difference(&shown).copied().collect::<Vec<_>>() {
                self.report_focus(p, false);
            }
            for p in shown.difference(&self.focus_reported).copied().collect::<Vec<_>>() {
                self.report_focus(p, true);
            }
            self.focus_reported = shown;
        }
        let ids: Vec<ClientId> = self.clients.values().filter(|c| c.attached).map(|c| c.id).collect();
        for id in ids {
            let wants = self.config.mouse
                && matches!(self.clients[&id].mode, super::ui::Mode::Normal)
                && self.client_focus(id).and_then(|p| self.panes.get(&p)).is_some_and(|p| p.input_modes().mouse_motion);
            let Some(c) = self.clients.get_mut(&id) else { continue };
            if c.pointer_motion != wants {
                // Leaving 1003 stops tracking altogether: back to 1002 (motion with a button held).
                let mode: &[u8] = if wants { b"\x1b[?1003h" } else { b"\x1b[?1003l\x1b[?1002h" };
                // Recorded once on its way only: a full queue means trying again next time.
                if c.out.try_send(frame_bytes(mode)).is_ok() {
                    c.pointer_motion = wants;
                }
            }
        }
        self.follow_keyboard();
    }

    fn report_focus(&mut self, pane: PaneId, gained: bool) {
        if let Some(p) = self.panes.get_mut(&pane)
            && p.input_modes().focus_events
        {
            p.write_input(if gained { b"\x1b[I".to_vec() } else { b"\x1b[O".to_vec() });
        }
    }

    /// Done agents and long commands in the client's focused pane have been seen.
    pub fn mark_seen_focused(&mut self, client: ClientId) {
        if let Some(p) = self.client_focus(client)
            && let Some(pane) = self.panes.get_mut(&p)
            && (pane.agent.mark_seen() | pane.set_command_unseen(false))
        {
            self.mark_dirty();
        }
    }

    /// Panes needing attention: blocked agents first, then finished agents and long commands,
    /// oldest first.
    pub fn attention_queue(&self) -> Vec<PaneId> {
        let mut v: Vec<(&Pane, bool, Instant)> = self
            .panes
            .values()
            .filter_map(|p| {
                if p.agent.state.needs_attention() {
                    Some((p, p.agent.state != AgentState::Blocked, p.agent.since))
                } else {
                    p.command_unseen().map(|at| (p, true, at))
                }
            })
            .collect();
        v.sort_by_key(|(_, later, at)| (*later, *at));
        v.into_iter().map(|(p, ..)| p.id).collect()
    }

    /// Whether some attached client is looking at this pane right now.
    pub(super) fn pane_watched(&self, pane: PaneId) -> bool {
        self.clients.values().any(|c| {
            c.attached
                && c.terminal_focused
                && self.client_focus(c.id) == Some(pane)
                && c.last_input.elapsed() < Duration::from_secs(120)
        })
    }

    pub fn on_agent_change(&mut self, pane: PaneId, before: AgentState) {
        let Some(p) = self.panes.get(&pane) else { return };
        let state = p.agent.state;
        if state == before {
            return;
        }
        if state != AgentState::Unknown {
            self.announce_pane(pane);
            let Some(p) = self.panes.get(&pane) else { return };
            let fields = serde_json::json!({
                "agent": p.agent.kind,
                "account": p.spec.account,
                "previous": before.label(),
                "message": p.agent.message,
            });
            self.emit_pane(&format!("agent.{}", state.label()), pane, fields);
        }
        let Some(p) = self.panes.get(&pane) else { return };
        if self.pane_watched(pane) {
            if state == AgentState::Done
                && let Some(p) = self.panes.get_mut(&pane)
            {
                p.agent.mark_seen();
            }
            self.check_waits();
            return;
        }
        let label = self.pane_label(pane);
        let agent = p.agent.kind.clone().unwrap_or_else(|| "agent".into());
        match state {
            AgentState::Blocked if self.config.notifications.on_blocked => {
                let msg = p.agent.message.clone().unwrap_or_else(|| "needs your input".into());
                self.notify(&format!("{agent} · {label}"), &msg, Some(pane));
            }
            AgentState::Done if self.config.notifications.on_done && before == AgentState::Working => {
                self.notify(&format!("{agent} · {label}"), "finished", Some(pane));
            }
            _ => {}
        }
        self.check_waits();
    }

    /// Desktop notification through the attached terminals and/or notify-send.
    pub fn notify(&mut self, title: &str, body: &str, pane: Option<PaneId>) {
        let mut fields = pane.map(|p| self.pane_fields(p)).unwrap_or_default();
        fields.insert("title".into(), title.into());
        fields.insert("body".into(), body.into());
        self.emit("notification", serde_json::Value::Object(fields));
        let mode = self.config.notifications.mode;
        if matches!(mode, NotifyMode::Terminal | NotifyMode::Both) {
            let seq = crate::pane::osc_notify(title, body);
            for c in self.clients.values().filter(|c| c.attached) {
                let _ = c.out.try_send(frame_bytes(&seq));
            }
        }
        if matches!(mode, NotifyMode::System | NotifyMode::Both) {
            let spawned = std::process::Command::new("notify-send")
                .args(["--app-name=kraal", title, body])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            if let Ok(child) = spawned {
                crate::exec::reap_later(child);
            }
        }
        for c in self.clients.values_mut().filter(|c| c.attached) {
            c.toast = Some((format!("{title}: {body}"), Instant::now() + Duration::from_secs(4)));
            c.dirty = true;
        }
        self.schedule_render();
    }

    /// Tell a client something, briefly; the messages log keeps it.
    pub fn toast(&mut self, client: ClientId, msg: impl Into<String>) {
        let msg = msg.into();
        self.log_message(&msg, false);
        if let Some(c) = self.clients.get_mut(&client) {
            // Long messages stay longer.
            let secs = (3 + msg.chars().count() as u64 / 25).min(10);
            c.toast = Some((msg, Instant::now() + Duration::from_secs(secs)));
            c.dirty = true;
        }
        self.schedule_render();
    }

    fn log_message(&mut self, text: &str, warning: bool) {
        if self.messages.len() >= super::MAX_MESSAGES {
            self.messages.pop_front();
        }
        self.messages.push_back(super::Message { at: std::time::SystemTime::now(), text: text.to_string(), warning });
    }

    /// Show the pending warnings to the attached clients (and log them).
    pub(super) fn show_warnings(&mut self) {
        if self.warnings.is_empty() {
            return;
        }
        if !self.clients.values().any(|c| c.attached) {
            // Kept for the next client, once each and only the latest: a server left detached
            // for days does not pile them up.
            let mut seen = std::collections::HashSet::new();
            let mut kept: Vec<String> = self.warnings.drain(..).rev().filter(|w| seen.insert(w.clone())).collect();
            kept.truncate(MAX_PENDING_WARNINGS);
            kept.reverse();
            self.warnings = kept;
            return;
        }
        let warnings = std::mem::take(&mut self.warnings);
        for w in &warnings {
            self.log_message(w, true);
        }
        let text = match warnings.len() {
            1 => warnings[0].clone(),
            n => format!(
                "{} (and {} more: {} shows them)",
                warnings[0],
                n - 1,
                self.keymap.hint(crate::keymap::Action::Messages).unwrap_or_default()
            ),
        };
        for c in self.clients.values_mut().filter(|c| c.attached) {
            c.toast = Some((text.clone(), Instant::now() + Duration::from_secs(10)));
            c.dirty = true;
        }
        self.schedule_render();
    }

    /// The welcome note goes once the menu has been opened.
    pub(super) fn dismiss_welcome(&mut self, client: ClientId) {
        if let Some(c) = self.clients.get_mut(&client)
            && std::mem::take(&mut c.welcome)
        {
            let marker = crate::paths::state_dir().join("welcomed");
            let _ = crate::paths::ensure_private_dir(&crate::paths::state_dir());
            let _ = std::fs::write(marker, "");
        }
    }

    /// Open a link in the client's browser. A `localhost` address of the server's machine means
    /// nothing on a remote client's: it is copied instead, with a word on where it works.
    pub fn open_for(&mut self, client: ClientId, url: &str) {
        let remote = self.clients.get(&client).is_some_and(|c| c.remote);
        if remote && crate::hints::loopback_port(url).is_some() {
            let host = crate::context::hostname().unwrap_or_else(|| "the server".into());
            self.copy_to_client(client, url);
            self.toast(client, format!("{url} is on {host}, where kraal runs: copied (open it there, or ssh -L)"));
            return;
        }
        self.send_msg(client, &crate::protocol::ServerMsg::Open { url: url.to_string() });
        self.toast(client, format!("opening {url}"));
    }
}
