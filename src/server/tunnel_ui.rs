//! Tunnel forms and live results use the same controls as the other management screens.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;

use super::net::{self, TunnelState};
use super::ui::{Mode, PickAction, PickItem, Picker, PickerKind, PickerState, Prompt, PromptAction};
use super::{ClientId, Server, forms, overlays};
use crate::ansi::Cursor;
use crate::input::InputEvent;

#[derive(Default)]
pub struct TunnelForm {
    server: String,
    server_cursor: usize,
    local: String,
    local_cursor: usize,
    local_edited: bool,
    field: usize,
    suggestion: Option<u16>,
    back: Option<PickerState>,
}

pub struct TunnelDetails {
    id: u32,
    field: usize,
    scroll: usize,
    back: Option<PickerState>,
    retry_seconds: Option<u64>,
}

fn buttons(suggestion: &str) -> forms::Buttons<'_> {
    forms::Buttons {
        primary: forms::Button { field: 3, label: "Add" },
        alternate: (!suggestion.is_empty()).then_some(forms::Button { field: 4, label: suggestion }),
        cancel: forms::Button { field: 5, label: "Cancel" },
    }
}

fn suggestion(p: &Prompt) -> String {
    p.tunnel_form.as_ref().and_then(|f| f.suggestion).map_or_else(String::new, |port| format!("Use {port}"))
}

pub(super) fn draw_form(p: &Prompt, screen: Rect, buf: &mut Buffer) -> Option<Cursor> {
    let form = p.tunnel_form.as_ref()?;
    let suggestion = suggestion(p);
    let buttons = buttons(&suggestion);
    let layout = forms::FormLayout::new(screen, 3, "", p.error.as_deref(), &buttons);
    overlays::frame(layout.area, "Add tunnel", crate::theme::current().accent, buf);
    let server = forms::draw_text_field(
        layout.fields[0],
        "Server    ",
        &form.server,
        form.server_cursor,
        "Name or user@host",
        form.field == 0,
        buf,
    );
    let remote = forms::draw_text_field(
        layout.fields[1],
        "Remote    ",
        &p.value,
        p.cursor,
        "localhost:5432",
        form.field == 1,
        buf,
    );
    let local = forms::draw_text_field(
        layout.fields[2],
        "Local     ",
        &form.local,
        form.local_cursor,
        "Local port",
        form.field == 2,
        buf,
    );
    forms::draw_message(&layout, p.scroll, buf);
    let verb = match form.field {
        4 if !suggestion.is_empty() => suggestion.as_str(),
        5 => "Cancel",
        _ => "Add",
    };
    forms::draw_buttons(&layout, &buttons, form.field, None, verb, buf);
    server.or(remote).or(local)
}

impl Server {
    pub(super) fn tunnel_picker(&self) -> Picker {
        let mut items: Vec<_> = self
            .tunnels
            .iter()
            .map(|t| {
                let detail = match &t.error {
                    Some(error) if t.state == TunnelState::Down => format!("Error: {error}"),
                    _ => t.state.label().into(),
                };
                PickItem::new(
                    t.spec.describe(),
                    detail,
                    PickAction::Tunnel { id: t.id, retry: t.state == TunnelState::Down },
                )
            })
            .collect();
        items.push(PickItem::new("+ Add tunnel", "", PickAction::AddTunnel));
        let mut picker = Picker::new(PickerKind::Tunnels, "Tunnels", items);
        picker.two_line = true;
        picker
    }

    pub(super) fn refresh_tunnel_pickers(&mut self) {
        let clients: Vec<_> = self
            .clients
            .iter_mut()
            .filter_map(|(&id, c)| {
                let Mode::Picker(p) = &mut c.mode else { return None };
                if p.kind != PickerKind::Tunnels {
                    return None;
                }
                let action = p.actions.and_then(|at| p.action_keys().get(at).map(|(_, label)| *label));
                let target = p.current().and_then(|i| match i.action {
                    PickAction::Tunnel { id, .. } => Some(id),
                    _ => None,
                });
                let menu = p.actions.take();
                let state = p.state();
                p.actions = menu;
                Some((id, state, target, action))
            })
            .collect();
        for (id, state, target, action) in clients {
            let mut picker = self.tunnel_picker();
            picker.restore(state);
            let selected = picker.current().and_then(|i| match i.action {
                PickAction::Tunnel { id, .. } => Some(id),
                _ => None,
            });
            if selected == target {
                picker.actions = action.and_then(|label| picker.action_keys().iter().position(|(_, l)| *l == label));
            }
            self.clients.get_mut(&id).unwrap().mode = Mode::Picker(picker);
        }
    }

