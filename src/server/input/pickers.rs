//! Lists to choose from: the generic picker, and the workspace, command and pane lists.

use anyhow::Context;

use super::plural;
use crate::agent::AgentState;
use crate::input::{InputEvent, Key, KeyCode, MouseButton, MouseEvent, MouseKind};
use crate::keymap::{Action, Command};
use crate::pane::PaneSpec;
use crate::paths;
use crate::protocol::Placement;
use crate::server::definitions::file_name;
use crate::server::ui::{ConfirmAction, Mode, PickAction, PickItem, Picker, PickerKind, Prompt, PromptAction};
use crate::server::{ClientId, Server};

impl Server {
    pub(super) fn on_picker(&mut self, id: ClientId, mut p: Picker, ev: InputEvent) -> Mode {
        // The list of what can be done with the selected item (Tab).
        if let Some(at) = p.actions {
            let keys = p.action_keys();
            // A click on an action does it; anywhere else closes the list.
            if let InputEvent::Mouse(MouseEvent { kind: MouseKind::Press(MouseButton::Left), col, row, .. }) = ev {
                let layout = crate::server::overlays::picker_layout(&p, self.client_screen(id));
                // The actions are drawn from the top of the list area, scrolled to the selected one.
                let first = at.saturating_sub((layout.list.height as usize).saturating_sub(1));
                let i = layout.row_at(col, row).map(|i| i - layout.start + first);
                p.actions = None;
                if let Some(i) = i.filter(|i| *i < keys.len()) {
                    let spec = crate::input::KeySpec::parse(keys[i].0).expect("picker action keys are valid");
                    let key = Key { code: spec.code, mods: spec.mods, raw: Vec::new() };
                    return self.on_picker(id, p, InputEvent::Key(key));
                }
                return Mode::Picker(p);
            }
            if let InputEvent::Key(k) = ev {
                match k.code {
                    KeyCode::Up => p.actions = Some(at.checked_sub(1).unwrap_or(keys.len() - 1)),
                    KeyCode::Down => p.actions = Some((at + 1) % keys.len()),
                    KeyCode::Enter => {
                        p.actions = None;
                        // The action is its key, pressed in the list.
                        let spec = crate::input::KeySpec::parse(keys[at].0).expect("picker action keys are valid");
                        let key = Key { code: spec.code, mods: spec.mods, raw: Vec::new() };
                        return self.on_picker(id, p, InputEvent::Key(key));
                    }
                    _ => p.actions = None,
                }
            }
            return Mode::Picker(p);
        }
        match ev {
            InputEvent::Key(k) => match (k.code, k.mods.ctrl) {
                (KeyCode::Esc, _) | (KeyCode::Char('c'), true) | (KeyCode::Char('g'), true) => {
                    return if p.kind == PickerKind::AccountReview {
                        Mode::Picker(self.accounts_picker(None))
                    } else {
                        Mode::Normal
                    };
                }
                (KeyCode::Enter, _) => {
                    let Some(item) = p.current().cloned() else { return Mode::Picker(p) };
                    return self.pick(id, &p, item);
                }
                (KeyCode::Tab, _) if p.action_keys().len() > 1 && p.current().is_some() => p.actions = Some(0),
                (KeyCode::Tab, _) => p.move_by(1),
                (KeyCode::Up, _) | (KeyCode::Char('p'), true) | (KeyCode::Char('k'), true) | (KeyCode::BackTab, _) => {
                    p.move_by(-1)
                }
                (KeyCode::Down, _) | (KeyCode::Char('n'), true) | (KeyCode::Char('j'), true) => p.move_by(1),
                (KeyCode::PageUp, _) => p.move_by(-10),
                (KeyCode::PageDown, _) => p.move_by(10),
                (KeyCode::Char('t'), true) if p.kind == PickerKind::Ssh => {
                    if let Some(PickAction::Ssh(spec)) = p.current().map(|i| i.action.clone()) {
                        self.connect(id, &spec, Placement::Tab);
                        return Mode::Normal;
                    }
                }
                (KeyCode::Char('f'), true) if p.kind == PickerKind::Ssh => {
                    if let Some(PickAction::Ssh(name)) = p.current().map(|i| i.action.clone())
                        && let Some(ws) = self.clients.get(&id).map(|c| c.workspace)
                    {
                        let (left, _) = self.files_defaults(id);
                        if let Err(e) = self.open_files(ws, left, format!("{name}:~")) {
                            self.toast(id, format!("{e:#}"));
                        }
                        return Mode::Normal;
                    }
                }
                (KeyCode::Char('t'), true) if p.kind == PickerKind::Agent => {
                    if let Some(PickAction::AgentLaunch { agent, account }) = p.current().map(|i| i.action.clone()) {
                        let ws = self.clients.get(&id).map(|c| c.workspace).unwrap_or(0);
                        if let Err(e) = self.launch_agent(ws, None, &agent, Some(&account), Placement::Tab, None) {
                            self.toast(id, format!("{e:#}"));
                        }
                        return Mode::Normal;
                    }
                }
                (KeyCode::Char('d'), true) if p.kind == PickerKind::Agent => {
                    if let Some(PickAction::AgentLaunch { agent, account }) = p.current().map(|i| &i.action)
                        && let Some(a) = self.accounts.get(agent, account)
                    {
                        return self.account_details(&a.id);
                    }
                }
                (KeyCode::Char('w'), true) if p.kind == PickerKind::Agent && p.worktree => {
                    if let Some(PickAction::AgentLaunch { agent, account }) = p.current().map(|i| i.action.clone()) {
                        return Mode::Prompt(Prompt::new(
                            &format!("Name of the worktree for {agent} (branch kraal/<name>)"),
                            "",
                            PromptAction::NewWorktree { agent, account },
                        ));
                    }
                }
                (KeyCode::Char(key @ ('r' | 'l' | 'x')), true) if p.kind == PickerKind::Accounts => {
                    if let Some(PickAction::AccountDetails(account)) = p.current().map(|i| &i.action)
                        && let Some(a) = self.accounts.by_id(account)
                    {
                        match key {
                            'r' => {
                                return Mode::Prompt(Prompt::new(
                                    &format!("Rename {}", super::super::account_ui::label(&a)),
                                    &a.name,
                                    PromptAction::RenameAccount { agent: a.agent.clone(), account: a.key() },
                                ));
                            }
                            'l' => {
                                if let Err(e) = self.open_login(id, &a) {
                                    self.toast(id, format!("{e:#}"));
                                } else {
                                    return Mode::Normal;
                                }
                            }
                            'x' => {
                                return Mode::confirm(
                                    format!(
                                        "Remove {}?\nConversations and the agent's own sign-in are kept.\nRunning panes keep their account. It will no longer be offered for new launches.",
                                        super::super::account_ui::label(&a)
                                    ),
                                    ConfirmAction::RemoveAccount(a),
                                );
                            }
                            _ => {}
                        }
                    }
                }
                (KeyCode::Char('r'), true) if p.kind == PickerKind::Ssh => {
                    let target = match p.current().map(|i| i.action.clone()) {
                        Some(PickAction::SshGroup(g)) => format!("@{g}"),
                        Some(PickAction::Ssh(name)) => name,
                        _ => return Mode::Picker(p),
                    };
                    let prod = self.production_servers(&target);
                    let warning = match prod.len() {
                        0 => String::new(),
                        1 if !target.starts_with('@') => " · PRODUCTION".to_string(),
                        n => format!(" · {n} PRODUCTION servers"),
                    };
                    return Mode::Prompt(Prompt::new(
                        &format!("Run on {target}{warning} (results in a window)"),
                        "",
                        PromptAction::RunOn { targets: vec![target] },
                    ));
                }
                (KeyCode::Char('o'), true) if p.kind == PickerKind::Ssh => {
                    if let Some(PickAction::Ssh(name)) = p.current().map(|i| i.action.clone()) {
                        return Mode::Prompt(Prompt::new(
                            &format!("Forward from {name}: [host:]port (e.g. 5432, or db.internal:5432)"),
                            "",
                            PromptAction::Tunnel { server: name },
                        ));
                    }
                }
                (KeyCode::Char('e'), true) if p.kind == PickerKind::Runbook => {
                    if let Some(PickAction::Runbook(path)) = p.current().map(|i| i.action.clone()) {
                        self.edit_runbook(id, &path);
                        return Mode::Normal;
                    }
                }
                (KeyCode::Char('y'), true) if p.kind == PickerKind::RunbookSteps => {
                    if let Some(PickAction::RunBlock(i)) = p.current().map(|i| i.action.clone()) {
                        self.copy_block(id, i);
                        return Mode::Normal;
                    }
                }
                (KeyCode::Char('e'), true) if p.kind == PickerKind::Ssh => {
                    self.edit_servers(id);
                    return Mode::Normal;
                }
                (KeyCode::Char('y'), true) if p.kind == PickerKind::Buffers => {
                    if let Some(PickAction::Buffer(b)) = p.current().map(|i| i.action.clone()) {
                        if let Some(text) = self.picked_buffer(id, b) {
                            self.send_clipboard(id, &text);
                        }
                        return Mode::Normal;
                    }
                }
                (KeyCode::Char('o'), true) if p.kind == PickerKind::Buffers => {
                    if let Some(PickAction::Buffer(b)) = p.current().map(|i| i.action.clone()) {
                        self.show_buffer(id, b);
                        return Mode::Normal;
                    }
                }
                (KeyCode::Char('x'), true) if p.kind == PickerKind::Buffers => {
                    if let Some(PickAction::Buffer(b)) = p.current().map(|i| i.action.clone()) {
                        self.delete_picked_buffer(id, b);
                        // The list as it is now, at the same place.
                        let mut next = self.buffer_picker();
                        next.selected = p.selected.min(next.filtered.len().saturating_sub(1));
                        return Mode::Picker(next);
                    }
                }
                (KeyCode::Char('x'), true) if p.kind == PickerKind::Ssh => {
                    if let Some(PickAction::Ssh(name)) = p.current().map(|i| i.action.clone()) {
                        match self.forget_server(&name) {
                            Ok(true) => {
                                self.toast(id, format!("forgot {name}"));
                                return Mode::Picker(self.ssh_picker());
                            }
                            Ok(false) => {
                                self.toast(id, format!("{name} comes from a workspace or ~/.ssh/config; edit it there"))
                            }
                            Err(e) => self.toast(id, format!("{e:#}")),
                        }
                    }
                }
                (KeyCode::Char('x'), true) if p.kind == PickerKind::Workspace => {
                    if let Some(PickAction::SwitchWorkspace(ws)) = p.current().map(|i| i.action.clone()) {
                        let name = self.ws_index(ws).map(|i| self.workspaces[i].name.clone()).unwrap_or_default();
                        return Mode::confirm(
                            format!("Close workspace \"{name}\" and all its panes?"),
                            ConfirmAction::CloseWorkspace(ws),
                        );
                    }
                }
                // A question that may come while the user types elsewhere: typing does not answer it.
                _ if matches!(p.kind, PickerKind::EditConflict | PickerKind::AccountReview) => {}
                _ => {
                    let before = p.query.clone();
                    if crate::server::ui::edit_line(&mut p.query, &mut p.cursor, &k) && p.query != before {
                        p.selected = 0;
                        p.refilter();
                    }
                }
            },
            InputEvent::Paste(_) if matches!(p.kind, PickerKind::EditConflict | PickerKind::AccountReview) => {}
            InputEvent::Paste(data) => {
                // A search field: what does not fit is simply not pasted.
                let _ = crate::server::ui::paste_line(&mut p.query, &mut p.cursor, &String::from_utf8_lossy(&data));
                p.refilter();
            }
            InputEvent::Mouse(MouseEvent { kind: MouseKind::ScrollUp, .. }) => p.move_by(-1),
            InputEvent::Mouse(MouseEvent { kind: MouseKind::ScrollDown, .. }) => p.move_by(1),
            InputEvent::Mouse(MouseEvent { kind: MouseKind::Press(MouseButton::Left), col, row, .. }) => {
                // Rows and their actions use the same activation as Enter.
                let layout = crate::server::overlays::picker_layout(&p, self.client_screen(id));
                if let Some(i) = layout.row_at(col, row) {
                    if i < p.filtered.len() {
                        p.selected = i;
                        let Some(item) = p.current().cloned() else { return Mode::Picker(p) };
                        return self.pick(id, &p, item);
                    }
                } else if !layout.area.contains(ratatui::layout::Position::new(col, row)) {
                    return Mode::Normal;
                }
            }
            _ => {}
        }
        Mode::Picker(p)
    }

