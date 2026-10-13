//! Lists to choose from: the generic picker, and the workspace, command and pane lists.

use std::collections::HashSet;

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
                (KeyCode::Char(key @ ('a' | 'u')), true) if p.kind == PickerKind::Pane => {
                    if let Some(PickAction::FocusPane { pane, .. }) = p.current().map(|i| &i.action) {
                        if key == 'a' {
                            return self.change_account_mode(id, *pane);
                        }
                        if let Err(e) = self.undo_account_change(*pane, id) {
                            self.toast(id, format!("{e:#}"));
                        }
                        return Mode::Normal;
                    }
                }
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
                        return self.ask_close_workspace(id, ws);
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
            PickAction::SwitchWorkspace(ws) | PickAction::ReopenWorkspace(ws) => self.show_workspace(id, ws),
            PickAction::RetryWorkspaceClose(ws) => return self.ask_close_workspace(id, ws),
            PickAction::OpenDefinition(path) => match self.open_definition(&path) {
                Ok(ws) => self.show_workspace(id, ws),
                Err(e) => self.toast(id, format!("{e:#}")),
            },
            PickAction::NewWorkspace => {
                let mut prompt = Prompt::new("New workspace", &p.query, PromptAction::NewWorkspace);
                prompt.cursor = p.cursor;
                return self.submit_prompt(id, prompt);
            }
            PickAction::Ssh(spec) => self.connect(id, &spec, Placement::Auto),
            PickAction::SshGroup(group) => match self.open_group(&group, &[]) {
                Ok(ws) => self.show_workspace(id, ws),
                Err(e) => self.toast(id, format!("{e:#}")),
            },
            PickAction::FocusPane { pane, .. } => self.reveal(id, pane),
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
                if target.is_none() {
                    return Mode::AccountForm(self.name_existing_form(id, existing));
                }
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
            PickAction::OpenAnotherAccount(pane) => {
                let agent = self.panes.get(&pane).and_then(|p| p.agent.kind.clone());
                let mut picker = self.agent_picker(id, None);
                picker.title = "Open with another account".into();
                if let Some(agent) = agent {
                    picker
                        .items
                        .retain(|i| matches!(&i.action, PickAction::AgentLaunch { agent: a, .. } if *a == agent));
                    picker.refilter();
                }
                return Mode::Picker(picker);
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
                    return self.change_account_mode(id, pane);
                }
            }
            PickAction::Worktree { pane, op } => return self.worktree_op(id, pane, &op),
            PickAction::AddAccount => return Mode::AccountForm(self.account_form(id)),
            PickAction::AccountDetails(account) => return self.account_details(&account),
            PickAction::None => return Mode::Picker(Picker::new(p.kind, &p.title, p.items.clone())),
        }
        Mode::Normal
    }

    pub(in crate::server) fn refresh_workspace_pickers(&mut self) {
        let clients: Vec<_> = self
            .clients
            .iter()
            .filter_map(|(&id, c)| {
                let Mode::Picker(p) = &c.mode else { return None };
                (p.kind == PickerKind::Workspace).then(|| (id, c.workspace, p.query.clone(), p.selected))
            })
            .collect();
        for (id, ws, query, selected) in clients {
            let mut picker = self.workspace_picker(ws);
            picker.query = query;
            picker.selected = selected;
            picker.refilter();
            self.clients.get_mut(&id).unwrap().mode = Mode::Picker(picker);
        }
    }

    pub(in crate::server) fn workspace_picker(&self, current: u32) -> Picker {
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
            let (tabs, count) = ws.counts();
            let mut detail = format!("{} · {tabs} tab{} · {count} pane{}", ws.status(), plural(tabs), plural(count));
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
                action: if ws.close_error.is_some() {
                    PickAction::RetryWorkspaceClose(ws.id)
                } else if ws.closed() {
                    PickAction::ReopenWorkspace(ws.id)
                } else {
                    PickAction::SwitchWorkspace(ws.id)
                },
                state,
                dim: false,
            });
        }
        let definitions = self.definitions();
        let mut names = HashSet::new();
        let mut collisions = HashSet::new();
        for (_, def) in &definitions {
            if let Ok(def) = def
                && !names.insert(&def.name)
            {
                collisions.insert(def.name.clone());
            }
        }
        for (path, def) in definitions {
            match def {
                Ok(def) => {
                    if !matches!(self.definition_workspace(&path), Ok(None)) {
                        continue;
                    }
                    let n = def.panes().len();
                    let source =
                        if collisions.contains(&def.name) { paths::shorten_home(&path) } else { file_name(&path) };
                    items.push(PickItem {
                        label: def.name,
                        detail: format!("not running · {n} pane{} · {source}", plural(n)),
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
        let selected = items
            .iter()
            .position(|item| {
                matches!(item.action,
            PickAction::SwitchWorkspace(id) | PickAction::ReopenWorkspace(id) | PickAction::RetryWorkspaceClose(id) if id == current)
            })
            .unwrap_or(0);
        let mut picker = Picker::new(PickerKind::Workspace, "Workspaces · type a new name to create one", items);
        picker.selected = selected;
        picker
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
                        action: PickAction::FocusPane {
                            pane: p,
                            agent: pane.agent.kind.is_some(),
                            undo: pane.spec.account_undo.is_some(),
                        },
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
mod workspace_tests {
    use super::*;

    #[test]
    fn colliding_workspace_files_show_their_directories_before_either_is_opened() {
        let (tx, _) = crate::server::queue::channel(16);
        let mut server = crate::server::tests::server(tx);
        // Exercise the clipping caused by macOS's long temporary paths on every platform.
        let dir =
            std::env::temp_dir().join(format!("kraal-picker-{}-{}", std::process::id(), "long-directory-".repeat(8)));
        for (sub, command) in [("one", "true"), ("two", "false")] {
            let path = dir.join(sub);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(
                path.join("same.kdl"),
                format!("layout {{ pane command=\"{command}\" start_suspended=true; }}\n"),
            )
            .unwrap();
            server.config.workspace_dirs.push(path.display().to_string());
        }
        std::fs::write(dir.join("one/only.kdl"), "layout { pane; }").unwrap();
        assert!(
            server.open_workspace("same").unwrap_err().to_string().contains("choose a file in the workspace picker")
        );
        let picker = server.workspace_picker(0);
        let rows: Vec<_> = picker.items.iter().filter(|item| item.label == "same").collect();
        assert_eq!(rows.len(), 2);
        for (row, sub) in rows.iter().zip(["one", "two"]) {
            let path = crate::workspace::definition_path(&dir.join(sub).join("same.kdl"));
            assert_eq!(row.detail, format!("not running · 1 pane · {}", paths::shorten_home(&path)));
            assert!(matches!(&row.action, PickAction::OpenDefinition(p) if p == &path));
        }
        let only = picker.items.iter().find(|item| item.label == "only").unwrap();
        assert_eq!(only.detail, "not running · 1 pane · only.kdl");
        for width in [80, 120] {
            let screen = ratatui::layout::Rect::new(0, 0, width, 30);
            let mut buffer = ratatui::buffer::Buffer::empty(screen);
            crate::server::overlays::draw_picker(&picker, screen, &mut buffer);
            let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
            assert!(text.contains("one/same.kdl") && text.contains("two/same.kdl"), "{text}");
        }
        assert!(server.workspaces.is_empty());
        assert!(server.panes.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::Mods;

    #[test]
    fn creation_from_the_picker_keeps_input_on_failure_and_rechecks_the_name() {
        let (tx, _rx) = crate::server::queue::channel(32);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        let current = server.clients[&client].workspace;
        server.env.shell = "/nonexistent/kraal-shell".into();
        for stale in [false, true] {
            let mut p = server.workspace_picker(current);
            p.query = " café 日 ".into();
            p.cursor = 3;
            p.refilter();
            assert!(matches!(p.current().unwrap().action, PickAction::NewWorkspace));
            if stale {
                server.workspaces[0].name = "café 日".into();
            }
            let event = InputEvent::Key(Key { code: KeyCode::Enter, mods: Mods::NONE, raw: Vec::new() });
            let Mode::Prompt(p) = server.on_picker(client, p, event) else { panic!("keep input") };
            assert_eq!((p.value.as_str(), p.cursor), (" café 日 ", 3));
            assert!(matches!(p.action, PromptAction::NewWorkspace));
            let reason = if stale { "already exists" } else { "No workspace was added." };
            assert!(p.error.as_ref().unwrap().contains(reason));
            assert_eq!(server.workspaces.len(), 1);
            assert_eq!(server.clients[&client].workspace, current);
        }
    }

    #[test]
    fn both_close_entries_list_only_the_chosen_workspace_including_other_tabs_and_floats() {
        use crate::input::{InputEvent, Key, KeyCode, Mods};
        use crate::pane::PaneSpec;
        use crate::protocol::Placement;
        use crate::server::ui::{PickAction, PickItem, Picker, PickerKind};

        let (tx, _rx) = crate::server::queue::channel(32);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        let ws = server.clients[&client].workspace;
        let other = server.new_workspace("other".into(), None);
        let _batch = crate::pane::batch();
        for (workspace, placement, name) in [
            (ws, Placement::Auto, "first"),
            (ws, Placement::Tab, "second"),
            (ws, Placement::Floating, "floating"),
            (other, Placement::Auto, "elsewhere"),
        ] {
            let id = server.spawn_pane_in(workspace, None, PaneSpec::default(), placement, Some(name.into())).unwrap();
            server.panes.get_mut(&id).unwrap().spec.command = vec!["sleep".into(), name.into()];
        }
        let Mode::Confirm(direct) = server.action(client, crate::keymap::Action::CloseWorkspace) else {
            panic!("confirmation")
        };
        // A picker can close a workspace other than the one the client shows.
        server.clients.get_mut(&client).unwrap().workspace = other;
        let picker = Picker::new(
            PickerKind::Workspace,
            "Workspaces",
            vec![PickItem::new("test", "", PickAction::SwitchWorkspace(ws))],
        );
        let close = InputEvent::Key(Key { code: KeyCode::Char('x'), mods: Mods::CTRL, raw: Vec::new() });
        let Mode::Confirm(picked) = server.on_picker(client, picker, close) else { panic!("confirmation") };
        assert_eq!(direct.message, picked.message);
        assert!(matches!(picked.action, ConfirmAction::CloseWorkspace(id) if id == ws));
        assert!(!picked.yes);
        for name in ["first", "second", "floating"] {
            assert!(picked.message.contains(&format!("sleep {name}  (pane ")), "{}", picked.message);
        }
        assert!(!picked.message.contains("elsewhere"));
        assert!(picked.message.contains("Unsaved work in these programs may be lost."));
        for question in [server.ask_quit(), server.ask_restart()] {
            let Mode::Confirm(q) = question else { panic!("confirmation") };
            assert!(q.message.contains("sleep elsewhere  (pane "));
            assert!(!q.message.contains("layout will be removed"));
        }
    }

    #[test]
    fn workspace_tab_and_close_apply_only_to_an_open_workspace() {
        let (tx, _rx) = crate::server::queue::channel(32);
        let mut server = crate::server::tests::server(tx);
        assert_eq!(server.new_workspace("open".into(), None), 1);
        let _batch = crate::pane::batch();
        server
            .spawn_pane_in(
                1,
                None,
                crate::pane::PaneSpec { command: vec!["sleep".into(), "60".into()], ..Default::default() },
                crate::protocol::Placement::Tab,
                None,
            )
            .unwrap();
        let key = |code, mods| InputEvent::Key(Key { code, mods, raw: Vec::new() });
        for action in [
            PickAction::SwitchWorkspace(1),
            PickAction::SwitchWorkspace(u32::MAX),
            PickAction::ReopenWorkspace(1),
            PickAction::OpenDefinition("closed.kdl".into()),
            PickAction::NewWorkspace,
            PickAction::None,
        ] {
            let open = matches!(action, PickAction::SwitchWorkspace(_));
            let stale = matches!(action, PickAction::SwitchWorkspace(u32::MAX));
            let mut p = Picker::new(
                PickerKind::Workspace,
                "Workspaces",
                vec![PickItem::new("", "", action), PickItem::new("scratch", "", PickAction::None)],
            );
            p.query = "new".into();
            p.filtered = vec![0, 1];
            let Mode::Picker(mut p) = server.on_picker(0, p, key(KeyCode::Tab, Mods::NONE)) else { panic!() };
            assert_eq!(p.actions, open.then_some(0));
            assert_eq!(p.selected, usize::from(!open));
            p.actions = None;
            p.selected = 0;
            let next = server.on_picker(0, p, key(KeyCode::Char('x'), Mods::CTRL));
            if stale {
                assert!(matches!(next, Mode::Normal), "a stale workspace cannot be closed");
            } else if open {
                assert!(matches!(next, Mode::Confirm(q) if matches!(q.action, ConfirmAction::CloseWorkspace(1))));
            } else {
                assert!(matches!(next, Mode::Picker(p) if p.query == "new" && p.selected == 0));
            }
        }
    }

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
                references: Vec::new(),
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
                references: Vec::new(),
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
