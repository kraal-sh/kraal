//! Save server entries with the shared management form before connecting.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::ui::{Mode, Picker, PickerState, Prompt};
use super::{ClientId, Server, forms, overlays};
use crate::ansi::Cursor;
use crate::input::InputEvent;
use crate::servers::{self, Inventory};
use crate::theme::NoticeKind;

type ServerPrompt = Prompt<Box<ServerForm>>;

#[derive(Default)]
pub struct ServerForm {
    address: String,
    cursor: usize,
    field: usize,
    pending: Option<u32>,
    back: Option<PickerState>,
    suggested: Option<servers::Server>,
    named: bool,
}

impl ServerPrompt {
    fn state(&mut self) -> forms::FormState<'_> {
        forms::FormState {
            field: &mut self.data.field,
            scroll: &mut self.scroll,
            error: &mut self.error,
            pending: &mut self.data.pending,
        }
    }
}

fn buttons(p: &ServerPrompt) -> forms::Buttons<'static> {
    let pending = p.data.pending.is_some();
    forms::Buttons {
        primary: forms::Button { field: 2, label: if p.error.is_some() { "Retry" } else { "Add" } },
        alternate: None,
        cancel: forms::Button { field: 3, label: if pending { "Close" } else { "Cancel" } },
    }
}

fn form_layout(p: &ServerPrompt, screen: Rect) -> forms::FormLayout {
    let (message, error) = if p.data.pending.is_some() { ("Saving server...", None) } else { ("", p.error.as_deref()) };
    forms::FormLayout::new(screen, 2, message, error, &buttons(p))
}

pub(super) fn draw_form(p: &ServerPrompt, screen: Rect, buf: &mut Buffer) -> Option<Cursor> {
    let form = p.data.as_ref();
    let layout = form_layout(p, screen);
    overlays::frame(layout.area, &p.title, crate::theme::current().accent, buf);
    let address = forms::draw_text_field(
        layout.fields[0],
        "Address   ",
        &form.address,
        form.cursor,
        "user@host[:port]",
        form.field == 0,
        buf,
    );
    let name = forms::draw_text_field(layout.fields[1], "Name      ", &p.value, p.cursor, "", form.field == 1, buf);
    forms::draw_message(&layout, p.scroll, buf);
    let buttons = buttons(p);
    let verb = if form.field == 3 { buttons.cancel.label } else { buttons.primary.label };
    forms::draw_buttons(&layout, &buttons, form.field, form.pending, verb, buf);
    address.or(name)
}

fn target(p: &ServerPrompt, known: &[servers::Known]) -> Option<servers::Server> {
    let form = p.data.as_ref();
    let address = form.address.trim();
    // Keep keys and jump hosts from a discovered command when its address has not been edited.
    form.suggested
        .as_ref()
        .filter(|s| s.address() == address)
        .cloned()
        .or_else(|| known.iter().find(|k| k.server.name == address).map(|k| k.server.clone()))
        // A literal address leaves omitted options to SSH, not to another saved entry.
        .or_else(|| servers::resolve(&[], address))
        .filter(|s| {
            !s.destination().starts_with('-')
                && !s.destination().chars().any(|c| c.is_whitespace() || c.is_control())
                && !s.host.contains('@')
                && s.user.as_ref().is_none_or(|u| !u.contains('@'))
                && s.port != Some(0)
        })
}

impl Server {
    pub(super) fn add_server_form(&self, from: Option<&Picker>, suggested: Option<&servers::Server>) -> ServerPrompt {
        let address =
            suggested.map(servers::Server::address).unwrap_or_else(|| from.map_or("", |p| p.query.trim()).into());
        let mut p = Prompt::new("Add server", "", Box::<ServerForm>::default());
        let form = p.data.as_mut();
        form.cursor = address.chars().count();
        form.address = address;
        form.suggested = suggested.cloned();
        form.back = from.and_then(Picker::form_back);
        self.suggest_server_name(&mut p);
        p
    }