    pub(in crate::server) fn pick(&mut self, id: ClientId, p: &Picker, item: PickItem) -> Mode {
        match item.action {
            PickAction::Command(command) => return self.command(id, command),
            PickAction::SwitchWorkspace(ws) => self.show_workspace(id, ws),
            PickAction::OpenDefinition(path) => match self.open_definition(&path) {
                Ok(ws) => self.show_workspace(id, ws),
                Err(e) => self.toast(id, format!("{e:#}")),
            },
            PickAction::NewWorkspace => {
                let name = p.query.trim().to_string();
                let ws = self.new_workspace(name, None);
                match self.spawn_pane_in(ws, None, PaneSpec::default(), Placement::Tab, None) {
                    Ok(_) => self.show_workspace(id, ws),
                    Err(e) => {
                        self.drop_empty_workspaces();
                        self.toast(id, format!("{e:#}"));
                    }
                }
            }
            PickAction::Ssh(spec) => self.connect(id, &spec, Placement::Auto),
            PickAction::SshGroup(group) => match self.open_group(&group, &[]) {
                Ok(ws) => self.show_workspace(id, ws),
                Err(e) => self.toast(id, format!("{e:#}")),
            },
            PickAction::FocusPane(pane) => self.reveal(id, pane),
            PickAction::Buffer(b) => return self.paste_buffer(id, b),
            PickAction::RemoteAgent { server, pane } => self.go_to_remote(id, &server, pane),
            PickAction::FilesSet { pane, side, target } => self.files_set(pane, side, &target),
            PickAction::EditAnswer { edit, answer } => self.answer_edit(id, edit, answer),
            PickAction::Runbook(path) => return self.pick_runbook(id, &path),
            PickAction::RunbookTarget(spec) => return self.set_draft_target(id, &spec),
            PickAction::RunbookValue(v) => return self.set_draft_value(id, v),
            PickAction::RunBlock(i) => return self.run_block(id, i),
            PickAction::RunbookList => return self.runbook_list(id),
            PickAction::Tool { pane, name } => return self.tool_request(id, pane, &name, None),
            PickAction::ReviewAccount(existing) => return Mode::Picker(self.review_account_picker(&existing)),
            PickAction::CheckExisting { agent } => {
                self.retry_existing_account(&agent);
                return Mode::Picker(self.accounts_picker(None));
            }
            PickAction::UseExisting { existing, target } => {
                let name = target.as_ref().map_or(existing.name.as_str(), |a| &a.name);
                return Mode::confirm(
                    format!("Use {name}?\nYour running agents will keep working.\nThis account will appear once."),
                    ConfirmAction::UseExisting { existing: Box::new(existing), target },
                );
            }
            PickAction::AgentLaunch { agent, account } => {
                let ws = self.clients.get(&id).map(|c| c.workspace).unwrap_or(0);
                let focus = self.client_focus(id);
                if let Err(e) = self.launch_agent(ws, focus, &agent, Some(&account), Placement::Auto, None) {
                    self.toast(id, format!("{e:#}"));
                }
            }
            PickAction::CancelSwitch { pane, token } => self.cancel_switch(pane, token),
            PickAction::AgentSwitch { pane, agent, account, .. } => {
                let result = self
                    .accounts
                    .get(&agent, &account)
                    .context("no such account")
                    .and_then(|a| self.switch_account(pane, &a, Some(id), false));
                if let Err(e) = result {
                    self.toast(id, format!("{e:#}"));
                }
            }
            PickAction::Worktree { pane, op } => return self.worktree_op(id, pane, &op),
            PickAction::AddAccount => return Mode::AccountForm(self.account_form(id)),
            PickAction::AccountDetails(account) => return self.account_details(&account),
            PickAction::None => return Mode::Picker(Picker::new(p.kind, &p.title, p.items.clone())),
        }
        Mode::Normal
    }