    pub(super) fn refresh_tunnel_results(&mut self) {
        let mut changed = Vec::new();
        for (&id, c) in &mut self.clients {
            if let Mode::TunnelDetails(details) = &mut c.mode {
                let seconds = self.tunnels.iter().find(|t| t.id == details.id).and_then(|t| t.retry_seconds());
                if details.retry_seconds != seconds {
                    details.retry_seconds = seconds;
                    changed.push(id);
                }
            }
        }
        for id in changed {
            self.mark_dirty_client(id);
        }
    }

    pub(super) fn add_tunnel_form(&self, from: Option<&Picker>, server: &str) -> Prompt {
        let mut p = Prompt::new("Add tunnel", "", PromptAction::AddTunnel);
        let form = p.tunnel_form.as_mut().unwrap();
        form.server = server.into();
        form.server_cursor = server.chars().count();
        form.field = usize::from(!server.is_empty());
        form.back = from.and_then(Picker::form_back);
        p
    }

    pub(super) fn on_tunnel_form(&mut self, client: ClientId, mut p: Prompt, event: InputEvent) -> Mode {
        let suggestion = suggestion(&p);
        let buttons = buttons(&suggestion);
        let layout = forms::FormLayout::new(self.client_screen(client), 3, "", p.error.as_deref(), &buttons);
        let form = p.tunnel_form.as_mut().unwrap();
        let before = (p.value.clone(), form.local.clone(), form.server.clone());
        let outcome =
            forms::FormState { field: &mut form.field, scroll: &mut p.scroll, error: &mut p.error, pending: &mut None }
                .handle(
                    event,
                    &mut [
                        forms::Field {
                            id: 0,
                            area: layout.fields[0],
                            value: forms::FieldValue::Text { value: &mut form.server, cursor: &mut form.server_cursor },
                        },
                        forms::Field {
                            id: 1,
                            area: layout.fields[1],
                            value: forms::FieldValue::Text { value: &mut p.value, cursor: &mut p.cursor },
                        },
                        forms::Field {
                            id: 2,
                            area: layout.fields[2],
                            value: forms::FieldValue::Text { value: &mut form.local, cursor: &mut form.local_cursor },
                        },
                    ],
                    &buttons,
                    &layout,
                );
        if form.local != before.1 {
            form.local_edited = true;
        }
        if p.value != before.0 && !form.local_edited {
            form.local = net::parse_remote(p.value.trim())
                .ok()
                .filter(|(_, p)| *p != 0)
                .map_or_else(String::new, |(_, port)| port.to_string());
            form.local_cursor = form.local.len();
        }
        if (p.value.as_str(), form.local.as_str(), form.server.as_str())
            != (before.0.as_str(), before.1.as_str(), before.2.as_str())
        {
            form.suggestion = None;
            p.scroll = 0;
        }
        match outcome {
            forms::FormEvent::Cancel => self.form_back(client, form.back.take()),
            forms::FormEvent::Submit if form.field == 4 && form.suggestion.is_some() => {
                form.local = form.suggestion.take().unwrap().to_string();
                form.local_cursor = form.local.len();
                form.local_edited = true;
                form.field = 3;
                p.error = None;
                p.scroll = 0;
                Mode::Prompt(p)
            }
            forms::FormEvent::Submit => self.submit_tunnel_form(p),
            _ => Mode::Prompt(p),
        }
    }

    pub(super) fn submit_tunnel_form(&mut self, mut p: Prompt) -> Mode {
        let form = p.tunnel_form.as_mut().unwrap();
        form.suggestion = None;
        p.scroll = 0;
        let result = (|| {
            let server = form.server.trim();
            if server.is_empty() {
                anyhow::bail!("Enter a server name or address.");
            }
            let (_, remote) = net::parse_remote(p.value.trim()).map_err(|e| {
                anyhow::anyhow!("Enter Remote as a port or host:port, for example localhost:5432.\n{e:#}")
            })?;
            if remote == 0 {
                anyhow::bail!("Remote port must be between 1 and 65535.");
            }
            let local = form
                .local
                .trim()
                .parse::<u16>()
                .ok()
                .filter(|p| *p != 0)
                .ok_or_else(|| anyhow::anyhow!("Local port must be between 1 and 65535."))?;
            let taken: Vec<_> = self.tunnels.iter().map(|t| t.spec.local).collect();
            let available = if taken.contains(&local) {
                Err(format!("Port {local} is already used by a tunnel."))
            } else {
                net::port_free(local)
            };
            if let Err(error) = available {
                form.suggestion = net::free_port(local, &taken);
                anyhow::bail!(error);
            }
            self.add_tunnel(server, p.value.trim(), Some(local))
        })();
        match result {
            Ok(tunnel) => {
                let mut picker = self.tunnel_picker();
                picker.close_to = form.back.take().map(Box::new);
                picker.select(|i| matches!(i.action, PickAction::Tunnel { id, .. } if id == tunnel.id));
                Mode::Picker(picker)
            }
            Err(error) => {
                p.error = Some(format!("{error:#}"));
                Mode::Prompt(p)
            }
        }
    }