    fn suggest_server_name(&self, p: &mut ServerPrompt) {
        if !p.data.named {
            p.value = target(p, &self.known_servers).map_or_else(String::new, |s| s.name.replace(':', "-"));
            p.cursor = p.value.chars().count();
        }
    }

    pub(super) fn on_server_form(&mut self, id: ClientId, mut p: ServerPrompt, event: InputEvent) -> Mode {
        let layout = form_layout(&p, self.client_screen(id));
        let buttons = buttons(&p);
        let before = p.value.clone();
        let form = p.data.as_mut();
        let address = form.address.clone();
        let outcome = forms::FormState {
            field: &mut form.field,
            scroll: &mut p.scroll,
            error: &mut p.error,
            pending: &mut form.pending,
        }
        .handle(
            event,
            &mut [
                forms::Field {
                    id: 0,
                    area: layout.fields[0],
                    value: forms::FieldValue::Text { value: &mut form.address, cursor: &mut form.cursor },
                },
                forms::Field {
                    id: 1,
                    area: layout.fields[1],
                    value: forms::FieldValue::Text { value: &mut p.value, cursor: &mut p.cursor },
                },
            ],
            &buttons,
            &layout,
        );
        if p.value != before {
            form.named = true;
        }
        if form.address != address {
            self.suggest_server_name(&mut p);
        }
        match outcome {
            forms::FormEvent::Cancel => self.form_back(id, p.data.back.take()),
            forms::FormEvent::Submit => self.submit_server_form(id, p),
            _ => Mode::ServerForm(p),
        }
    }

    pub(super) fn submit_server_form(&mut self, id: ClientId, mut p: ServerPrompt) -> Mode {
        if p.data.pending.is_some() {
            return Mode::ServerForm(p);
        }
        let Some(mut entry) = target(&p, &self.known_servers) else {
            p.error = Some("Enter an address such as admin@db.lan or admin@db.lan:2222.".into());
            return Mode::ServerForm(p);
        };
        entry.name = p.value.trim().to_string();
        entry.agents = None;
        if !servers::valid_name(&entry.name) {
            p.error = Some("Use letters, digits, hyphens, underscores or periods for Name.".into());
            return Mode::ServerForm(p);
        }
        let token = self.alloc_id();
        if !p.state().begin(token) {
            return Mode::ServerForm(p);
        }
        p.error = None;
        p.scroll = 0;
        if !self.background(
            move || Inventory::update(|inv| inv.add_named(entry)),
            move |server, result| server.server_form_saved(id, token, result),
        ) {
            p.state().complete(token, Some("Couldn't start saving. Retry.".into()));
        }
        Mode::ServerForm(p)
    }