    pub(super) fn workspace_picker(&self, current: u32) -> Picker {
        let mut items = Vec::new();
        for ws in &self.workspaces {
            let panes = ws.panes();
            let (mut working, mut blocked, mut done) = (0, 0, 0);
            for p in &panes {
                match self.panes.get(p).map(|x| x.agent.state) {
                    Some(AgentState::Working) => working += 1,
                    Some(AgentState::Blocked) => blocked += 1,
                    Some(AgentState::Done) => done += 1,
                    _ => {}
                }
            }
            let mut detail =
                format!("{} tab{} · {} pane{}", ws.tabs.len(), plural(ws.tabs.len()), panes.len(), plural(panes.len()));
            if blocked + working + done > 0 {
                detail.push_str(&format!(" · ▲{blocked} ●{working} ✓{done}"));
            }
            if ws.id == current {
                detail.push_str(" · current");
            }
            let state = if blocked > 0 {
                Some(AgentState::Blocked)
            } else if done > 0 {
                Some(AgentState::Done)
            } else if working > 0 {
                Some(AgentState::Working)
            } else {
                None
            };
            items.push(PickItem {
                label: ws.name.clone(),
                detail,
                action: PickAction::SwitchWorkspace(ws.id),
                state,
                dim: false,
            });
        }
        for (path, def) in self.definitions() {
            match def {
                Ok(def) => {
                    if self.definition_workspace(&path).is_some() {
                        continue;
                    }
                    let n = def.panes().len();
                    items.push(PickItem {
                        label: def.name,
                        detail: format!("not running · {n} pane{} · {}", plural(n), file_name(&path)),
                        action: PickAction::OpenDefinition(path),
                        state: None,
                        dim: true,
                    });
                }
                Err(e) => items.push(PickItem {
                    label: path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
                    detail: format!("error: {e:#}"),
                    action: PickAction::None,
                    state: None,
                    dim: true,
                }),
            }
        }
        items.push(PickItem {
            label: String::new(),
            detail: String::new(),
            action: PickAction::NewWorkspace,
            state: None,
            dim: false,
        });
        Picker::new(PickerKind::Workspace, "Workspaces · type a new name to create one", items)
    }