    pub(super) fn tunnel_details(&self, id: u32, back: Option<PickerState>) -> Mode {
        Mode::TunnelDetails(TunnelDetails {
            id,
            field: 2,
            scroll: 0,
            back,
            retry_seconds: self.tunnels.iter().find(|t| t.id == id).and_then(|t| t.retry_seconds()),
        })
    }

    pub(super) fn tunnel_action(&mut self, id: u32, retry: bool, back: Option<PickerState>) -> Mode {
        if retry {
            self.retry_tunnel(id);
            self.tunnel_details(id, back)
        } else {
            if let Some(t) = self.tunnels.iter().find(|t| t.id == id) {
                let _ = self.remove_tunnel(&t.spec.local.to_string());
            }
            self.form_back(0, back)
        }
    }

    fn tunnel_result(&self, id: u32) -> (String, String, bool, bool) {
        let Some(t) = self.tunnels.iter().find(|t| t.id == id) else {
            return ("Tunnel".into(), "Stopped".into(), false, false);
        };
        let mut message = format!("{}\n{}", t.spec.describe(), t.state.label());
        if t.state == TunnelState::Down {
            if let Some(error) = &t.error {
                message.push_str(&format!("\n{error}"));
            }
            if let Some(seconds) = t.retry_seconds() {
                message.push_str(&format!("\nRetrying in {seconds}s."));
            }
        }
        (format!("Tunnel · {}", t.spec.server), message, t.state == TunnelState::Down, true)
    }

    pub(super) fn draw_tunnel_details(&self, details: &TunnelDetails, screen: Rect, buf: &mut Buffer) {
        let (title, message, retry, exists) = self.tunnel_result(details.id);
        let buttons = result_buttons(retry, exists);
        let layout = result_layout(screen, &message, retry, exists);
        overlays::frame(layout.area, &title, crate::theme::current().accent, buf);
        let theme = crate::theme::current();
        let color = match self.tunnels.iter().find(|t| t.id == details.id).map(|t| t.state) {
            Some(TunnelState::Up) => theme.done,
            Some(TunnelState::Connecting) => theme.working,
            Some(TunnelState::Down) => theme.blocked,
            None => theme.dim,
        };
        forms::draw_message(&layout, details.scroll, buf);
        buf.set_style(layout.body, Style::default().fg(color));
        let field = if details.field == 0 && !retry { 2 } else { details.field };
        let verb = match field {
            0 if retry => "Retry now",
            0 | 1 if exists => "Stop",
            _ => "Close",
        };
        forms::draw_buttons(&layout, &buttons, field, (!exists).then_some(0), verb, buf);
    }

    pub(super) fn on_tunnel_details(
        &mut self,
        client: ClientId,
        mut details: TunnelDetails,
        event: InputEvent,
    ) -> Mode {
        let (_, message, retry, exists) = self.tunnel_result(details.id);
        let buttons = result_buttons(retry, exists);
        let layout = result_layout(self.client_screen(client), &message, retry, exists);
        if details.field == 0 && !retry {
            details.field = 2;
        }
        let outcome = forms::FormState {
            field: &mut details.field,
            scroll: &mut details.scroll,
            error: &mut None,
            pending: &mut None,
        }
        .handle(event, &mut [], &buttons, &layout);
        match outcome {
            forms::FormEvent::Cancel => self.form_back(client, details.back),
            forms::FormEvent::Submit if !exists => self.form_back(client, details.back),
            forms::FormEvent::Submit => self.tunnel_action(details.id, retry && details.field == 0, details.back),
            _ => Mode::TunnelDetails(details),
        }
    }
}

