//! Focus and attention: which pane a client sees, agents that need someone, notifications.

use std::time::{Duration, Instant};

use super::{ClientId, Dirty, Server, frame_bytes};
use crate::agent::AgentState;
use crate::config::NotifyMode;
use crate::layout::PaneId;
/// Warnings kept while no client is attached.
const MAX_PENDING_WARNINGS: usize = 50;

/// Something that needs the user: a pane here, or an agent of a server's kraal session (its
/// pane's id there).
#[derive(Debug, Clone, PartialEq)]
pub enum Attention {
    Pane(PaneId),
    Remote { server: String, pane: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToastAction {
    ChangeAccount,
    UndoAccount,
    RestoreWorkspace(String),
}

impl ToastAction {
    pub(super) fn label(&self) -> &'static str {
        match self {
            Self::ChangeAccount => "Change account",
            Self::UndoAccount | Self::RestoreWorkspace(_) => "Undo",
        }
    }
}

impl Server {
    /// Focused pane of the tab a client is looking at.
    pub fn client_focus(&self, id: ClientId) -> Option<PaneId> {
        let c = self.clients.get(&id)?;
        let wi = self.ws_index(c.workspace)?;
        self.workspaces[wi].tab()?.focus
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
        let shown = || {
            self.clients.values().filter(|c| c.attached && c.terminal_focused).filter_map(|c| self.client_focus(c.id))
        };
        let same = shown().all(|p| self.focus_reported.contains(&p))
            && self.focus_reported.iter().all(|p| shown().any(|q| q == *p));
        if !same {
            let shown: std::collections::HashSet<PaneId> = shown().collect();
            for p in self.focus_reported.difference(&shown).copied().collect::<Vec<_>>() {
                self.report_focus(p, false);
            }
            for p in shown.difference(&self.focus_reported).copied().collect::<Vec<_>>() {
                self.report_focus(p, true);
            }
            self.focus_reported = shown;
        }
        // The clients whose terminal must report pointer motion, or stop.
        let motion: Vec<(ClientId, bool)> = self
            .clients
            .values()
            .filter(|c| c.attached)
            .filter_map(|c| {
                let wants = self.config.mouse
                    && matches!(c.mode, super::ui::Mode::Normal)
                    && self
                        .client_focus(c.id)
                        .and_then(|p| self.panes.get(&p))
                        .is_some_and(|p| p.wants_pointer_motion());
                (c.pointer_motion != wants).then_some((c.id, wants))
            })
            .collect();
        for (id, wants) in motion {
            let Some(c) = self.clients.get_mut(&id) else { continue };
            // Leaving 1003 stops tracking altogether: back to 1002 (motion with a button held).
            let mode: &[u8] = match (wants, self.config.mouse) {
                (true, _) => b"\x1b[?1003h",
                (false, true) => b"\x1b[?1003l\x1b[?1002h",
                (false, false) => b"\x1b[?1003l",
            };
            // Recorded once on its way only: a full queue means trying again next time.
            if c.out.try_send(frame_bytes(mode)).is_ok() {
                c.pointer_motion = wants;
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

    /// What needs attention: blocked agents first, then finished agents and long commands,
    /// oldest first; agents of the servers watched among them.
    pub fn attention_queue(&self) -> Vec<Attention> {
        let mut v: Vec<(Attention, bool, Instant)> = self
            .panes
            .values()
            .filter_map(|p| {
                if p.agent.state.needs_attention() {
                    Some((Attention::Pane(p.id), p.agent.state != AgentState::Blocked, p.agent.since))
                } else {
                    p.command_unseen().map(|at| (Attention::Pane(p.id), true, at))
                }
            })
            .collect();
        v.extend(self.remote_agents().filter(|(_, a)| a.state.needs_attention()).map(|(server, a)| {
            (Attention::Remote { server: server.to_string(), pane: a.pane }, a.state != AgentState::Blocked, a.since)
        }));
        v.sort_by_key(|(_, later, at)| (*later, *at));
        v.into_iter().map(|(a, ..)| a).collect()
    }

    /// Whether `client` is looking at what needs attention.
    pub(super) fn attention_shown(&self, client: ClientId, a: &Attention) -> bool {
        match a {
            Attention::Pane(p) => self.client_focus(client) == Some(*p),
            Attention::Remote { server, pane } => self.remote_shown(client, server, *pane),
        }
    }

    /// Take `client` to what needs attention.
    pub(super) fn go_to(&mut self, client: ClientId, a: Attention) {
        match a {
            Attention::Pane(p) => self.reveal(client, p),
            Attention::Remote { server, pane } => self.go_to_remote(client, &server, pane),
        }
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
                "account_id": p.spec.account_id,
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
            c.toast_action = None;
            c.dirty = Dirty::Yes;
        }
        self.schedule_render();
    }

    pub(super) fn notify_action(&mut self, message: &str, pane: PaneId, action: ToastAction) {
        let label = action.label();
        self.notify(&format!("{message} · {label}"), "", Some(pane));
        for c in self.clients.values_mut().filter(|c| c.attached) {
            c.toast = Some((format!("{message} · {label}"), Instant::now() + Duration::from_secs(6)));
            c.toast_action = Some((pane, action.clone()));
        }
        self.mark_dirty();
    }

    pub(super) fn toast_action(&mut self, client: ClientId, pane: PaneId, action: ToastAction) -> super::ui::Mode {
        if let ToastAction::RestoreWorkspace(identity) = &action {
            return self.restore_workspace_mode(client, identity, None);
        }
        if !self.panes.contains_key(&pane) {
            return super::ui::Mode::Normal;
        }
        match action {
            ToastAction::ChangeAccount => self.change_account_mode(client, pane),
            ToastAction::UndoAccount => {
                if let Err(e) = self.undo_account_change(pane, client) {
                    self.toast(client, format!("{e:#}"));
                }
                super::ui::Mode::Normal
            }
            ToastAction::RestoreWorkspace(_) => unreachable!("answered above"),
        }
    }

    /// Tell a client something, briefly; the messages log keeps it.
    pub fn toast(&mut self, client: ClientId, msg: impl Into<String>) {
        let msg = msg.into();
        self.log_message(&msg, false);
        if let Some(c) = self.clients.get_mut(&client) {
            // Long messages stay longer.
            let secs = (3 + msg.chars().count() as u64 / 25).min(10);
            c.toast = Some((msg, Instant::now() + Duration::from_secs(secs)));
            c.toast_action = None;
            c.dirty = Dirty::Yes;
        }
        self.schedule_render();
    }

    /// Tell a client that attaches with a newer kraal installed than this server's that a
    /// restart brings it: once, after the message the client is being shown, if any.
    pub(super) fn version_notice(&mut self, client: ClientId, installed: &str) {
        let plain = installed.len() <= 40 && installed.chars().all(|c| c.is_ascii_alphanumeric() || ".-+".contains(c));
        if !plain || !crate::protocol::newer(installed, crate::protocol::KRAAL_VERSION) {
            return;
        }
        let how = self.keymap.hint(crate::keymap::Action::Restart).unwrap_or_else(|| "kraal restart".into());
        let text = format!(
            "kraal {installed} installed; this session runs {}: {how} restarts it",
            crate::protocol::KRAAL_VERSION
        );
        let Some(c) = self.clients.get_mut(&client) else { return };
        if c.toast.is_some() {
            c.notice = Some(text);
        } else {
            self.toast(client, text);
        }
    }

    fn log_message(&mut self, text: &str, warning: bool) {
        if self.messages.len() >= super::MAX_MESSAGES {
            self.messages.pop_front();
        }
        self.messages.push_back(super::Message { at: std::time::SystemTime::now(), text: text.to_string(), warning });
    }

    /// Log pending warnings and show them to attached clients with no active message.
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
        let now = Instant::now();
        for c in self.clients.values_mut().filter(|c| c.attached) {
            // A queued warning may already be obsolete after the user's action. Keep its
            // result visible, even before the first frame; the warning stays in Messages.
            if c.toast.as_ref().is_some_and(|(_, until)| *until > now) {
                continue;
            }
            c.toast = Some((text.clone(), now + Duration::from_secs(10)));
            c.toast_action = None;
            c.dirty = Dirty::Yes;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{
        queue,
        tests::{client, server},
    };

    #[test]
    fn a_pending_warning_cannot_hide_a_result_before_its_first_frame() {
        for (cols, rows) in [(80, 24), (124, 36)] {
            for warning_first in [false, true] {
                let (tx, _rx) = queue::channel(16);
                let mut server = server(tx);
                let (id, _peer) = client(&mut server);
                let c = server.clients.get_mut(&id).unwrap();
                c.cols = cols;
                c.rows = rows;
                let warning = "reading accounts.toml: Is a directory";
                let result = "claude·work is signed in";
                server.warnings.push(warning.into());
                if warning_first {
                    server.show_warnings();
                }
                server.toast(id, result);
                let until = server.clients[&id].toast.as_ref().unwrap().1;
                // A watch can queue an error before the retry repairs the file. The tick
                // can deliver it after the retry succeeds, before any frame shows success.
                server.show_warnings();
                let mut buffer = ratatui::buffer::Buffer::empty(server.client_screen(id));
                server.compose(id, crate::accounts::now(), &mut buffer);
                let text: String = buffer.content.iter().map(|c| c.symbol()).collect();
                assert!(text.contains(result), "{cols}x{rows}: {text}");
                assert_eq!(server.clients[&id].toast, Some((result.into(), until)));
                assert!(server.messages.iter().any(|m| m.warning && m.text == warning));
                assert!(server.messages.iter().any(|m| !m.warning && m.text == result));
                assert!(server.warnings.is_empty());
                assert!(server.clients[&id].notice.is_none(), "an obsolete warning must not be replayed later");
            }
        }
    }

    #[test]
    fn warnings_keep_an_active_notification_and_its_action() {
        for action in [ToastAction::UndoAccount, ToastAction::ChangeAccount] {
            let (tx, _rx) = queue::channel(16);
            let mut server = server(tx);
            let (id, _peer) = client(&mut server);
            server.config.notifications.mode = NotifyMode::Off;
            server.notify_action("work", 42, action.clone());
            let before = server.clients[&id].toast.clone();
            server.warnings.push("could not save paste buffers".into());
            server.show_warnings();
            assert_eq!(server.clients[&id].toast, before);
            assert_eq!(server.clients[&id].toast_action, Some((42, action)));
            assert_eq!(server.messages.back().unwrap().text, "could not save paste buffers");
            assert!(server.messages.back().unwrap().warning);
        }
    }

    #[test]
    fn warnings_show_on_idle_clients_and_clear_expired_actions() {
        let (tx, _rx) = queue::channel(16);
        let mut server = server(tx);
        let [(busy, _busy_peer), (idle, _idle_peer), (expired, _expired_peer), (detached, _detached_peer)] =
            std::array::from_fn(|_| client(&mut server));
        server.toast(busy, "Workspace renamed");
        let c = server.clients.get_mut(&expired).unwrap();
        c.toast = Some(("Using work · Undo".into(), Instant::now() - Duration::from_secs(1)));
        c.toast_action = Some((42, ToastAction::UndoAccount));
        server.clients.get_mut(&detached).unwrap().attached = false;
        server.warnings.extend(["first warning".into(), "second warning".into()]);
        server.show_warnings();
        assert_eq!(server.clients[&busy].toast.as_ref().unwrap().0, "Workspace renamed");
        for id in [idle, expired] {
            let c = &server.clients[&id];
            assert!(c.toast.as_ref().unwrap().0.starts_with("first warning (and 1 more:"));
            assert!(c.toast_action.is_none(), "a warning must not retain an unrelated action");
        }
        assert!(server.clients[&detached].toast.is_none());
        assert_eq!(server.messages.iter().filter(|m| m.warning).count(), 2);
        server.show_warnings();
        assert_eq!(server.messages.iter().filter(|m| m.warning).count(), 2);
    }

    #[test]
    fn warnings_while_detached_are_bounded_and_logged_when_a_client_attaches() {
        let (tx, _rx) = queue::channel(16);
        let mut server = server(tx);
        for n in 0..MAX_PENDING_WARNINGS + 10 {
            server.warnings.extend([format!("warning {n}"), format!("warning {n}")]);
        }
        server.show_warnings();
        assert_eq!(server.warnings.len(), MAX_PENDING_WARNINGS);
        assert_eq!(server.warnings[0], "warning 10");
        assert!(server.messages.is_empty());
        let (id, _peer) = client(&mut server);
        server.show_warnings();
        assert!(server.clients[&id].toast.as_ref().unwrap().0.starts_with("warning 10 (and 49 more:"));
        assert_eq!(server.messages.len(), MAX_PENDING_WARNINGS);
        assert!(server.messages.iter().all(|m| m.warning));
        assert!(server.warnings.is_empty());
    }
}