    /// Every command, to find by what it does and run; each shows its keys.
    pub(super) fn command_picker(&self) -> Picker {
        let mut items = Vec::new();
        // Tabs are picked by number from the keyboard, not from a list of commands.
        for a in Action::all().filter(|a| !matches!(a, Action::Tab(_))) {
            let command = Command::Action(a);
            let keys = self.keymap.all_hints(&command);
            let item = PickItem::new(a.describe(), keys.join("  ·  "), PickAction::Command(command));
            items.push(if keys.is_empty() { item.dim() } else { item });
        }
        for name in self.config.tools.iter().filter(|(_, t)| t.enabled).map(|(n, _)| n.clone()) {
            let command = Command::Tool(name.clone());
            let keys = self.keymap.all_hints(&command).join("  ·  ");
            items.push(PickItem::new(format!("Tool: {name}"), keys, PickAction::Command(command)));
        }
        Picker::new(PickerKind::Commands, "Commands", items)
    }

    pub(super) fn pane_picker(&self) -> Picker {
        let mut items = Vec::new();
        for ws in &self.workspaces {
            for tab in &ws.tabs {
                for p in tab.panes() {
                    let Some(pane) = self.panes.get(&p) else { continue };
                    let mut detail = String::new();
                    if let Some(a) = &pane.agent.kind {
                        detail = format!("{a} {}", pane.agent.state.label());
                    } else if !pane.spec.command.is_empty() {
                        detail = pane.spec.command.join(" ");
                    } else if let Some(cwd) = pane.current_cwd() {
                        detail = paths::shorten_home(&cwd);
                    }
                    items.push(PickItem {
                        label: format!("{} › {} › {}", ws.name, self.tab_label(tab), pane.display_name()),
                        detail,
                        action: PickAction::FocusPane(p),
                        state: pane.agent.kind.as_ref().map(|_| pane.agent.state),
                        dim: false,
                    });
                }
            }
        }
        // The agents of the servers watched: Enter attaches to their session.
        for (server, a) in self.remote_agents() {
            items.push(PickItem {
                label: format!("{server} › {} › {}", a.workspace, a.name),
                detail: format!("{} {} · kraal on {server}", a.agent, a.state.label()),
                action: PickAction::RemoteAgent { server: server.to_string(), pane: a.pane },
                state: Some(a.state),
                dim: false,
            });
        }
        Picker::new(PickerKind::Pane, "Find a pane", items)
    }