fn result_layout(screen: Rect, message: &str, retry: bool, exists: bool) -> forms::FormLayout {
    let (message, error) = if retry { ("", Some(message)) } else { (message, None) };
    // Fix the height and action slots so a queued Retry click never hits Stop after a state change,
    // including when the buttons stack vertically.
    let mut layout = forms::FormLayout::with_height(
        screen,
        0,
        message,
        error,
        &result_buttons(true, true),
        Some(screen.height.saturating_sub(2)),
    );
    if !retry {
        layout.primary = if exists { layout.alternate.unwrap() } else { layout.cancel };
        layout.alternate = None;
    }
    layout
}

fn result_buttons(retry: bool, exists: bool) -> forms::Buttons<'static> {
    forms::Buttons {
        primary: forms::Button {
            field: if retry { 0 } else { 1 },
            label: if retry {
                "Retry now"
            } else if exists {
                "Stop"
            } else {
                "Close"
            },
        },
        alternate: retry.then_some(forms::Button { field: 1, label: "Stop" }),
        cancel: forms::Button { field: 2, label: "Close" },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{Key, KeySpec, MouseButton, MouseEvent, MouseKind};

    fn key(spec: &str) -> InputEvent {
        let k = KeySpec::parse(spec).unwrap();
        InputEvent::Key(Key { code: k.code, mods: k.mods, raw: Vec::new() })
    }

    fn click(area: Rect) -> InputEvent {
        InputEvent::Mouse(MouseEvent {
            kind: MouseKind::Press(MouseButton::Left),
            col: area.x,
            row: area.y,
            mods: Default::default(),
        })
    }

    fn prompt(mode: Mode) -> Prompt {
        let Mode::Prompt(p) = mode else { panic!("expected the form") };
        p
    }

    #[test]
    fn tunnel_cancel_and_result_close_return_to_the_origin() {
        let (tx, _rx) = crate::server::event_channel(32);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        for kind in [PickerKind::Tunnels, PickerKind::Ssh] {
            for origin in 0..3 {
                let mut list = if kind == PickerKind::Ssh { server.ssh_picker() } else { server.tunnel_picker() };
                list.query = "db".into();
                list.cursor = 1;
                list.refilter();
                list.standalone_actions = origin == 1;
                let from = (origin != 2).then_some(&list);
                for leave in 0..3 {
                    let mut p = server.add_tunnel_form(from, "db");
                    let layout = forms::FormLayout::new(server.client_screen(client), 3, "", None, &buttons(""));
                    let event = match leave {
                        0 => key("esc"),
                        1 => click(layout.cancel),
                        _ => {
                            p.tunnel_form.as_mut().unwrap().field = 5;
                            key("enter")
                        }
                    };
                    forms::assert_return(server.on_tunnel_form(client, p, event), from);
                    for state in [TunnelState::Connecting, TunnelState::Up, TunnelState::Down] {
                        server.tunnels = vec![net::test_tunnel(42, 15432, state)];
                        let Mode::TunnelDetails(details) = server.tunnel_details(42, from.and_then(Picker::form_back))
                        else {
                            panic!()
                        };
                        let (_, message, retry, exists) = server.tunnel_result(42);
                        let layout = result_layout(server.client_screen(client), &message, retry, exists);
                        let event = match leave {
                            0 => key("esc"),
                            1 => click(layout.cancel),
                            _ => key("enter"),
                        };
                        forms::assert_return(server.on_tunnel_details(client, details, event), from);
                        assert_eq!(server.tunnels.len(), 1);
                        server.tunnels.clear();
                    }
                }
            }
        }
    }

    #[test]
    fn list_add_is_pinned_and_server_shortcut_preserves_the_return_selection() {
        for (cols, rows) in [(80, 24), (124, 36)] {
            let (tx, _rx) = crate::server::event_channel(32);
            let mut server = crate::server::tests::server(tx);
            let (client, _peer) = crate::server::tests::client(&mut server);
            let c = server.clients.get_mut(&client).unwrap();
            (c.cols, c.rows) = (cols, rows);
            let mut picker = server.tunnel_picker();
            picker.query = "no match".into();
            picker.refilter();
            assert!(matches!(picker.current().unwrap().action, PickAction::AddTunnel));
            assert_eq!(picker.action_keys(), [("enter", "Add")]);
            for mouse in [false, true] {
                let picker = server.tunnel_picker();
                let area = overlays::picker_layout(&picker, server.client_screen(client)).list;
                let input = if mouse { format!("\x1b[<0;{};{}M", area.x + 1, area.y + 1) } else { "\r".into() };
                server.clients.get_mut(&client).unwrap().mode = Mode::Picker(picker);
                server.client_input(client, input.as_bytes());
                let p = prompt(std::mem::replace(&mut server.clients.get_mut(&client).unwrap().mode, Mode::Normal));
                assert!(p.tunnel_form.as_ref().unwrap().server.is_empty());
            }
            let mut inv = crate::servers::Inventory::empty();
            inv.servers.push(crate::servers::Server { name: "db".into(), host: "db.lan".into(), ..Default::default() });
            server.known_servers = crate::servers::known(&inv, &[], &[]);
            let mut picker = server.ssh_picker();
            picker.query = "db".into();
            picker.refilter();
            picker.select(|i| matches!(i.action, PickAction::Ssh(_)));
            assert!(picker.action_keys().contains(&("ctrl+o", "Add tunnel")));
            server.clients.get_mut(&client).unwrap().mode = Mode::Picker(picker);
            server.client_input(client, b"\x0f");
            let p = prompt(std::mem::replace(&mut server.clients.get_mut(&client).unwrap().mode, Mode::Normal));
            assert_eq!(p.tunnel_form.as_ref().unwrap().server, "db");
            assert_eq!(p.tunnel_form.as_ref().unwrap().field, 1);
            let Mode::Picker(back) = server.on_tunnel_form(client, p, key("esc")) else { panic!() };
            assert_eq!(back.query, "db");
            assert!(matches!(&back.current().unwrap().action, PickAction::Ssh(s) if s == "db"));
            assert!(server.tunnels.is_empty());
            assert!(server.panes.is_empty());
        }
    }

    #[test]
    fn local_follows_remote_until_edited_and_errors_keep_all_values() {
        let (tx, _rx) = crate::server::event_channel(32);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        let mut p = server.add_tunnel_form(None, "db");
        p = prompt(server.on_tunnel_form(client, p, InputEvent::Paste(b"[::1]:5432".to_vec())));
        assert_eq!(p.tunnel_form.as_ref().unwrap().local, "5432");
        for event in [
            key("tab"),
            key("ctrl+u"),
            InputEvent::Paste(b"15432".to_vec()),
            key("shift+tab"),
            key("ctrl+u"),
            InputEvent::Paste(b"db.internal:5433".to_vec()),
        ] {
            p = prompt(server.on_tunnel_form(client, p, event));
        }
        assert_eq!(p.tunnel_form.as_ref().unwrap().local, "15432");
        for (remote, local, expected) in [
            ("no-port", "15432", "not a port"),
            ("0", "15432", "Remote port"),
            ("5432", "0", "Local port"),
            ("5432", "65536", "Local port"),
        ] {
            p.value = remote.into();
            p.tunnel_form.as_mut().unwrap().local = local.into();
            p = prompt(server.submit_tunnel_form(p));
            assert!(p.error.as_deref().unwrap().contains(expected));
            assert_eq!(p.value, remote);
            assert_eq!(p.tunnel_form.as_ref().unwrap().local, local);
            assert_eq!(p.tunnel_form.as_ref().unwrap().server, "db");
            assert!(server.tunnels.is_empty());
        }
    }

    #[test]
    fn busy_and_reserved_ports_offer_an_explicit_change_without_starting_a_tunnel() {
        let (tx, _rx) = crate::server::event_channel(32);
        let mut server = crate::server::tests::server(tx);
        let busy = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = busy.local_addr().unwrap().port();
        for reserved in [false, true] {
            if reserved {
                server.tunnels.push(net::test_tunnel(42, port, TunnelState::Connecting));
            }
            let mut p = server.add_tunnel_form(Some(&server.tunnel_picker()), "db");
            p.value = "5432".into();
            p.tunnel_form.as_mut().unwrap().local = port.to_string();
            p = prompt(server.submit_tunnel_form(p));
            assert!(p.error.as_deref().unwrap().contains(&port.to_string()));
            let suggested = p.tunnel_form.as_ref().unwrap().suggestion.unwrap();
            assert_ne!(suggested, port);
            assert_eq!(p.tunnel_form.as_ref().unwrap().local, port.to_string());
            assert_eq!(p.value, "5432");
            assert_eq!(server.tunnels.len(), usize::from(reserved));
        }
    }

    #[test]
    fn suggested_port_buttons_only_edit_local_and_long_errors_scroll_at_both_sizes() {
        for (cols, rows) in [(80, 24), (124, 36)] {
            for mouse in [false, true] {
                let (tx, _rx) = crate::server::event_channel(32);
                let mut server = crate::server::tests::server(tx);
                let (client, _peer) = crate::server::tests::client(&mut server);
                let c = server.clients.get_mut(&client).unwrap();
                (c.cols, c.rows) = (cols, rows);
                let screen = server.client_screen(client);
                let mut p = server.add_tunnel_form(Some(&server.tunnel_picker()), "db");
                p.value = "localhost:5432".into();
                let form = p.tunnel_form.as_mut().unwrap();
                form.local = "5432".into();
                form.suggestion = Some(10000);
                p.error = Some("Port 5432 is in use.".into());
                let mut buf = Buffer::empty(screen);
                draw_form(&p, screen, &mut buf);
                let text: String = crate::testing::buffer_text(&buf);
                for expected in ["Server    db", "Remote    localhost:5432", "Local     5432", "Use 10000"] {
                    assert!(text.contains(expected), "{text}");
                }
                let layout = forms::FormLayout::new(screen, 3, "", p.error.as_deref(), &buttons("Use 10000"));
                assert_eq!(buf[layout.body.as_position()].fg, crate::theme::current().blocked);
                let event = if mouse {
                    click(layout.alternate.unwrap())
                } else {
                    p.tunnel_form.as_mut().unwrap().field = 4;
                    key("enter")
                };
                p = prompt(server.on_tunnel_form(client, p, event));
                assert_eq!(p.tunnel_form.as_ref().unwrap().local, "10000");
                assert_eq!(p.tunnel_form.as_ref().unwrap().server, "db");
                assert_eq!(p.value, "localhost:5432");
                assert!(server.tunnels.is_empty(), "Use only changes the field");
                p.error = Some("A long connection error. ".repeat(300) + "Final detail.");
                let layout = forms::FormLayout::new(screen, 3, "", p.error.as_deref(), &buttons(""));
                assert!(layout.max_scroll > 0);
                while p.scroll < layout.max_scroll {
                    p = prompt(server.on_tunnel_form(client, p, key("pagedown")));
                }
                let mut buf = Buffer::empty(screen);
                draw_form(&p, screen, &mut buf);
                let text: String = crate::testing::buffer_text(&buf);
                for expected in ["Final detail.", "PgUp/PgDn", "Cancel", "Local"] {
                    assert!(text.contains(expected), "{text}");
                }
                let event = if mouse { click(layout.cancel) } else { key("esc") };
                assert!(matches!(server.on_tunnel_form(client, p, event), Mode::Picker(_)));
            }
        }
    }

    #[test]
    fn refreshing_a_filtered_list_closes_actions_when_the_selected_tunnel_changes() {
        let (tx, _rx) = crate::server::event_channel(32);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        for (id, port) in [(42, 15432), (43, 15433)] {
            let mut t = net::test_tunnel(id, port, TunnelState::Down);
            t.error = Some("Cannot reach db".into());
            server.tunnels.push(t);
        }
        let mut picker = server.tunnel_picker();
        picker.query = "Error".into();
        picker.refilter();
        assert!(matches!(picker.current().unwrap().action, PickAction::Tunnel { id: 42, .. }));
        server.clients.get_mut(&client).unwrap().mode = Mode::Picker(picker);
        server.client_input(client, b"\t\x1b[B\x1b[B");
        let Mode::Picker(picker) = &server.clients[&client].mode else { panic!() };
        assert_eq!(picker.action_keys()[picker.actions.unwrap()].1, "Stop");
        server.tunnels[0].state = TunnelState::Connecting;
        server.refresh_tunnel_pickers();
        let Mode::Picker(picker) = &server.clients[&client].mode else { panic!() };
        assert!(matches!(picker.current().unwrap().action, PickAction::Tunnel { id: 43, .. }));
        assert!(picker.actions.is_none(), "Stop must not carry over to another tunnel");
        server.client_input(client, b"\r");
        assert!(matches!(&server.clients[&client].mode, Mode::TunnelDetails(d) if d.id == 43));
        assert_eq!(server.tunnels.iter().map(|t| t.id).collect::<Vec<_>>(), [42, 43]);
    }

    #[test]
    fn a_retry_click_after_the_state_changes_cannot_stop_the_tunnel() {
        for (cols, rows) in [(32, 24), (80, 24), (124, 36)] {
            for state in [TunnelState::Connecting, TunnelState::Up] {
                let (tx, _rx) = crate::server::event_channel(32);
                let mut server = crate::server::tests::server(tx);
                let (client, _peer) = crate::server::tests::client(&mut server);
                server.bottom_bar = false;
                let c = server.clients.get_mut(&client).unwrap();
                (c.cols, c.rows) = (cols, rows);
                let mut t = net::test_tunnel(42, 15432, TunnelState::Down);
                t.spec.server.clear();
                server.tunnels.push(t);
                // Fail before starting SSH to set the automatic retry countdown.
                server.retry_tunnel(42);
                server.tunnels[0].spec.server = "db".into();
                server.tunnels[0].error = Some("Cannot reach db".into());
                assert!(server.tunnels[0].retry_seconds().is_some());
                let Mode::TunnelDetails(mut details) = server.tunnel_details(42, None) else { panic!() };
                let screen = server.client_screen(client);
                let (_, message, _, _) = server.tunnel_result(42);
                let layout = result_layout(screen, &message, true, true);
                for message in ["Error".into(), "Cannot reach db. ".repeat(300)] {
                    let changed = result_layout(screen, &message, true, true);
                    assert_eq!(changed.primary, layout.primary);
                    assert_eq!(changed.alternate, layout.alternate);
                    assert_eq!(changed.cancel, layout.cancel);
                    assert_eq!(changed.area, layout.area);
                }
                let stop = layout.alternate.unwrap();
                let close = layout.cancel;
                let mut buf = Buffer::empty(screen);
                server.draw_tunnel_details(&details, screen, &mut buf);
                assert_eq!(buf[(layout.primary.x + 1, layout.primary.y)].symbol(), "R");
                server.tunnels[0].state = state;
                for _ in 0..2 {
                    let Mode::TunnelDetails(next) = server.on_tunnel_details(client, details, click(layout.primary))
                    else {
                        panic!("a stale Retry click must leave the details open");
                    };
                    details = next;
                    assert_eq!(server.tunnels.len(), 1);
                    assert_eq!(server.tunnels[0].id, 42);
                    assert_eq!(server.tunnels[0].state, state);
                }
                let (_, message, _, _) = server.tunnel_result(42);
                let layout = result_layout(screen, &message, false, true);
                assert_eq!(layout.primary, stop);
                assert_eq!(layout.cancel, close);
                server.draw_tunnel_details(&details, screen, &mut buf);
                assert_eq!(buf[(stop.x + 1, stop.y)].symbol(), "S");
                assert!(matches!(server.on_tunnel_details(client, details, click(layout.primary)), Mode::Normal));
                assert!(server.tunnels.is_empty(), "Stop still works at its own coordinates");
            }
        }
    }

    #[test]
    fn retry_buttons_and_keys_keep_the_same_tunnel_and_its_countdown() {
        for mouse in [false, true] {
            let (tx, _rx) = crate::server::event_channel(32);
            let mut server = crate::server::tests::server(tx);
            let (client, _peer) = crate::server::tests::client(&mut server);
            let mut t = net::test_tunnel(42, 15432, TunnelState::Down);
            // An invalid saved target fails before starting SSH, without using the network.
            t.spec.server.clear();
            t.error = Some("Unknown server".into());
            server.tunnels.push(t);
            let Mode::TunnelDetails(mut details) = server.tunnel_details(42, None) else { panic!() };
            let (_, message, _, _) = server.tunnel_result(42);
            let layout = result_layout(server.client_screen(client), &message, true, true);
            let event = if mouse {
                click(layout.primary)
            } else {
                details.field = 0;
                key("enter")
            };
            let Mode::TunnelDetails(details) = server.on_tunnel_details(client, details, event) else { panic!() };
            assert_eq!(server.tunnels.len(), 1);
            assert_eq!(server.tunnels[0].spec.local, 15432);
            assert!(server.tunnel_result(42).1.contains("Retrying in 4s."));
            server.clients.get_mut(&client).unwrap().mode = Mode::TunnelDetails(details);
            server.clients.get_mut(&client).unwrap().dirty = super::super::Dirty::No;
            if let Mode::TunnelDetails(d) = &mut server.clients.get_mut(&client).unwrap().mode {
                d.retry_seconds = None;
            }
            server.refresh_tunnel_results();
            assert!(matches!(server.clients[&client].dirty, super::super::Dirty::Yes));
            server.clients.get_mut(&client).unwrap().mode = Mode::Picker(server.tunnel_picker());
            server.client_input(client, b"\x12");
            assert!(matches!(server.clients[&client].mode, Mode::TunnelDetails(_)));
            assert!(server.tunnels[0].retry_seconds().is_some_and(|s| s > 4));
            let Mode::TunnelDetails(mut details) = server.tunnel_details(42, None) else { panic!() };
            details.field = 1;
            server.on_tunnel_details(client, details, key("enter"));
            server.check_tunnels();
            assert!(server.tunnels.is_empty());
        }
    }

    #[test]
    fn results_are_live_close_keeps_running_and_stop_does_not_target_a_replacement() {
        for screen in [Rect::new(0, 0, 80, 24), Rect::new(0, 0, 124, 36)] {
            let (tx, _rx) = crate::server::event_channel(32);
            let mut server = crate::server::tests::server(tx);
            let (client, _peer) = crate::server::tests::client(&mut server);
            let c = server.clients.get_mut(&client).unwrap();
            (c.cols, c.rows) = (screen.width, screen.height);
            server.tunnels.push(net::test_tunnel(42, 15432, TunnelState::Connecting));
            server.clients.get_mut(&client).unwrap().mode = Mode::Picker(server.tunnel_picker());
            for (state, label) in
                [(TunnelState::Connecting, "Connecting"), (TunnelState::Up, "Ready"), (TunnelState::Down, "Error")]
            {
                server.tunnels[0].state = state;
                server.tunnels[0].error = (state == TunnelState::Down).then(|| "Can't reach db.".into());
                server.refresh_tunnel_pickers();
                let Mode::Picker(picker) = &server.clients[&client].mode else { panic!() };
                assert!(picker.current().unwrap().detail.contains(label));
                assert_eq!(picker.action_keys().contains(&("ctrl+r", "Retry now")), state == TunnelState::Down);
                let Mode::TunnelDetails(details) = server.tunnel_details(42, Some(picker.state())) else { panic!() };
                let mut buf = Buffer::empty(screen);
                server.draw_tunnel_details(&details, screen, &mut buf);
                let text: String = crate::testing::buffer_text(&buf);
                for expected in [label, "localhost:15432", "Enter Close", "Stop"] {
                    assert!(text.contains(expected), "{text}");
                }
                assert_eq!(text.contains("Retry now"), state == TunnelState::Down);
                let (_, message, retry, exists) = server.tunnel_result(42);
                let layout = result_layout(screen, &message, retry, exists);
                assert_eq!(buf[layout.cancel.as_position()].bg, crate::theme::current().accent);
                assert_eq!(
                    buf[layout.body.as_position()].fg,
                    match state {
                        TunnelState::Up => crate::theme::current().done,
                        TunnelState::Connecting => crate::theme::current().working,
                        TunnelState::Down => crate::theme::current().blocked,
                    }
                );
                assert!(matches!(server.on_tunnel_details(client, details, key("enter")), Mode::Picker(_)));
                assert_eq!(server.tunnels.len(), 1);
            }
            let Mode::TunnelDetails(mut details) = server.tunnel_details(42, None) else { panic!() };
            details.field = 0;
            if let Mode::Picker(p) = &mut server.clients.get_mut(&client).unwrap().mode {
                p.actions = Some(1);
            }
            server.tunnels[0].state = TunnelState::Up;
            server.refresh_tunnel_pickers();
            let Mode::Picker(p) = &server.clients[&client].mode else { panic!() };
            assert!(p.actions.is_none());
            assert!(!p.action_keys().contains(&("ctrl+r", "Retry now")));
            assert!(matches!(server.on_tunnel_details(client, details, key("enter")), Mode::Normal));
            assert_eq!(server.tunnels.len(), 1, "a focused Retry must not become Stop");
            let Mode::TunnelDetails(details) = server.tunnel_details(42, None) else { panic!() };
            server.remove_tunnel("15432").unwrap();
            server.tunnels.push(net::test_tunnel(43, 15432, TunnelState::Connecting));
            server.tunnel_action(42, false, None);
            assert_eq!(server.tunnels[0].id, 43);
            let mut buf = Buffer::empty(screen);
            server.draw_tunnel_details(&details, screen, &mut buf);
            let text: String = crate::testing::buffer_text(&buf);
            assert!(text.contains("Stopped") && !text.contains(" Stop "), "{text}");
            let Mode::TunnelDetails(details) = server.tunnel_details(43, None) else { panic!() };
            let (_, message, _, _) = server.tunnel_result(43);
            let layout = result_layout(server.client_screen(client), &message, false, true);
            assert!(matches!(server.on_tunnel_details(client, details, click(layout.primary)), Mode::Normal));
            assert!(server.tunnels.is_empty());
            let Mode::Picker(picker) = &server.clients[&client].mode else { panic!() };
            assert_eq!(picker.items.len(), 1, "removing the last tunnel refreshes other lists");
        }
    }
}