    fn server_form_saved(&mut self, id: ClientId, token: u32, result: anyhow::Result<(String, bool)>) {
        if result.is_ok() {
            let defs = self.definitions().into_iter().filter_map(|(_, d)| d.ok()).collect::<Vec<_>>();
            self.refresh_known(&defs);
        }
        let active = self
            .clients
            .get(&id)
            .is_some_and(|c| matches!(&c.mode, Mode::ServerForm(p) if p.data.pending == Some(token)));
        if !active {
            self.toast(
                id,
                if result.is_ok() { NoticeKind::Notice } else { NoticeKind::Error },
                match result {
                    Ok((_, true)) => "Server added".into(),
                    Ok((_, false)) => "Server already saved".into(),
                    Err(e) => format!("Couldn't save server: {e:#}. Open Add server to retry."),
                },
            );
            return;
        }
        let Mode::ServerForm(mut p) = std::mem::replace(&mut self.clients.get_mut(&id).unwrap().mode, Mode::Normal)
        else {
            unreachable!()
        };
        let mode = match result {
            Ok((name, added)) => {
                p.state().complete(token, None);
                let mut picker = self.ssh_picker();
                let back = p.data.back.take();
                if let Some(state) = back.clone() {
                    picker.restore(state);
                }
                picker.close_to = back.map(Box::new);
                picker.actions = None;
                picker.reveal(|i| matches!(&i.action, super::ui::PickAction::Ssh(s) if *s == name));
                picker.result(NoticeKind::Notice, if added { "Server added" } else { "Server already saved" })
            }
            Err(e) => {
                p.state().complete(token, Some(format!("Couldn't save server.\n{e:#}\nFix the cause and retry.")));
                Mode::ServerForm(p)
            }
        };
        self.clients.get_mut(&id).unwrap().mode = mode;
        self.mark_dirty();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::{Key, KeySpec, MouseButton, MouseEvent, MouseKind};
    use crate::server::ui::PickAction;

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

    #[test]
    fn late_save_errors_preserve_each_kind_of_input_and_a_new_pending_save() {
        let (tx, _rx) = crate::server::event_channel(32);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        let mut pending = server.add_server_form(None, None);
        assert!(pending.state().begin(43));
        for mode in [
            Mode::Prompt(Prompt::new("Name", "", super::super::ui::PromptAction::RenamePane(0))),
            Mode::ServerForm(pending),
            Mode::TunnelForm(server.add_tunnel_form(None, "db")),
            Mode::WorkspaceForm(server.add_workspace_form(client, None)),
        ] {
            let kind = std::mem::discriminant(&mode);
            server.clients.get_mut(&client).unwrap().mode = mode;
            server.client_input(client, b"draft");
            server.server_form_saved(client, 42, Err(anyhow::anyhow!("late failure")));
            let c = &server.clients[&client];
            assert_eq!(std::mem::discriminant(&c.mode), kind);
            assert_eq!(c.toast.as_ref().unwrap().0, "Couldn't save server: late failure. Open Add server to retry.");
            match &c.mode {
                Mode::Prompt(p) => assert_eq!(p.value, "draft"),
                Mode::ServerForm(p) => {
                    assert_eq!(p.data.pending, Some(43));
                    assert!(p.data.address.is_empty());
                    assert!(p.error.is_none());
                }
                Mode::TunnelForm(p) => assert_eq!(p.value, "draft"),
                Mode::WorkspaceForm(p) => assert!(p.value.ends_with("draft")),
                _ => panic!("input was closed"),
            }
        }
        assert!(server.jobs.is_empty());
    }

    #[test]
    fn pending_server_forms_ignore_mouse_submissions_and_close_without_losing_the_result() {
        let (tx, _rx) = crate::server::event_channel(32);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        for (width, height) in [(20, 12), (32, 12), (80, 24)] {
            let c = server.clients.get_mut(&client).unwrap();
            (c.cols, c.rows) = (width, height);
            let mut p = server.add_server_form(None, None);
            assert!(p.state().begin(42));
            let layout = form_layout(&p, server.client_screen(client));
            let mut buf = Buffer::empty(server.client_screen(client));
            let cursor = draw_form(&p, buf.area, &mut buf).unwrap();
            assert!(buf.area.contains(ratatui::layout::Position::new(cursor.x, cursor.y)));
            for event in [click(layout.primary), key("enter"), click(layout.fields[1]), key("tab")] {
                let Mode::ServerForm(next) = server.on_server_form(client, p, event) else { panic!("keep saving") };
                p = next;
                assert_eq!(p.data.pending, Some(42));
                assert_eq!(p.data.field, 0);
            }
            assert!(matches!(server.on_server_form(client, p, click(layout.cancel)), Mode::Normal));
            server.clients.get_mut(&client).unwrap().mode = Mode::Normal;
            server.server_form_saved(client, 42, Err(anyhow::anyhow!("closed")));
            assert!(matches!(server.clients[&client].mode, Mode::Normal));
        }
        assert!(server.jobs.is_empty());
    }

    #[test]
    fn server_cancel_and_saved_result_close_return_to_the_origin() {
        let (tx, _rx) = crate::server::event_channel(32);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        for origin in 0..3 {
            let mut list = server.ssh_picker();
            list.query = "db".into();
            list.cursor = 1;
            list.refilter();
            list.select(|i| matches!(i.action, super::super::ui::PickAction::AddServer(None)));
            list.standalone_actions = origin == 1;
            let from = (origin != 2).then_some(&list);
            for leave in 0..3 {
                let mut p = server.add_server_form(from, None);
                let layout = form_layout(&p, server.client_screen(client));
                let event = match leave {
                    0 => key("esc"),
                    1 => click(layout.cancel),
                    _ => {
                        p.data.field = buttons(&p).cancel.field;
                        key("enter")
                    }
                };
                forms::assert_return(server.on_server_form(client, p, event), from);
                let mut p = server.add_server_form(from, None);
                p.data.pending = Some(42);
                server.clients.get_mut(&client).unwrap().mode = Mode::ServerForm(p);
                server.server_form_saved(client, 42, Ok(("db".into(), true)));
                server.client_input(client, b"\x1b");
                server.flush_client_input(client);
                let mode = std::mem::replace(&mut server.clients.get_mut(&client).unwrap().mode, Mode::Normal);
                forms::assert_return(mode, from);
            }
        }
    }

    #[test]
    fn add_is_pinned_search_only_filters_and_keyboard_and_mouse_open_the_same_form() {
        for (cols, rows) in [(80, 24), (124, 36)] {
            for mouse in [false, true] {
                let (tx, _rx) = crate::server::event_channel(32);
                let mut server = crate::server::tests::server(tx);
                let (client, _peer) = crate::server::tests::client(&mut server);
                let c = server.clients.get_mut(&client).unwrap();
                (c.cols, c.rows) = (cols, rows);
                let mut picker = server.ssh_picker();
                picker.query = "admin@db.lan:2222".into();
                picker.cursor = picker.query.len();
                picker.refilter();
                assert_eq!(picker.items.len(), 1);
                assert!(matches!(picker.current().unwrap().action, PickAction::AddServer(None)));
                assert_eq!(picker.action_keys(), [("enter", "Add")]);
                let layout = overlays::picker_layout(&picker, server.client_screen(client));
                let input =
                    if mouse { format!("\x1b[<0;{};{}M", layout.list.x + 1, layout.list.y + 1) } else { "\r".into() };
                server.clients.get_mut(&client).unwrap().mode = Mode::Picker(picker);
                server.client_input(client, input.as_bytes());
                let Mode::ServerForm(mut p) =
                    std::mem::replace(&mut server.clients.get_mut(&client).unwrap().mode, Mode::Normal)
                else {
                    panic!("Add form")
                };
                assert_eq!(p.value, "db");
                assert_eq!(p.data.address, "admin@db.lan:2222");
                p.error = Some("A detailed error. ".repeat(300) + "Last detail.");
                let screen = server.client_screen(client);
                let layout = form_layout(&p, screen);
                assert!(layout.max_scroll > 0);
                p.scroll = layout.max_scroll;
                let mut buf = Buffer::empty(screen);
                draw_form(&p, screen, &mut buf);
                assert_eq!(buf[layout.body.as_position()].fg, crate::theme::current().blocked);
                let text = crate::testing::buffer_text(&buf);
                for expected in ["Address", "Name", "Last detail.", "Retry", "Cancel", "PgUp/PgDn"] {
                    assert!(text.contains(expected), "{expected}: {text}");
                }
                let event = if mouse { click(layout.cancel) } else { key("esc") };
                let Mode::Picker(back) = server.on_server_form(client, p, event) else { panic!("return to list") };
                assert_eq!(back.query, "admin@db.lan:2222");
                assert_eq!(back.current().unwrap().label, "+ Add server");
                assert!(server.panes.is_empty());
                assert!(server.jobs.is_empty());
            }
        }
    }

    #[test]
    fn address_suggests_a_name_but_keeps_an_edited_name_and_discovered_options() {
        let (tx, _rx) = crate::server::event_channel(32);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        let mut p = server.add_server_form(None, None);
        for event in [
            InputEvent::Paste(b"admin@db.lan:2222".to_vec()),
            key("tab"),
            key("ctrl+u"),
            InputEvent::Paste("café".as_bytes().to_vec()),
            key("shift+tab"),
            key("ctrl+u"),
            InputEvent::Paste(b"ops@[::1]:22".to_vec()),
        ] {
            let Mode::ServerForm(next) = server.on_server_form(client, p, event) else { panic!() };
            p = next;
        }
        assert_eq!(p.value, "café");
        let s = target(&p, &[]).unwrap();
        assert_eq!(s.address(), "ops@[::1]:22");
        let s = servers::Server {
            identity: Some("~/.ssh/db".into()),
            jump: Some("bastion".into()),
            groups: vec!["discovered".into()],
            ..s
        };
        let mut p = server.add_server_form(None, Some(&s));
        assert!(servers::valid_name(&p.value));
        assert!(target(&p, &[]).unwrap().same_connection(&s));
        assert_eq!(target(&p, &[]).unwrap().groups, s.groups);
        p.data.field = 1;
        let cursor = p.cursor;
        let Mode::ServerForm(p) = server.on_server_form(client, p, key("left")) else { panic!() };
        assert_eq!(p.cursor, cursor - 1, "the suggested name is editable from any position");
        assert!(matches!(server.on_server_form(client, p, key("esc")), Mode::Normal));
        assert!(server.jobs.is_empty());
    }

    #[test]
    fn literal_addresses_keep_omitted_ports_and_saved_names_keep_their_connection() {
        let (tx, _rx) = crate::server::event_channel(32);
        let mut server = crate::server::tests::server(tx);
        let saved =
            servers::Server { name: "box22".into(), host: "box.lan".into(), port: Some(22), ..Default::default() };
        let mut inv = Inventory::empty();
        inv.servers.push(saved.clone());
        server.known_servers = servers::known(&inv, &[], &[]);
        for (address, port, name, added) in [
            ("box.lan", None, "box", true),
            ("box.lan:22", Some(22), "box22", false),
            ("box22", Some(22), "box22", false),
        ] {
            let mut p = server.add_server_form(None, None);
            p.data.address = address.into();
            let s = target(&p, &server.known_servers).unwrap();
            assert_eq!(s.port, port, "{address}");
            assert_eq!(inv.add_named(s).unwrap(), (name.into(), added));
        }
        assert_eq!(inv.servers.len(), 2);
        assert!(inv.get("box22").unwrap().same_connection(&saved));
    }

    #[test]
    fn literal_addresses_do_not_inherit_agent_watching() {
        let (tx, _rx) = crate::server::event_channel(32);
        let mut server = crate::server::tests::server(tx);
        let mut inv = Inventory::empty();
        inv.servers.push(servers::Server {
            name: "box".into(),
            host: "box.lan".into(),
            agents: Some("main".into()),
            ..Default::default()
        });
        server.known_servers = servers::known(&inv, &[], &[]);
        let mut p = server.add_server_form(None, None);
        p.data.address = "box.lan:22".into();
        let mut s = target(&p, &server.known_servers).unwrap();
        s.name = "box22".into();
        assert_eq!(s.agents, None);
        assert_eq!(inv.add_named(s).unwrap(), ("box22".into(), true));
        assert_eq!(inv.get("box").unwrap().agents.as_deref(), Some("main"));
        assert_eq!(inv.get("box22").unwrap().agents_session(), None);
    }

    #[test]
    fn saved_names_find_each_connection_in_either_inventory_order() {
        let (tx, _rx) = crate::server::event_channel(32);
        let server = crate::server::tests::server(tx);
        let first = servers::resolve(&[], "admin@box.lan:22").unwrap();
        for second in [
            servers::Server { name: "key".into(), identity: Some("~/.ssh/other".into()), ..first.clone() },
            servers::Server { name: "jump".into(), jump: Some("bastion".into()), ..first.clone() },
        ] {
            for entries in [[first.clone(), second.clone()], [second.clone(), first.clone()]] {
                let mut inv = Inventory::empty();
                inv.servers.extend(entries.clone());
                let known = servers::known(&inv, &[], &[]);
                for entry in &entries {
                    let mut p = server.add_server_form(None, None);
                    p.data.address = entry.name.clone();
                    let s = target(&p, &known).unwrap();
                    assert!(s.same_connection(entry));
                    assert_eq!(inv.add_named(s).unwrap(), (entry.name.clone(), false));
                }
                assert_eq!(inv.servers.len(), 2);
                for entry in &entries {
                    assert!(inv.get(&entry.name).unwrap().same_connection(entry));
                }
            }
        }
    }

    #[test]
    fn saving_returns_to_the_saved_row_without_connecting_and_late_results_keep_the_current_screen() {
        if !crate::testing::in_own_process(
            "server::server_ui::tests::saving_returns_to_the_saved_row_without_connecting_and_late_results_keep_the_current_screen",
        ) {
            return;
        }
        let (tx, _rx) = crate::server::event_channel(32);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        let s = servers::resolve(&[], "admin@db.lan").unwrap();
        let saved = Inventory::update(|inv| inv.add_named(s.clone())).unwrap();
        let mut picker = server.ssh_picker();
        picker.query = "not-a-match".into();
        picker.refilter();
        let mut p = server.add_server_form(Some(&picker), Some(&s));
        p.data.pending = Some(42);
        server.clients.get_mut(&client).unwrap().mode = Mode::ServerForm(p);
        server.server_form_saved(client, 42, Ok(saved));
        let Mode::Picker(result) = &server.clients[&client].mode else { panic!("saved list") };
        assert_eq!(result.message.as_deref(), Some("Server added"));
        assert!(result.query.is_empty());
        assert!(matches!(&result.current().unwrap().action, PickAction::Ssh(name) if name == "db"));
        assert_eq!(result.action_keys()[0], ("enter", "Connect"));
        let mut p = server.add_server_form(None, Some(&s));
        p.data.pending = Some(43);
        server.clients.get_mut(&client).unwrap().mode = server.on_server_form(client, p, key("esc"));
        server.server_form_saved(client, 43, Ok(("db".into(), false)));
        assert!(matches!(server.clients[&client].mode, Mode::Normal));
        assert!(server.panes.is_empty());
        assert_eq!(Inventory::load().unwrap().servers.len(), 1);
    }

    #[test]
    fn invalid_fields_and_save_failures_keep_values_and_pending_forms_ignore_resubmission() {
        let (tx, _rx) = crate::server::event_channel(32);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        for (address, name, error) in [
            ("", "db", "Enter an address"),
            ("db:bad", "db", "Enter an address"),
            ("-oProxyCommand=x", "db", "Enter an address"),
            ("db:0", "db", "Enter an address"),
            ("db.lan", "", "Use letters"),
            ("db.lan", "bad/name", "Use letters"),
        ] {
            let mut p = server.add_server_form(None, None);
            p.data.address = address.into();
            p.value = name.into();
            let Mode::ServerForm(p) = server.submit_server_form(client, p) else { panic!() };
            assert!(p.error.as_ref().unwrap().contains(error));
            assert_eq!(p.value, name);
            assert_eq!(p.data.address, address);
        }
        let mut p = server.add_server_form(None, Some(&servers::resolve(&[], "admin@db.lan").unwrap()));
        p.data.pending = Some(42);
        for event in [key("enter"), key("tab"), InputEvent::Paste(b"ignored".to_vec())] {
            let Mode::ServerForm(next) = server.on_server_form(client, p, event) else { panic!() };
            p = next;
        }
        assert_eq!(p.value, "db");
        server.clients.get_mut(&client).unwrap().mode = Mode::ServerForm(p);
        server.server_form_saved(client, 41, Err(anyhow::anyhow!("stale")));
        assert!(matches!(&server.clients[&client].mode, Mode::ServerForm(p) if p.error.is_none()));
        server.server_form_saved(client, 42, Err(anyhow::anyhow!("permission denied")));
        let Mode::ServerForm(p) = &server.clients[&client].mode else { panic!() };
        assert!(p.error.as_ref().unwrap().contains("permission denied"));
        assert_eq!(p.value, "db");
        assert_eq!(p.data.address, "admin@db.lan");
        assert!(p.data.pending.is_none());
        assert!(server.jobs.is_empty());
        assert!(server.panes.is_empty());
    }
}