    /// Open an SSH pane in the client's workspace to a server name or `user@host`.
    pub(super) fn connect(&mut self, id: ClientId, spec: &str, placement: Placement) {
        let Some(ws) = self.clients.get(&id).map(|c| c.workspace) else { return };
        let Some(server) = self.resolve_server(spec) else {
            self.toast(id, format!("not a server: {spec}"));
            return;
        };
        if let Err(e) = self.open_ssh(ws, None, &server, placement) {
            self.toast(id, format!("{e:#}"));
        }
    }

    /// Edit `servers.toml` in a floating pane.
    pub(super) fn edit_servers(&mut self, id: ClientId) {
        let path = crate::servers::Inventory::path();
        if !path.exists()
            && let Err(e) = crate::servers::Inventory::empty().save()
        {
            self.toast(id, format!("{e:#}"));
            return;
        }
        let command = crate::edit::editor_argv(&path, None);
        let Some(ws) = self.clients.get(&id).map(|c| c.workspace) else { return };
        let spec = PaneSpec { command, close_on_exit: true, ..Default::default() };
        if let Err(e) = self.spawn_pane_in(ws, None, spec, Placement::Floating, Some("servers.toml".into())) {
            self.toast(id, format!("{e:#}"));
        }
    }
}

#[cfg(test)]
mod account_tests {
    use super::*;
    use crate::input::Mods;

