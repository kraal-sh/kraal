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
use crate::server::ui::{ConfirmAction, Mode, PickAction, PickItem, Picker, PickerKind, Prompt, PromptAction};
use crate::server::{ClientId, Server};

impl Server {
    pub(super) fn on_picker(&mut self, id: ClientId, mut p: Picker, ev: InputEvent) -> Mode {
        if let InputEvent::Mouse(MouseEvent { kind: MouseKind::Press(MouseButton::Left), col, row: 0, .. }) = &ev
            && let Some(super::super::render::BarItem::Toast(
                _,
                super::super::attention::ToastAction::RestoreWorkspace(identity),
            )) = self.tab_bar_hit(id, *col)
        {
            return self.restore_workspace_mode(id, &identity, Some(&p));
        }
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
                    if keys[i].0.is_empty() {
                        return self.picker_menu_action(id, p, keys[i].1);
                    }
                    let spec = crate::input::KeySpec::parse(keys[i].0).expect("picker action keys are valid");
                    let key = Key { code: spec.code, mods: spec.mods, raw: Vec::new() };
                    return self.on_picker(id, p, InputEvent::Key(key));
                }
            } else if let InputEvent::Key(k) = ev {
                match k.code {
                    KeyCode::Up => p.actions = Some(at.checked_sub(1).unwrap_or(keys.len() - 1)),
                    KeyCode::Down => p.actions = Some((at + 1) % keys.len()),
                    KeyCode::Enter => {
                        p.actions = None;
                        if keys[at].0.is_empty() {
                            return self.picker_menu_action(id, p, keys[at].1);
                        }
                        // The action is its key, pressed in the list.
                        let spec = crate::input::KeySpec::parse(keys[at].0).expect("picker action keys are valid");
                        let key = Key { code: spec.code, mods: spec.mods, raw: Vec::new() };
                        return self.on_picker(id, p, InputEvent::Key(key));
                    }
                    _ => p.actions = None,
                }
            }
            return if p.kind == PickerKind::Workspace && p.actions.is_none() {
                self.return_to_workspaces(id, Mode::Normal, p.state())
            } else {
                Mode::Picker(p)
            };
        }
        match ev {
            InputEvent::Key(k) => match (k.code, k.mods.ctrl) {
                (KeyCode::Esc, _) | (KeyCode::Char('c'), true) | (KeyCode::Char('g'), true) => {
                    return if p.kind == PickerKind::AccountReview {
                        Mode::Picker(self.accounts_picker(None))
                    } else if p.kind == PickerKind::DeletedWorkspaces {
                        let mut back = self.workspace_picker(self.clients.get(&id).map_or(0, |c| c.workspace));
                        if let Some(state) = p.back.take() {
                            back.restore(*state);
                        }
                        Mode::Picker(back)
                    } else {
                        Mode::Normal
                    };
                }
                (KeyCode::Enter, _) => {
                    let Some(item) = p.current().cloned() else { return Mode::Picker(p) };
                    return self.pick(id, &p, item);
                }
                (KeyCode::Char('z'), true) if p.undo.is_some() => {
                    return self.restore_workspace_mode(id, p.undo.as_deref().unwrap(), Some(&p));
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
                (KeyCode::Char('r'), true) if p.kind == PickerKind::Workspace => {
                    if let Some(PickAction::SwitchWorkspace(ws) | PickAction::ReopenWorkspace(ws)) =
                        p.current().map(|i| &i.action)
                    {
                        return self.rename_workspace_form(*ws, Some(&p));
                    }
                }
                (KeyCode::Char('x'), true) if p.kind == PickerKind::Workspace => {
                    if let Some(PickAction::SwitchWorkspace(ws)) = p.current().map(|i| i.action.clone()) {
                        let mode = self.ask_close_workspace(id, ws);
                        return self.return_to_workspaces(id, mode, p.state());
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
            InputEvent::Mouse(MouseEvent { kind: MouseKind::Press(MouseButton::Right), col, row, .. })
                if p.kind == PickerKind::Workspace =>
            {
                let layout = crate::server::overlays::picker_layout(&p, self.client_screen(id));
                if let Some(i) = layout.row_at(col, row).filter(|&i| i < p.filtered.len()) {
                    p.selected = i;
                    p.actions = (p.action_keys().len() > 1).then_some(0);
                }
            }
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
            PickAction::SwitchWorkspace(ws) | PickAction::ReopenWorkspace(ws) => {
                if let Err(e) = self.reopen_workspace(ws) {
                    let mode = Mode::confirm(
                        format!(
                            "Couldn't open the workspace.\n{e:#}\nThe saved workspace is kept. Fix the cause and retry."
                        ),
                        ConfirmAction::OpenWorkspace(PickAction::ReopenWorkspace(ws)),
                    );
                    return self.return_to_workspaces(id, mode, p.state());
                }
                self.show_workspace(id, ws);
            }
            PickAction::RetryWorkspaceClose(ws) => {
                let mode = self.ask_close_workspace(id, ws);
                return self.return_to_workspaces(id, mode, p.state());
            }
            PickAction::RetryWorkspaceDelete(ws) => {
                let mode = self.ask_delete_workspace(super::super::workspace_lifecycle::DeleteTarget::Workspace(ws));
                return self.return_to_workspaces(id, mode, p.state());
            }
            PickAction::DeletedWorkspaces => {
                let mut deleted = self.deleted_workspace_picker();
                deleted.back = Some(Box::new(p.state()));
                return Mode::Picker(deleted);
            }
            PickAction::RestoreWorkspace(identity) => {
                return self.restore_workspace_mode(id, &identity, Some(p));
            }
            PickAction::OpenDefinition(path) => match self.open_definition(&path) {
                Ok(ws) => self.show_workspace(id, ws),
                Err(e) => {
                    let mode = Mode::confirm(
                        format!("Couldn't open the file.\n{e:#}\nThe file is unchanged. Fix the cause and retry."),
                        ConfirmAction::OpenWorkspace(PickAction::OpenDefinition(path)),
                    );
                    return self.return_to_workspaces(id, mode, p.state());
                }
            },
            PickAction::AddWorkspace => {
                return Mode::Prompt(self.add_workspace_form(id, Some(p)));
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
            PickAction::UnavailableDefinition(_) => {
                let mut next = Picker::new(p.kind, &p.title, p.items.clone());
                next.restore(p.state());
                return Mode::Picker(next);
            }
        }
        Mode::Normal
    }

    pub(in crate::server) fn refresh_workspace_pickers(&mut self) {
        let clients: Vec<_> = self
            .clients
            .iter()
            .filter_map(|(&id, c)| {
                let Mode::Picker(p) = &c.mode else { return None };
                matches!(p.kind, PickerKind::Workspace | PickerKind::DeletedWorkspaces)
                    .then(|| (id, c.workspace, p.state()))
            })
            .collect();
        for (id, ws, state) in clients {
            let mut picker = if state.kind == PickerKind::DeletedWorkspaces {
                self.deleted_workspace_picker()
            } else {
                self.workspace_picker(ws)
            };
            picker.restore(state);
            if picker.undo.as_ref().is_some_and(|id| !self.deleted_workspaces.iter().any(|d| &d.workspace.id == id)) {
                picker.undo = None;
                picker.message = None;
            }
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
                detail.push_str(" · Current");
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
                action: if ws.delete_pending {
                    PickAction::RetryWorkspaceDelete(ws.id)
                } else if ws.close_error.is_some() {
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
                    let source = if collisions.contains(&def.name) {
                        format!(" · {}", paths::shorten_home(&path))
                    } else {
                        String::new()
                    };
                    items.push(PickItem {
                        label: def.name,
                        detail: format!(
                            "Closed · {} tab{} · {n} pane{}{source}",
                            def.tabs.len(),
                            plural(def.tabs.len()),
                            plural(n)
                        ),
                        action: PickAction::OpenDefinition(path),
                        state: None,
                        dim: false,
                    });
                }
                Err(e) => items.push(PickItem {
                    label: path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
                    detail: format!("error: {e:#}"),
                    action: PickAction::UnavailableDefinition(path),
                    state: None,
                    dim: false,
                }),
            }
        }
        items.sort_by_key(|i| self.workspace_position(&i.action));
        if !self.deleted_workspaces.is_empty() {
            items.push(PickItem::new(
                format!("Deleted workspaces ({})", self.deleted_workspaces.len()),
                "",
                PickAction::DeletedWorkspaces,
            ));
        }
        items.push(PickItem {
            label: "+ Add workspace".into(),
            detail: String::new(),
            action: PickAction::AddWorkspace,
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
        let mut picker = Picker::new(PickerKind::Workspace, "Workspaces", items);
        picker.selected = selected;
        picker
    }

    fn picker_menu_action(&mut self, id: ClientId, mut p: Picker, action: &str) -> Mode {
        match action {
            "Duplicate" => {
                return match p.current().and_then(|i| i.action.workspace()) {
                    Some(ws) => self.duplicate_workspace_form(ws, &p),
                    None => Mode::Picker(p),
                };
            }
            "Move up" | "Move down" => return self.move_picker_workspace(id, p, action == "Move up"),
            "Clear search to reorder" => {
                let selected = p.current().and_then(|i| self.workspace_key(&i.action));
                p.query.clear();
                p.cursor = 0;
                p.refilter();
                p.select(|i| self.workspace_key(&i.action) == selected);
                return Mode::Picker(p);
            }
            "Delete" => {}
            _ => return Mode::Picker(p),
        }
        use super::super::workspace_lifecycle::DeleteTarget;
        let target = match p.current().map(|i| &i.action) {
            Some(PickAction::SwitchWorkspace(ws) | PickAction::ReopenWorkspace(ws)) => DeleteTarget::Workspace(*ws),
            Some(PickAction::OpenDefinition(path)) => DeleteTarget::Source(path.clone()),
            _ => return Mode::Picker(p),
        };
        let mode = self.ask_delete_workspace(target);
        self.return_to_workspaces(id, mode, p.state())
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
            assert_eq!(row.detail, format!("Closed · 1 tab · 1 pane · {}", paths::shorten_home(&path)));
            assert!(matches!(&row.action, PickAction::OpenDefinition(p) if p == &path));
        }
        let only = picker.items.iter().find(|item| item.label == "only").unwrap();
        assert_eq!(only.detail, "Closed · 1 tab · 1 pane");
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
    fn closed_rows_open_once_and_their_actions_rename_without_switching() {
        let (tx, _rx) = crate::server::queue::channel(32);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        let current = server.clients[&client].workspace;
        let ws = server.new_workspace("closed".into(), None);
        let mut saved = crate::persist::snapshot(&server).workspaces.remove(1);
        saved.closed = true;
        for line in [0, 1] {
            server.workspaces[1].kept = Some(saved.clone());
            let p = server.workspace_picker(current);
            let layout = crate::server::overlays::picker_layout(&p, server.client_screen(client));
            let click = |button| {
                InputEvent::Mouse(MouseEvent {
                    kind: MouseKind::Press(button),
                    col: layout.list.x + 2,
                    row: layout.list.y + 2 + line,
                    mods: Mods::NONE,
                })
            };
            assert!(matches!(server.on_picker(client, p, click(MouseButton::Left)), Mode::Normal));
            assert_eq!(server.clients[&client].workspace, ws);
            assert!(!server.workspaces[1].closed());
            assert!(server.panes.is_empty());
            server.workspaces[1].kept = Some(saved.clone());
            server.clients.get_mut(&client).unwrap().workspace = current;
            let p = server.workspace_picker(current);
            let Mode::Picker(p) = server.on_picker(client, p, click(MouseButton::Right)) else { panic!() };
            assert_eq!(p.actions, Some(0));
            let key = |code| InputEvent::Key(Key { code, mods: Mods::NONE, raw: Vec::new() });
            let Mode::Picker(p) = server.on_picker(client, p, key(KeyCode::Down)) else { panic!() };
            let Mode::Prompt(p) = server.on_picker(client, p, key(KeyCode::Enter)) else { panic!() };
            assert!(matches!(p.action, PromptAction::RenameWorkspace(id) if id == ws));
            assert_eq!(server.clients[&client].workspace, current);
            assert!(server.workspaces[1].closed());
        }
    }

    #[test]
    fn failed_open_keeps_a_readable_retry_and_cancel_returns_to_the_search() {
        let (tx, _rx) = crate::server::queue::channel(32);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        let current = server.clients[&client].workspace;
        let path = std::env::temp_dir().join(format!("kraal-w6-open-{}.kdl", std::process::id()));
        let item = PickItem::new("file", "Closed", PickAction::OpenDefinition(path.clone()));
        let mut p = Picker::new(PickerKind::Workspace, "Workspaces", vec![item.clone()]);
        p.query = "file".into();
        p.cursor = 2;
        p.refilter();
        std::fs::write(&path, "invalid {").unwrap();
        for retry in [false, true] {
            let Mode::Confirm(q) = server.pick(client, &p, item.clone()) else { panic!() };
            assert!(q.message.contains("Fix the cause and retry."));
            assert!(!q.yes);
            if retry {
                std::fs::write(&path, "layout { pane; }").unwrap();
            }
            let _batch = crate::pane::batch();
            let event = InputEvent::Key(Key {
                code: if retry { KeyCode::Char('y') } else { KeyCode::Esc },
                mods: Mods::NONE,
                raw: Vec::new(),
            });
            let mode = server.on_confirm(client, q, event);
            if retry {
                assert!(matches!(mode, Mode::Normal));
                assert_ne!(server.clients[&client].workspace, current);
                assert_eq!(server.workspaces.len(), 2);
            } else {
                let Mode::Picker(p) = mode else { panic!() };
                assert_eq!((p.query.as_str(), p.cursor), ("file", 2));
                assert_eq!(server.clients[&client].workspace, current);
                assert_eq!(server.workspaces.len(), 1);
            }
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn closing_from_a_filtered_list_keeps_its_query_cursor_and_workspace_selection() {
        let (tx, _rx) = crate::server::queue::channel(32);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        let ws = server.clients[&client].workspace;
        crate::server::persistence::tests::writer(&mut server, vec![Ok(())]);
        let mut p = server.workspace_picker(ws);
        p.query = "test".into();
        p.cursor = 2;
        p.refilter();
        let event = InputEvent::Key(Key { code: KeyCode::Char('x'), mods: Mods::CTRL, raw: Vec::new() });
        let Mode::Picker(p) = server.on_picker(client, p, event) else { panic!() };
        assert_eq!((p.query.as_str(), p.cursor), ("test", 2));
        assert!(matches!(p.current().unwrap().action, PickAction::ReopenWorkspace(id) if id == ws));
        assert!(server.workspaces[0].closed());
    }

    #[test]
    fn closing_from_a_status_filter_clears_it_and_enter_reopens_the_same_workspace() {
        let (tx, _rx) = crate::server::queue::channel(32);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        let ws = server.clients[&client].workspace;
        server
            .spawn_pane_in(
                ws,
                None,
                PaneSpec { files: Some(["/missing".into(), "/missing".into()]), ..Default::default() },
                Placement::Auto,
                None,
            )
            .unwrap();
        let identity = server.workspaces[0].stable_id.clone();
        crate::server::persistence::tests::writer(&mut server, vec![Ok(()), Ok(())]);
        let mut p = server.workspace_picker(ws);
        p.query = "Open 1".into();
        p.cursor = p.query.len();
        p.refilter();
        assert_eq!(p.current().unwrap().action.workspace(), Some(ws));
        let key = |code, mods| InputEvent::Key(Key { code, mods, raw: Vec::new() });
        let Mode::Picker(p) = server.on_picker(client, p, key(KeyCode::Char('x'), Mods::CTRL)) else { panic!() };
        assert_eq!((p.query.as_str(), p.cursor), ("", 0));
        assert!(matches!(p.current().unwrap().action, PickAction::ReopenWorkspace(id) if id == ws));
        assert!(server.workspaces[0].closed());
        assert!(server.panes.is_empty());
        assert!(matches!(server.on_picker(client, p, key(KeyCode::Enter, Mods::NONE)), Mode::Normal));
        assert_eq!(server.clients[&client].workspace, ws);
        assert_eq!(server.workspaces.len(), 1);
        assert_eq!(server.workspaces[0].stable_id, identity);
        assert!(!server.workspaces[0].closed());
        assert_eq!(server.workspaces[0].panes().len(), 1);
    }

    #[test]
    fn blank_and_informational_workspace_rows_do_not_activate_add() {
        for size in [(80, 24), (124, 36)] {
            let (tx, _rx) = crate::server::queue::channel(32);
            let mut server = crate::server::tests::server(tx);
            let (client, _peer) = crate::server::tests::client(&mut server);
            let ws = server.clients[&client].workspace;
            let c = server.clients.get_mut(&client).unwrap();
            (c.cols, c.rows) = size;
            for query in ["", "no match"] {
                let mut p = server.workspace_picker(ws);
                p.query = query.into();
                p.refilter();
                // Keep space for a filtered-out row, as when the list was refreshed.
                if query.is_empty() {
                    p.min_items = 3;
                }
                let selected = p.current().unwrap().label.clone();
                let layout = crate::server::overlays::picker_layout(&p, server.client_screen(client));
                let first = layout.list.y + if query.is_empty() { layout.row_height } else { 0 };
                let mut buf = ratatui::buffer::Buffer::empty(server.client_screen(client));
                crate::server::overlays::draw_picker(&p, buf.area, &mut buf);
                let shown: String = buf.content.iter().map(|c| c.symbol()).collect();
                if !query.is_empty() {
                    assert!(shown.contains("No matching workspaces."));
                }
                for row in first..layout.list.bottom() {
                    for button in [MouseButton::Left, MouseButton::Right] {
                        let event = InputEvent::Mouse(MouseEvent {
                            kind: MouseKind::Press(button),
                            col: layout.list.x + 2,
                            row,
                            mods: Mods::NONE,
                        });
                        let Mode::Picker(next) = server.on_picker(client, p, event) else { panic!("no action") };
                        p = next;
                        assert_eq!(p.current().unwrap().label, selected);
                        assert_eq!(p.query, query);
                        assert!(p.actions.is_none());
                        assert_eq!(server.workspaces.len(), 1);
                    }
                }
                let event = InputEvent::Mouse(MouseEvent {
                    kind: MouseKind::Press(MouseButton::Left),
                    col: layout.list.x + 2,
                    row: layout.list.bottom(),
                    mods: Mods::NONE,
                });
                let Mode::Prompt(p) = server.on_picker(client, p, event) else { panic!("Add opens its form") };
                assert!(matches!(p.action, PromptAction::NewWorkspace));
            }
        }
    }

    #[test]
    fn workspace_picker_and_add_actions_work_without_a_current_workspace() {
        let (tx, _rx) = crate::server::queue::channel(32);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        for empty in [false, true] {
            if empty {
                server.workspaces.clear();
            }
            let Mode::Picker(p) = server.action(client, Action::WorkspacePicker) else { panic!() };
            assert!(p.kind == PickerKind::Workspace);
            assert!(p.items.iter().any(|i| matches!(i.action, PickAction::AddWorkspace)));
            let Mode::Prompt(p) = server.action(client, Action::NewWorkspace) else { panic!() };
            assert!(matches!(p.action, PromptAction::NewWorkspace));
        }
    }

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
            assert!(matches!(p.current().unwrap().action, PickAction::AddWorkspace));
            if stale {
                server.workspaces[0].name = "café 日".into();
            }
            let event = InputEvent::Key(Key { code: KeyCode::Enter, mods: Mods::NONE, raw: Vec::new() });
            let Mode::Prompt(p) = server.on_picker(client, p, event) else { panic!("keep input") };
            assert_eq!(p.value, "café 日");
            assert!(matches!(p.action, PromptAction::NewWorkspace));
            assert!(p.error.is_none());
            assert_eq!(server.workspaces.len(), 1, "Add opens the form without creating");
            let event = InputEvent::Key(Key { code: KeyCode::Enter, mods: Mods::NONE, raw: Vec::new() });
            let Mode::Prompt(p) = server.on_prompt(client, p, event) else { panic!("keep input") };
            let reason = if stale { "already used" } else { "No workspace was added." };
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
        assert!(matches!(picked.action, ConfirmAction::CloseWorkspace { workspace: id, .. } if id == ws));
        assert!(!picked.yes);
        for name in ["first", "second", "floating"] {
            assert!(picked.message.lines().any(|line| line == format!("  sleep {name}")), "{}", picked.message);
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
    fn workspace_actions_offer_rename_on_closed_rows_and_close_only_on_open_rows() {
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
            PickAction::AddWorkspace,
            PickAction::UnavailableDefinition("invalid.kdl".into()),
        ] {
            let open = matches!(action, PickAction::SwitchWorkspace(_));
            let stale = matches!(action, PickAction::SwitchWorkspace(u32::MAX));
            let actions = open || matches!(action, PickAction::ReopenWorkspace(_) | PickAction::OpenDefinition(_));
            let mut p = Picker::new(
                PickerKind::Workspace,
                "Workspaces",
                vec![
                    PickItem::new("", "", action),
                    PickItem::new("scratch", "", PickAction::UnavailableDefinition("invalid.kdl".into())),
                ],
            );
            p.query = "new".into();
            p.filtered = vec![0, 1];
            let Mode::Picker(mut p) = server.on_picker(0, p, key(KeyCode::Tab, Mods::NONE)) else { panic!() };
            assert_eq!(p.actions, actions.then_some(0));
            assert_eq!(p.selected, usize::from(!actions));
            p.actions = None;
            p.selected = 0;
            let next = server.on_picker(0, p, key(KeyCode::Char('x'), Mods::CTRL));
            if stale {
                assert!(matches!(next, Mode::Picker(_)), "a stale workspace returns to the list");
            } else if open {
                assert!(
                    matches!(next, Mode::Confirm(q) if matches!(q.action, ConfirmAction::CloseWorkspace { workspace: 1, .. }))
                );
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

#[cfg(test)]
mod deletion_tests {
    use super::*;
    use crate::input::KeySpec;
    use crate::server::persistence::tests::writer;
    use crate::server::workspace_lifecycle::DeleteTarget;
    use ratatui::buffer::Buffer;

    fn key(spec: &str) -> InputEvent {
        let spec = KeySpec::parse(spec).unwrap();
        InputEvent::Key(Key { code: spec.code, mods: spec.mods, raw: Vec::new() })
    }

    #[test]
    fn global_actions_work_after_last_workspace_deletion_and_reattachment() {
        for reattach in [false, true] {
            let (tx, _rx) = crate::server::queue::channel(4096);
            let mut server = crate::server::tests::server(tx);
            let (mut client, _peer) = crate::server::tests::client(&mut server);
            let ws = server.clients[&client].workspace;
            writer(&mut server, vec![]);
            server.keep_and_delete_workspace(ws).unwrap();
            let _reattached_peer = if reattach {
                server.detach(client, "detached");
                let (stream, peer) = std::os::unix::net::UnixStream::pair().unwrap();
                client = server.next_id;
                server.new_connection(stream);
                server.attach(client, 80, 24, None);
                Some(peer)
            } else {
                None
            };
            assert!(server.workspaces.is_empty());
            assert!(matches!(server.clients[&client].mode, Mode::Picker(_)));
            server.client_input(client, b"\x1b");
            server.flush_client_input(client);
            assert!(matches!(server.clients[&client].mode, Mode::Normal));
            server.client_input(client, b"\x02?");
            assert!(matches!(server.clients[&client].mode, Mode::Help { .. }));
            server.client_input(client, b"\x1b");
            server.flush_client_input(client);
            server.client_input(client, b"\x02q");
            assert!(
                matches!(&server.clients[&client].mode, Mode::Confirm(q) if matches!(q.action, ConfirmAction::Quit))
            );
            server.client_input(client, b"\x1b");
            server.flush_client_input(client);
            server.client_input(client, b"\x02Wr");
            assert!(
                matches!(&server.clients[&client].mode, Mode::Confirm(q) if matches!(q.action, ConfirmAction::Restart))
            );
            server.client_input(client, b"\x1b");
            server.flush_client_input(client);
            assert!(matches!(server.action(client, Action::Messages), Mode::Messages { .. }));
            assert!(matches!(server.action(client, Action::CommandPalette), Mode::Picker(_)));
            assert!(matches!(server.action(client, Action::Accounts), Mode::Picker(_)));
            assert!(matches!(server.action(client, Action::AddAccount), Mode::AccountForm(_)));
            assert!(matches!(server.action(client, Action::NewWorkspace), Mode::Prompt(_)));
            assert!(matches!(server.action(client, Action::WorkspacePicker), Mode::Picker(_)));
            let before = server.bottom_bar;
            server.action(client, Action::ToggleBottomBar);
            assert_eq!(server.bottom_bar, !before);
            let before = server.sidebar_shown(&server.clients[&client]);
            server.action(client, Action::ToggleSidebar);
            assert_eq!(server.sidebar_shown(&server.clients[&client]), !before);
            server.client_input(client, b"\x02d");
            assert!(!server.clients.contains_key(&client));
            assert!(server.workspaces.is_empty());
            assert!(!server.quitting);
        }
    }

    #[test]
    fn delete_rechecks_programs_and_a_scanned_row_that_was_opened_by_another_client() {
        let (tx, _rx) = crate::server::queue::channel(4096);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        let ws = server.clients[&client].workspace;
        let Mode::Confirm(question) = server.ask_delete_workspace(DeleteTarget::Workspace(ws)) else { panic!() };
        assert_eq!(question.action.verb(), "Delete");
        let _batch = crate::pane::batch();
        server
            .spawn_pane_in(
                ws,
                None,
                PaneSpec { command: vec!["sleep".into(), "60".into()], ..Default::default() },
                Placement::Tab,
                Some("new program".into()),
            )
            .unwrap();
        let Mode::Confirm(updated) = server.on_confirm(client, question, key("y")) else { panic!() };
        assert!(!updated.yes);
        assert_eq!(updated.action.verb(), "Stop and delete");
        assert!(updated.message.contains("sleep 60"));
        assert_eq!(server.panes.len(), 1);
        assert!(server.deleted_workspaces.is_empty());
        let path = std::env::temp_dir().join(format!("kraal-delete-question-{}.kdl", crate::accounts::new_uuid()));
        std::fs::write(&path, "layout { pane; }\n").unwrap();
        let Mode::Confirm(question) = server.ask_delete_workspace(DeleteTarget::Source(path.clone())) else { panic!() };
        server.open_definition(&path).unwrap();
        let Mode::Confirm(retry) = server.on_confirm(client, question, key("y")) else { panic!() };
        assert!(retry.message.contains("opened meanwhile"));
        assert_eq!(server.panes.len(), 2);
        assert!(server.deleted_workspaces.is_empty());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn delete_confirmation_cancel_undo_and_deleted_list_work_with_keys_and_mouse_at_both_sizes() {
        for size in [(80, 24), (124, 36)] {
            let (tx, _rx) = crate::server::queue::channel(4096);
            let mut server = crate::server::tests::server(tx);
            let (client, _peer) = crate::server::tests::client(&mut server);
            let c = server.clients.get_mut(&client).unwrap();
            (c.cols, c.rows) = size;
            let ws = c.workspace;
            writer(&mut server, vec![]);
            let mut p = server.workspace_picker(ws);
            p.query = "test".into();
            p.cursor = 2;
            p.refilter();
            let Mode::Picker(mut p) = server.on_picker(client, p, key("tab")) else { panic!() };
            p.actions = Some(p.action_keys().len() - 1);
            let event = if size.0 == 80 {
                key("enter")
            } else {
                let layout = crate::server::overlays::picker_layout(&p, server.client_screen(client));
                InputEvent::Mouse(MouseEvent {
                    kind: MouseKind::Press(MouseButton::Left),
                    col: layout.list.x + 2,
                    row: layout.list.y + p.actions.unwrap() as u16,
                    mods: Default::default(),
                })
            };
            let Mode::Confirm(q) = server.on_picker(client, p, event) else { panic!() };
            assert!(!q.yes);
            assert_eq!(q.action.verb(), "Delete");
            assert!(q.message.contains("Deleted workspaces"));
            assert!(q.message.contains("Project files will be kept."));
            let mut buf = Buffer::empty(server.client_screen(client));
            crate::server::overlays::draw_confirm(&q, buf.area, &mut buf);
            let shown: String = buf.content.iter().map(|c| c.symbol()).collect();
            for text in ["Delete test?", "Cancel", "Delete"] {
                assert!(shown.contains(text), "{shown}");
            }
            let Mode::Picker(p) = server.on_confirm(client, q, key("enter")) else { panic!() };
            assert_eq!((p.query.as_str(), p.cursor), ("test", 2));
            assert_eq!(server.workspaces.len(), 1);
            let Mode::Confirm(q) = server.ask_delete_workspace(DeleteTarget::Workspace(ws)) else { panic!() };
            let Mode::Picker(p) = server.on_confirm(client, q, key("y")) else { panic!() };
            assert!(server.workspaces.is_empty());
            assert!(!server.quitting);
            assert!(p.undo.is_some());
            let mut buf = Buffer::empty(server.client_screen(client));
            crate::server::overlays::draw_picker(&p, buf.area, &mut buf);
            let shown: String = buf.content.iter().map(|c| c.symbol()).collect();
            assert!(shown.contains("Ctrl+Z Undo"), "{shown}");
            assert!(shown.contains("Deleted workspaces (1)"), "{shown}");
            let identity = p.undo.clone().unwrap();
            let Mode::Picker(restored) = server.on_picker(client, p, key("ctrl+z")) else { panic!() };
            let ws = restored.current().unwrap().action.workspace().unwrap();
            assert!(server.workspaces[0].closed());
            assert!(server.panes.is_empty());
            assert_eq!(server.workspaces[0].stable_id, identity);
            let Mode::Picker(_) = server.delete_workspace(client, DeleteTarget::Workspace(ws)) else { panic!() };
            let col = (0..size.0)
                .find(|&col| {
                    matches!(server.tab_bar_hit(client, col), Some(crate::server::render::BarItem::Toast(_, _)))
                })
                .unwrap();
            let p = server.workspace_picker(0);
            let click = InputEvent::Mouse(MouseEvent {
                kind: MouseKind::Press(MouseButton::Left),
                col,
                row: 0,
                mods: Default::default(),
            });
            let Mode::Picker(restored) = server.on_picker(client, p, click) else { panic!() };
            let ws = restored.current().unwrap().action.workspace().unwrap();
            assert!(server.workspaces[0].closed());
            server.keep_and_delete_workspace(ws).unwrap();
            let mut list = server.workspace_picker(0);
            list.select(|i| matches!(i.action, PickAction::DeletedWorkspaces));
            let Mode::Picker(deleted) = server.on_picker(client, list, key("enter")) else { panic!() };
            assert!(deleted.kind == PickerKind::DeletedWorkspaces);
            let layout = crate::server::overlays::picker_layout(&deleted, server.client_screen(client));
            let click = InputEvent::Mouse(MouseEvent {
                kind: MouseKind::Press(MouseButton::Left),
                col: layout.list.x + 2,
                row: layout.list.y + 1,
                mods: Default::default(),
            });
            let Mode::Picker(restored) = server.on_picker(client, deleted, click) else { panic!() };
            assert!(restored.message.as_ref().unwrap().contains("Workspace restored"));
            assert!(server.workspaces[0].closed());
        }
    }
}

#[cfg(test)]
mod workspace_action_tests {
    use super::*;
    use crate::input::{InputEvent, Key, KeySpec, Mods, MouseButton, MouseEvent, MouseKind};
    use crate::pane::PaneSpec;
    use crate::persist;
    use crate::protocol::Placement;
    use crate::server::{overlays, persistence, queue, tests};
    use ratatui::buffer::Buffer;

    fn key(spec: &str) -> InputEvent {
        let spec = KeySpec::parse(spec).unwrap();
        InputEvent::Key(Key { code: spec.code, mods: spec.mods, raw: Vec::new() })
    }

    #[test]
    fn duplicate_actions_target_the_selected_open_or_closed_row_with_keyboard_and_mouse() {
        for (cols, rows) in [(80, 24), (124, 36)] {
            for closed in [false, true] {
                let (tx, _rx) = crate::server::queue::channel(32);
                let mut server = crate::server::tests::server(tx);
                let (client, _peer) = crate::server::tests::client(&mut server);
                let current = server.clients[&client].workspace;
                let source = server.new_workspace("other".into(), None);
                let c = server.clients.get_mut(&client).unwrap();
                (c.cols, c.rows) = (cols, rows);
                if closed {
                    let mut kept = crate::persist::snapshot(&server).workspaces.remove(1);
                    kept.closed = true;
                    server.workspaces[1].kept = Some(kept);
                }
                for mouse in [false, true] {
                    let mut p = server.workspace_picker(current);
                    p.select(|i| i.action.workspace() == Some(source));
                    let Mode::Picker(mut p) = server.on_picker(client, p, key("tab")) else { panic!() };
                    let at = p.action_keys().iter().position(|(_, action)| *action == "Duplicate").unwrap();
                    let event = if mouse {
                        let layout = overlays::picker_layout(&p, server.client_screen(client));
                        InputEvent::Mouse(MouseEvent {
                            kind: MouseKind::Press(MouseButton::Left),
                            col: layout.list.x + 1,
                            row: layout.list.y + at as u16,
                            mods: Default::default(),
                        })
                    } else {
                        p.actions = Some(at);
                        key("enter")
                    };
                    let Mode::Prompt(p) = server.on_picker(client, p, event) else { panic!() };
                    assert!(matches!(p.action, PromptAction::DuplicateWorkspace(ws) if ws == source));
                    assert_eq!(p.value, "other-copy");
                    assert_eq!(server.clients[&client].workspace, current);
                }
            }
        }
    }

    fn choose(server: &mut Server, client: ClientId, p: Picker, action: &str, mouse: bool) -> Mode {
        let Mode::Picker(mut p) = server.on_picker(client, p, key("tab")) else { panic!() };
        assert_eq!(p.actions, Some(0));
        let at = p.action_keys().iter().position(|(_, label)| *label == action).unwrap();
        if mouse {
            let layout = overlays::picker_layout(&p, server.client_screen(client));
            server.on_picker(
                client,
                p,
                InputEvent::Mouse(MouseEvent {
                    kind: MouseKind::Press(MouseButton::Left),
                    col: layout.list.x + 2,
                    row: layout.list.y + at as u16,
                    mods: Mods::NONE,
                }),
            )
        } else {
            for _ in 0..at {
                let Mode::Picker(next) = server.on_picker(client, p, key("down")) else { panic!() };
                p = next;
            }
            server.on_picker(client, p, key("enter"))
        }
    }

    fn names(server: &Server) -> Vec<String> {
        server
            .workspace_picker(0)
            .items
            .into_iter()
            .filter(|i| i.action.workspace().is_some() || i.action.definition().is_some())
            .map(|i| i.label)
            .collect()
    }

    #[test]
    fn workspace_moves_preserve_clients_panes_and_selection_and_survive_restore() {
        let (tx, _rx) = queue::channel(32);
        let mut server = tests::server(tx);
        let (client, _peer) = tests::client(&mut server);
        let api = server.clients[&client].workspace;
        server.workspaces[0].name = "api".into();
        let (other, _peer2) = tests::client(&mut server);
        let web = server.clients[&other].workspace;
        server.workspaces[1].name = "web".into();
        let docs = server.new_workspace("docs".into(), None);
        let mut closed = persist::snapshot(&server).workspaces.remove(2);
        closed.closed = true;
        server.workspaces[2].kept = Some(closed);
        let _batch = crate::pane::batch();
        let a = server.spawn_pane_in(api, None, PaneSpec::default(), Placement::Tab, None).unwrap();
        let b = server.spawn_pane_in(web, None, PaneSpec::default(), Placement::Tab, None).unwrap();
        let identities: Vec<_> = server.workspaces.iter().map(|w| w.stable_id.clone()).collect();
        let tabs: Vec<_> = server.workspaces.iter().map(|w| (w.id, w.active, w.tab().map(|t| t.id))).collect();
        let saved = persistence::tests::writer(&mut server, vec![]);
        let mut other_list = server.workspace_picker(web);
        other_list.select(|i| i.action.workspace() == Some(api));
        server.clients.get_mut(&other).unwrap().mode = Mode::Picker(other_list);
        let mut p = server.workspace_picker(api);
        p.select(|i| i.action.workspace() == Some(docs));
        let Mode::Picker(p) = choose(&mut server, client, p, "Move up", false) else { panic!() };
        assert_eq!(names(&server), ["api", "docs", "web"]);
        assert_eq!(p.current().unwrap().action.workspace(), Some(docs));
        assert!(p.actions.is_none());
        let Mode::Picker(p) = choose(&mut server, client, p, "Move up", true) else { panic!() };
        assert_eq!(names(&server), ["docs", "api", "web"]);
        assert!(!p.action_keys().iter().any(|(_, a)| *a == "Move up"));
        assert_eq!((server.clients[&client].workspace, server.clients[&other].workspace), (api, web));
        assert_eq!((server.client_focus(client), server.client_focus(other)), (Some(a), Some(b)));
        for (id, active, tab) in tabs {
            let w = &server.workspaces[server.ws_index(id).unwrap()];
            assert_eq!((w.active, w.tab().map(|t| t.id)), (active, tab));
        }
        assert_eq!(server.panes.len(), 2);
        let Mode::Picker(other_list) = &server.clients[&other].mode else { panic!() };
        assert_eq!(other_list.current().unwrap().action.workspace(), Some(api));
        assert_eq!(
            other_list.items.iter().take(3).map(|i| i.label.as_str()).collect::<Vec<_>>(),
            ["docs", "api", "web"]
        );
        let Mode::Picker(p) = choose(&mut server, client, p, "Move down", false) else { panic!() };
        assert_eq!(p.current().unwrap().action.workspace(), Some(docs));
        let snap = saved.lock().unwrap().last().unwrap().clone();
        assert_eq!(
            snap["workspaces"].as_array().unwrap().iter().map(|w| w["name"].as_str().unwrap()).collect::<Vec<_>>(),
            ["api", "docs", "web"]
        );
        let (tx, _rx2) = queue::channel(32);
        let mut restored = tests::server(tx);
        persist::apply(&mut restored, serde_json::from_value(snap).unwrap()).unwrap();
        assert_eq!(names(&restored), ["api", "docs", "web"]);
        assert_eq!(
            restored.workspaces.iter().map(|w| &w.stable_id).collect::<Vec<_>>(),
            [&identities[0], &identities[2], &identities[1]]
        );
        assert!(restored.workspaces[1].closed());
        restored.reopen_workspace(restored.workspaces[1].id).unwrap();
        assert_eq!(names(&restored), ["api", "docs", "web"]);
    }

    #[test]
    fn exporting_over_an_ordered_source_keeps_identity_and_allows_the_next_move() {
        use crate::persist::WorkspaceKey;
        use crate::server::ui::{Prompt, PromptAction};

        // Export uses the default definitions directory, so give this test its own HOME.
        if std::env::var_os("WORKSPACE_EXPORT_ORDER_CHILD").is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "server::input::pickers::workspace_order_tests::exporting_over_an_ordered_source_keeps_identity_and_allows_the_next_move"])
                .env("WORKSPACE_EXPORT_ORDER_CHILD", "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let (tx, _rx) = queue::channel(32);
        let mut server = tests::server(tx);
        let (client, _peer) = tests::client(&mut server);
        let a = server.clients[&client].workspace;
        server.workspaces[0].name = "a".into();
        let b = server.new_workspace("b".into(), None);
        let dir = crate::paths::workspaces_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.kdl");
        let source = "layout { pane command=\"true\" start_suspended=true; }\n";
        std::fs::write(&path, source).unwrap();
        server.config.workspace_dirs = vec![dir.display().to_string()];
        let x = WorkspaceKey::Source(crate::workspace::definition_path(&path));
        let a_key = server.workspace_key(&PickAction::SwitchWorkspace(a)).unwrap();
        let b_key = server.workspace_key(&PickAction::SwitchWorkspace(b)).unwrap();
        let saved = persistence::tests::writer(&mut server, vec![]);
        server.reorder_workspace(client, x.clone(), b_key.clone(), true);
        server.reorder_workspace(client, x.clone(), a_key.clone(), true);
        assert_eq!(names(&server), ["x", "a", "b"]);
        let prompt = Prompt::new("Save workspace layout as", "x", PromptAction::SaveWorkspace(a));
        let Mode::Confirm(q) = server.on_prompt(client, prompt, key("enter")) else { panic!() };
        assert_eq!(q.action.verb(), "Replace file");
        assert!(matches!(server.on_confirm(client, q, key("y")), Mode::Normal));
        assert_eq!(names(&server), ["a", "b"]);
        let Mode::Picker(p) = server.reorder_workspace(client, a_key.clone(), b_key.clone(), false) else { panic!() };
        assert!(p.message.is_none());
        assert_eq!(names(&server), ["b", "a"]);
        assert_eq!(server.workspace_order, [b_key, a_key]);
        assert_eq!(std::fs::read_to_string(path.with_extension("kdl.bak")).unwrap(), source);
        let snap = saved.lock().unwrap().last().unwrap().clone();
        let (tx, _rx2) = queue::channel(32);
        let mut restored = tests::server(tx);
        restored.config.workspace_dirs = server.config.workspace_dirs.clone();
        persist::apply(&mut restored, serde_json::from_value(snap).unwrap()).unwrap();
        assert_eq!(names(&restored), ["b", "a"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concurrent_workspace_moves_preserve_pending_menus_and_reject_changed_neighbors() {
        for changed_neighbor in [false, true] {
            let (tx, _rx) = queue::channel(32);
            let mut server = tests::server(tx);
            let (client, _peer) = tests::client(&mut server);
            let current = server.clients[&client].workspace;
            server.workspaces[0].name = "api".into();
            let (other, _peer2) = tests::client(&mut server);
            let other_ws = server.clients[&other].workspace;
            server.workspaces[1].name = "web".into();
            let middle = server.new_workspace("middle".into(), None);
            let docs = server.new_workspace("docs".into(), None);
            let mut closed = persist::snapshot(&server).workspaces.remove(3);
            closed.closed = true;
            server.workspaces[3].kept = Some(closed);
            let saved = persistence::tests::writer(&mut server, vec![]);
            let mut p = server.workspace_picker(current);
            p.select(|i| i.action.workspace() == Some(docs));
            let Mode::Picker(mut p) = server.on_picker(client, p, key("tab")) else { panic!() };
            let at = p.action_keys().iter().position(|(_, label)| *label == "Move up").unwrap();
            for _ in 0..at {
                let Mode::Picker(next) = server.on_picker(client, p, key("down")) else { panic!() };
                p = next;
            }
            server.clients.get_mut(&client).unwrap().mode = Mode::Picker(p);
            let mut p = server.workspace_picker(other_ws);
            p.select(|i| i.action.workspace() == Some(if changed_neighbor { middle } else { other_ws }));
            assert!(matches!(choose(&mut server, other, p, "Move up", false), Mode::Picker(_)));
            let Mode::Picker(p) = std::mem::replace(&mut server.clients.get_mut(&client).unwrap().mode, Mode::Normal)
            else {
                panic!()
            };
            assert_eq!(p.action_keys()[p.actions.expect("keep the pending menu")].1, "Move up");
            assert_eq!(p.current().unwrap().action.workspace(), Some(docs));
            let mut dismissed = server.workspace_picker(current);
            dismissed.restore(p.state());
            let Mode::Picker(dismissed) = server.on_picker(client, dismissed, key("esc")) else { panic!() };
            assert!(dismissed.actions.is_none());
            assert_eq!(dismissed.current().unwrap().action.workspace(), Some(docs));
            assert_eq!(
                dismissed.items.iter().map(|i| &i.label).collect::<Vec<_>>(),
                server.workspace_picker(current).items.iter().map(|i| &i.label).collect::<Vec<_>>()
            );
            let Mode::Picker(p) = server.on_picker(client, p, key("enter")) else { panic!() };
            assert_eq!(p.current().unwrap().action.workspace(), Some(docs));
            assert!(p.actions.is_none());
            if changed_neighbor {
                assert_eq!(p.message.as_deref(), Some("Order changed. Choose Move up or Move down again."));
                assert_eq!(names(&server), ["api", "middle", "web", "docs"]);
                assert_eq!(saved.lock().unwrap().len(), 1);
            } else {
                assert_eq!(names(&server), ["web", "api", "docs", "middle"]);
                assert_eq!(saved.lock().unwrap().len(), 2);
            }
            assert!(server.workspaces[server.ws_index(docs).unwrap()].closed());
            assert!(server.panes.is_empty());
            assert_eq!((server.clients[&client].workspace, server.clients[&other].workspace), (current, other_ws));
        }
    }

    #[test]
    fn pending_workspace_moves_reject_deleted_targets_and_neighbors() {
        for delete_target in [false, true] {
            let (tx, _rx) = queue::channel(32);
            let mut server = tests::server(tx);
            let (client, _peer) = tests::client(&mut server);
            let current = server.clients[&client].workspace;
            let (other, _peer2) = tests::client(&mut server);
            let neighbor = server.new_workspace("neighbor".into(), None);
            let target = server.new_workspace("target".into(), None);
            let saved = persistence::tests::writer(&mut server, vec![]);
            let mut p = server.workspace_picker(current);
            p.select(|i| i.action.workspace() == Some(target));
            p.actions = Some(p.action_keys().iter().position(|(_, label)| *label == "Move up").unwrap());
            server.clients.get_mut(&client).unwrap().mode = Mode::Picker(p);
            let deleted = if delete_target { target } else { neighbor };
            assert!(matches!(
                server.delete_workspace(other, crate::server::workspace_lifecycle::DeleteTarget::Workspace(deleted)),
                Mode::Picker(_)
            ));
            // Reusing the name must not retarget the pending move either.
            server.new_workspace(if delete_target { "target" } else { "neighbor" }.into(), None);
            server.refresh_workspace_pickers();
            let before = names(&server);
            let writes = saved.lock().unwrap().len();
            let Mode::Picker(p) = std::mem::replace(&mut server.clients.get_mut(&client).unwrap().mode, Mode::Normal)
            else {
                panic!()
            };
            assert_eq!(p.action_keys()[p.actions.unwrap()].1, "Move up");
            let Mode::Picker(p) = server.on_picker(client, p, key("enter")) else { panic!() };
            assert_eq!(p.message.as_deref(), Some("Workspace unavailable. Select a workspace to move."));
            assert!(p.actions.is_none());
            if !delete_target {
                assert_eq!(p.current().unwrap().action.workspace(), Some(target));
            }
            assert_eq!(names(&server), before);
            assert_eq!(saved.lock().unwrap().len(), writes);
            assert_eq!(server.clients[&client].workspace, current);
            assert!(server.panes.is_empty());
        }
    }

    #[test]
    fn workspace_search_clears_without_moving_and_keeps_the_same_named_row_at_both_sizes() {
        for size in [(80, 24), (124, 36)] {
            let (tx, _rx) = queue::channel(32);
            let mut server = tests::server(tx);
            let (client, _peer) = tests::client(&mut server);
            server.workspaces[0].name = "same".into();
            let ws = server.new_workspace("other".into(), None);
            server.workspaces[1].name = "same".into();
            let c = server.clients.get_mut(&client).unwrap();
            (c.cols, c.rows) = size;
            let saved = persistence::tests::writer(&mut server, vec![]);
            let mut p = server.workspace_picker(0);
            p.query = "same".into();
            p.cursor = 2;
            p.refilter();
            p.select(|i| i.action.workspace() == Some(ws));
            let keys = p.action_keys();
            assert!(keys.contains(&("", "Clear search to reorder")));
            assert!(!keys.iter().any(|(_, a)| matches!(*a, "Move up" | "Move down")));
            p.actions = Some(keys.iter().position(|(_, a)| *a == "Clear search to reorder").unwrap());
            let mut buf = Buffer::empty(server.client_screen(client));
            overlays::draw_picker(&p, buf.area, &mut buf);
            let text: String = buf.content.iter().map(|c| c.symbol()).collect();
            assert!(text.contains("Clear search to reorder"));
            let Mode::Picker(p) = server.on_picker(client, p, key("enter")) else { panic!() };
            assert_eq!((p.query.as_str(), p.cursor), ("", 0));
            assert_eq!(p.current().unwrap().action.workspace(), Some(ws));
            assert!(!p.action_keys().iter().any(|(_, a)| *a == "Move down"));
            assert!(server.workspace_order.is_empty());
            assert!(saved.lock().unwrap().is_empty());
            let Mode::Picker(p) = choose(&mut server, client, p, "Move up", false) else { panic!() };
            assert_eq!(p.current().unwrap().action.workspace(), Some(ws));
            assert_eq!(server.workspaces[0].id, ws);
        }
    }

    #[test]
    fn workspace_order_failures_roll_back_and_retry_without_switching_or_repeating_a_move() {
        for rollback in [Ok(()), Err("still full".into())] {
            let (tx, _rx) = queue::channel(32);
            let mut server = tests::server(tx);
            let (client, _peer) = tests::client(&mut server);
            let current = server.clients[&client].workspace;
            let ws = server.new_workspace("docs".into(), None);
            let saved =
                persistence::tests::writer(&mut server, vec![Err("disk full".into()), rollback.clone(), Ok(())]);
            let mut p = server.workspace_picker(current);
            p.select(|i| i.action.workspace() == Some(ws));
            let Mode::Confirm(q) = choose(&mut server, client, p, "Move up", false) else { panic!() };
            assert!(q.message.contains("disk full"));
            assert_eq!(q.action.verb(), "Retry");
            if rollback.is_err() {
                assert!(q.message.contains("The saved order may differ. Fix the save error and retry."));
                assert!(server.persist_dirty);
            } else {
                assert!(q.message.contains("Couldn't save the order. Previous order kept."));
            }
            assert_eq!(names(&server), ["test", "docs"]);
            assert!(server.workspace_order.is_empty());
            assert_eq!(server.clients[&client].workspace, current);
            assert_eq!(saved.lock().unwrap().last().unwrap()["workspaces"][0]["name"], "test");
            let Mode::Picker(p) = server.on_confirm(client, q, key("y")) else { panic!() };
            assert_eq!(p.current().unwrap().action.workspace(), Some(ws));
            assert_eq!(names(&server), ["docs", "test"]);
            let wanted = server.workspace_key(&PickAction::SwitchWorkspace(ws)).unwrap();
            let neighbor = server.workspace_key(&PickAction::SwitchWorkspace(current)).unwrap();
            assert!(matches!(server.reorder_workspace(client, wanted, neighbor, true), Mode::Picker(_)));
            assert_eq!(names(&server), ["docs", "test"]);
            assert_eq!(server.clients[&client].workspace, current);
        }
    }

    #[test]
    fn workspace_order_error_cancel_keeps_the_row_and_shows_retry_at_both_sizes() {
        for size in [(80, 24), (124, 36)] {
            let (tx, _rx) = queue::channel(32);
            let mut server = tests::server(tx);
            let (client, _peer) = tests::client(&mut server);
            let current = server.clients[&client].workspace;
            let ws = server.new_workspace("docs".into(), None);
            let c = server.clients.get_mut(&client).unwrap();
            (c.cols, c.rows) = size;
            let saved = persistence::tests::writer(&mut server, vec![Err("disk full".into()), Ok(())]);
            let mut p = server.workspace_picker(current);
            p.select(|i| i.action.workspace() == Some(ws));
            let Mode::Confirm(q) = choose(&mut server, client, p, "Move up", false) else { panic!() };
            let mut buf = Buffer::empty(server.client_screen(client));
            overlays::draw_confirm(&q, buf.area, &mut buf);
            let text: String = buf.content.iter().map(|c| c.symbol()).collect();
            for message in ["Couldn't save the order.", "Previous order kept.", "disk full", "Retry", "Cancel"] {
                assert!(text.contains(message), "{text}");
            }
            let count = saved.lock().unwrap().len();
            let Mode::Picker(p) = server.on_confirm(client, q, key("esc")) else { panic!() };
            assert_eq!(p.current().unwrap().action.workspace(), Some(ws));
            assert_eq!(names(&server), ["test", "docs"]);
            assert_eq!(server.clients[&client].workspace, current);
            assert_eq!(saved.lock().unwrap().len(), count);
        }
    }
}