    #[test]
    fn one_click_on_either_line_opens_an_unselected_account_and_blank_rows_do_nothing() {
        let (tx, _) = crate::server::queue::channel(16);
        let mut server = crate::server::tests::server(tx);
        let a = server
            .accounts
            .add(crate::accounts::Account {
                id: crate::accounts::new_uuid(),
                agent: "claude".into(),
                name: "work".into(),
                home: "/nonexistent/work".into(),
                threshold: None,
                note: None,
                sign_in_required: false,
                review: false,
                public_identity: None,
                linked_to: None,
            })
            .unwrap();
        for line in [0, 1] {
            let mut p = server.accounts_picker(None);
            p.selected = 1;
            let l = crate::server::overlays::picker_layout(&p, server.client_screen(0));
            let event = InputEvent::Mouse(MouseEvent {
                kind: MouseKind::Press(MouseButton::Left),
                col: l.list.x + 2,
                row: l.list.y + line,
                mods: Mods::default(),
            });
            assert!(matches!(server.on_picker(0, p, event), Mode::AccountDetails(d) if d.account == a.id));
        }
        let p = Picker::new(PickerKind::Accounts, "Accounts", vec![]);
        let l = crate::server::overlays::picker_layout(&p, server.client_screen(0));
        let event = InputEvent::Mouse(MouseEvent {
            kind: MouseKind::Press(MouseButton::Left),
            col: l.list.x + 2,
            row: l.list.y,
            mods: Mods::default(),
        });
        assert!(matches!(server.on_picker(0, p, event), Mode::Picker(_)));
    }

    #[test]
    fn remove_first_asks_and_enter_cancels_without_touching_accounts() {
        let (tx, _) = crate::server::queue::channel(16);
        let mut server = crate::server::tests::server(tx);
        let a = server
            .accounts
            .add(crate::accounts::Account {
                id: crate::accounts::new_uuid(),
                agent: "claude".into(),
                name: "work".into(),
                home: "/nonexistent/work".into(),
                threshold: None,
                note: None,
                sign_in_required: false,
                review: false,
                public_identity: None,
                linked_to: None,
            })
            .unwrap();
        let p = server.accounts_picker(Some(&a.id));
        let key = Key { code: KeyCode::Char('x'), mods: Mods { ctrl: true, ..Default::default() }, raw: Vec::new() };
        let Mode::Confirm(q) = server.on_picker(0, p, InputEvent::Key(key)) else { panic!() };
        assert!(!q.yes);
        assert!(q.message.contains("Conversations and the agent's own sign-in are kept."));
        assert_eq!(q.action.verb(), "Remove");
        let key = Key { code: KeyCode::Enter, mods: Mods::default(), raw: Vec::new() };
        assert!(
            matches!(server.on_confirm(0, q, InputEvent::Key(key)), Mode::Picker(p) if p.kind == PickerKind::Accounts)
        );
        assert_eq!(server.accounts.all().len(), 1);
    }
}
