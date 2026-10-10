//! Keyboard and mouse input from attached clients: key bindings, modes and actions.

use std::time::{Duration, Instant};

use anyhow::Context;

use alacritty_terminal::selection::SelectionType;

use super::render::{BarItem, SideItem};
use super::ui::{ConfirmAction, Drag, Mode, PickAction, PickItem, Picker, PickerKind, Prompt, PromptAction};
use super::{ClientId, Server, Tab};
use crate::agent::AgentState;
use crate::config::Action;
use crate::input::{
    InputEvent, Key, KeyCode, MouseButton, MouseEvent, MouseKind, encode_alternate_scroll, encode_key, encode_key_code,
    encode_mouse, encode_paste,
};
use crate::layout::{Dir, PaneId, neighbour};
use crate::pane::{PaneSpec, osc52};
use crate::protocol::Placement;
use crate::{paths, workspace};

impl Server {
    pub(super) fn client_input(&mut self, id: ClientId, bytes: &[u8]) {
        let Some(c) = self.clients.get_mut(&id) else { return };
        if !c.attached {
            return;
        }
        c.last_input = Instant::now();
        let events = c.parser.feed(bytes);
        c.pending_since = c.parser.has_pending().then(Instant::now);
        if self.primary != Some(id) {
            self.primary = Some(id);
            self.relayout();
        }
        for ev in events {
            self.client_event(id, ev);
        }
    }

    pub(super) fn flush_client_input(&mut self, id: ClientId) {
        let Some(c) = self.clients.get_mut(&id) else { return };
        c.pending_since = None;
        let events = c.parser.flush();
        for ev in events {
            self.client_event(id, ev);
        }
    }

    fn client_event(&mut self, id: ClientId, ev: InputEvent) {
        if let InputEvent::Raw(raw) = &ev
            && self.terminal_reply(raw)
        {
            return;
        }
        match &ev {
            InputEvent::FocusGained | InputEvent::FocusLost => {
                let gained = matches!(ev, InputEvent::FocusGained);
                if let Some(c) = self.clients.get_mut(&id) {
                    c.terminal_focused = gained;
                }
                if gained {
                    self.mark_seen_focused(id);
                }
                if let Some(p) = self.client_focus(id)
                    && let Some(pane) = self.panes.get_mut(&p)
                    && pane.input_modes().focus_events
                {
                    pane.write_input(if gained { b"\x1b[I".to_vec() } else { b"\x1b[O".to_vec() });
                }
                return;
            }
            _ => {}
        }
        let Some(c) = self.clients.get_mut(&id) else { return };
        let mode = std::mem::replace(&mut c.mode, Mode::Normal);
        let next = match mode {
            Mode::Normal => self.on_normal(id, ev),
            Mode::Prefix => self.on_prefix(id, ev),
            Mode::Scroll => self.on_scroll(id, ev),
            Mode::Search { query, found } => self.on_search(id, query, found, ev),
            Mode::Picker(p) => self.on_picker(id, p, ev),
            Mode::Prompt(p) => self.on_prompt(id, p, ev),
            Mode::Help { scroll } => self.on_help(id, scroll, ev),
            Mode::Confirm { message, action } => self.on_confirm(id, message, action, ev),
        };
        if let Some(c) = self.clients.get_mut(&id) {
            c.mode = next;
            c.dirty = true;
        }
        self.mark_dirty_client(id);
    }

    /// Replies from the outer terminal to queries the server sent (colours, DA, CPR).
    fn terminal_reply(&mut self, raw: &[u8]) -> bool {
        let s = String::from_utf8_lossy(raw);
        for (code, fg) in [("\x1b]10;", true), ("\x1b]11;", false)] {
            if let Some(rest) = s.strip_prefix(code) {
                let value = rest.trim_end_matches(['\x07', '\\']).trim_end_matches('\x1b').to_string();
                let (mut f, mut b) = self.outer_colors.clone().unwrap_or_default();
                if fg {
                    f = value;
                } else {
                    b = value;
                }
                self.outer_colors = Some((f.clone(), b.clone()));
                crate::pane::set_outer_colors(&f, &b);
                return true;
            }
        }
        s.starts_with("\x1b]")
            || s.starts_with("\x1bP")
            || (s.starts_with("\x1b[?") && (s.ends_with('c') || s.ends_with('R')))
    }

    // ---- modes ----------------------------------------------------------------------------

    fn on_normal(&mut self, id: ClientId, ev: InputEvent) -> Mode {
        match ev {
            InputEvent::Key(k) => {
                if self.keymap.is_prefix(&k) {
                    return Mode::Prefix;
                }
                if let Some(a) = self.keymap.direct(&k) {
                    return self.action(id, a);
                }
                self.key_to_pane(id, &k)
            }
            InputEvent::Mouse(m) => {
                self.on_mouse(id, m, false);
                Mode::Normal
            }
            InputEvent::Paste(data) => self.paste_or_upload(id, data),
            InputEvent::Raw(raw) => {
                if let Some(p) = self.client_focus(id)
                    && let Some(pane) = self.panes.get_mut(&p)
                {
                    pane.write_input(raw);
                }
                Mode::Normal
            }
            _ => Mode::Normal,
        }
    }

    fn on_prefix(&mut self, id: ClientId, ev: InputEvent) -> Mode {
        match ev {
            InputEvent::Key(k) => {
                if k.code == KeyCode::Esc {
                    return Mode::Normal;
                }
                match self.keymap.prefixed(&k) {
                    Some(a) => self.action(id, a),
                    None => Mode::Normal,
                }
            }
            InputEvent::Mouse(m) => {
                self.on_mouse(id, m, false);
                Mode::Normal
            }
            _ => Mode::Prefix,
        }
    }

    fn on_scroll(&mut self, id: ClientId, ev: InputEvent) -> Mode {
        let Some(p) = self.client_focus(id) else { return Mode::Normal };
        match ev {
            InputEvent::Key(k) => {
                let Some(pane) = self.panes.get_mut(&p) else { return Mode::Normal };
                if !pane.is_terminal() {
                    return self.scroll_other(id, p, &k);
                }
                pane.copy_mode(true);
                let page = pane.rows.max(2) as i32 - 1;
                let half = (pane.rows / 2).max(1) as i32;
                use alacritty_terminal::vi_mode::ViMotion as V;
                match (k.code, k.mods.ctrl) {
                    (KeyCode::Up, _) | (KeyCode::Char('k'), false) => pane.vi_motion(V::Up),
                    (KeyCode::Down, _) | (KeyCode::Char('j'), false) => pane.vi_motion(V::Down),
                    (KeyCode::Left, _) | (KeyCode::Char('h'), false) => pane.vi_motion(V::Left),
                    (KeyCode::Right, _) | (KeyCode::Char('l'), false) => pane.vi_motion(V::Right),
                    (KeyCode::Char('w'), false) => pane.vi_motion(V::SemanticRight),
                    (KeyCode::Char('b'), false) => pane.vi_motion(V::SemanticLeft),
                    (KeyCode::Char('e'), false) => pane.vi_motion(V::SemanticRightEnd),
                    (KeyCode::Char('W'), false) => pane.vi_motion(V::WordRight),
                    (KeyCode::Char('B'), false) => pane.vi_motion(V::WordLeft),
                    (KeyCode::Char('E'), false) => pane.vi_motion(V::WordRightEnd),
                    (KeyCode::Char('0'), false) | (KeyCode::Home, _) => pane.vi_motion(V::First),
                    (KeyCode::Char('^'), false) => pane.vi_motion(V::FirstOccupied),
                    (KeyCode::Char('$'), false) | (KeyCode::End, _) => pane.vi_motion(V::Last),
                    (KeyCode::Char('H'), false) => pane.vi_motion(V::High),
                    (KeyCode::Char('M'), false) => pane.vi_motion(V::Middle),
                    (KeyCode::Char('L'), false) => pane.vi_motion(V::Low),
                    (KeyCode::Char('{'), false) => pane.vi_motion(V::ParagraphUp),
                    (KeyCode::Char('}'), false) => pane.vi_motion(V::ParagraphDown),
                    (KeyCode::Char('%'), false) => pane.vi_motion(V::Bracket),
                    (KeyCode::Char('g'), false) => pane.vi_edge(true),
                    (KeyCode::Char('G'), false) => pane.vi_edge(false),
                    (KeyCode::PageUp, _) | (KeyCode::Char('b'), true) => pane.vi_scroll(page),
                    (KeyCode::PageDown, _) | (KeyCode::Char('f'), true) | (KeyCode::Char(' '), false) => {
                        pane.vi_scroll(-page)
                    }
                    (KeyCode::Char('u'), true) => pane.vi_scroll(half),
                    (KeyCode::Char('d'), true) => pane.vi_scroll(-half),
                    (KeyCode::Char('y'), true) => pane.scroll(1),
                    (KeyCode::Char('e'), true) => pane.scroll(-1),
                    (KeyCode::Char('v'), false) => pane.vi_select(SelectionType::Simple),
                    (KeyCode::Char('V'), false) => pane.vi_select(SelectionType::Lines),
                    (KeyCode::Char('v'), true) => pane.vi_select(SelectionType::Block),
                    (KeyCode::Char('y'), false) | (KeyCode::Enter, _) => {
                        if let Some(text) = pane.selection_text() {
                            self.copy_to_client(id, &text);
                        }
                        let pane = self.panes.get_mut(&p).unwrap();
                        pane.end_search();
                        pane.copy_mode(false);
                        return Mode::Normal;
                    }
                    (KeyCode::Char('/'), false) => {
                        return Mode::Search { query: String::new(), found: false };
                    }
                    (KeyCode::Char('n'), false) => {
                        pane.search_next(true);
                    }
                    (KeyCode::Char('N'), false) => {
                        pane.search_next(false);
                    }
                    (KeyCode::Esc, _) if pane.has_selection() => pane.clear_selection(),
                    (KeyCode::Esc, _) | (KeyCode::Char('q'), false) | (KeyCode::Char('c'), true) => {
                        pane.end_search();
                        pane.copy_mode(false);
                        return Mode::Normal;
                    }
                    _ => {}
                }
                Mode::Scroll
            }
            InputEvent::Mouse(m) => {
                self.on_mouse(id, m, true);
                Mode::Scroll
            }
            _ => Mode::Scroll,
        }
    }

    /// Scrolling a preview or file manager pane (no cursor there).
    fn scroll_other(&mut self, _id: ClientId, p: PaneId, k: &Key) -> Mode {
        let Some(pane) = self.panes.get_mut(&p) else { return Mode::Normal };
        let half = (pane.rows / 2).max(1) as i32;
        match (k.code, k.mods.ctrl) {
            (KeyCode::Up, _) | (KeyCode::Char('k'), false) => pane.scroll(1),
            (KeyCode::Down, _) | (KeyCode::Char('j'), false) => pane.scroll(-1),
            (KeyCode::PageUp, _) | (KeyCode::Char('b'), true) => pane.scroll_page(true),
            (KeyCode::PageDown, _) | (KeyCode::Char(' '), false) | (KeyCode::Char('f'), true) => {
                pane.scroll_page(false)
            }
            (KeyCode::Char('u'), true) => pane.scroll(half),
            (KeyCode::Char('d'), true) => pane.scroll(-half),
            (KeyCode::Char('g'), false) | (KeyCode::Home, _) => pane.scroll_to_top(),
            (KeyCode::Char('G'), false) | (KeyCode::End, _) => pane.scroll_to_bottom(),
            (KeyCode::Esc, _) | (KeyCode::Char('q'), false) => return Mode::Normal,
            _ => {}
        }
        Mode::Scroll
    }

    fn on_search(&mut self, id: ClientId, mut query: String, mut found: bool, ev: InputEvent) -> Mode {
        let Some(p) = self.client_focus(id) else { return Mode::Normal };
        let Some(pane) = self.panes.get_mut(&p) else { return Mode::Normal };
        match ev {
            InputEvent::Key(k) => match (k.code, k.mods.ctrl) {
                (KeyCode::Esc, _) | (KeyCode::Char('c'), true) | (KeyCode::Char('g'), true) => {
                    pane.end_search();
                    pane.clear_selection();
                    return Mode::Scroll;
                }
                (KeyCode::Enter, _) => return Mode::Scroll,
                (KeyCode::Char('n'), true) | (KeyCode::Up, _) => found = pane.search_next(true),
                (KeyCode::Char('p'), true) | (KeyCode::Down, _) => found = pane.search_next(false),
                (KeyCode::Backspace, _) => {
                    query.pop();
                    found = pane.search(&query);
                }
                (KeyCode::Char('u'), true) => {
                    query.clear();
                    found = pane.search(&query);
                }
                (KeyCode::Char(ch), false) if !k.mods.alt => {
                    query.push(ch);
                    found = pane.search(&query);
                }
                _ => {}
            },
            InputEvent::Paste(data) => {
                query.push_str(String::from_utf8_lossy(&data).lines().next().unwrap_or(""));
                found = pane.search(&query);
            }
            InputEvent::Mouse(m) => {
                self.on_mouse(id, m, true);
            }
            _ => {}
        }
        Mode::Search { query, found }
    }

    fn on_picker(&mut self, id: ClientId, mut p: Picker, ev: InputEvent) -> Mode {
        match ev {
            InputEvent::Key(k) => match (k.code, k.mods.ctrl) {
                (KeyCode::Esc, _) | (KeyCode::Char('c'), true) | (KeyCode::Char('g'), true) => return Mode::Normal,
                (KeyCode::Enter, _) => {
                    let Some(item) = p.current().cloned() else { return Mode::Picker(p) };
                    return self.pick(id, &p, item);
                }
                (KeyCode::Up, _) | (KeyCode::Char('p'), true) | (KeyCode::Char('k'), true) | (KeyCode::BackTab, _) => {
                    p.move_by(-1)
                }
                (KeyCode::Down, _) | (KeyCode::Char('n'), true) | (KeyCode::Char('j'), true) | (KeyCode::Tab, _) => {
                    p.move_by(1)
                }
                (KeyCode::PageUp, _) => p.move_by(-10),
                (KeyCode::PageDown, _) => p.move_by(10),
                (KeyCode::Backspace, _) => {
                    p.query.pop();
                    p.refilter();
                }
                (KeyCode::Char('u'), true) => {
                    p.query.clear();
                    p.refilter();
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
                (KeyCode::Char('l'), true) if p.kind == PickerKind::Agent => {
                    if let Some(PickAction::AgentLaunch { agent, account }) = p.current().map(|i| i.action.clone())
                        && let Some(a) = self.accounts.get(&agent, &account)
                    {
                        if let Err(e) = self.open_login(id, &a) {
                            self.toast(id, format!("{e:#}"));
                        }
                        return Mode::Normal;
                    }
                }
                (KeyCode::Char('x'), true) if p.kind == PickerKind::Agent => {
                    if let Some(PickAction::AgentLaunch { agent, account }) = p.current().map(|i| i.action.clone()) {
                        let result = crate::accounts::Accounts::load().and_then(|mut all| {
                            let removed = all.remove(&agent, &account);
                            all.save()?;
                            Ok(removed)
                        });
                        match result {
                            Ok(true) => {
                                self.reload_accounts();
                                self.toast(id, format!("forgot {agent}·{account} (its directory is kept)"));
                                return Mode::Picker(self.agent_picker(id, None));
                            }
                            Ok(false) => self.toast(id, "the default account cannot be forgotten"),
                            Err(e) => self.toast(id, format!("{e:#}")),
                        }
                    }
                }
                (KeyCode::Char('r'), true) if p.kind == PickerKind::Ssh => {
                    let target = match p.current().map(|i| i.action.clone()) {
                        Some(PickAction::SshGroup(g)) => format!("@{g}"),
                        Some(PickAction::Ssh(name)) => name,
                        _ => return Mode::Picker(p),
                    };
                    return Mode::Prompt(Prompt::new(
                        &format!("Run on {target} (results in a window)"),
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
                (KeyCode::Char('e'), true) if p.kind == PickerKind::Ssh => {
                    self.edit_servers(id);
                    return Mode::Normal;
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
                        return Mode::Confirm {
                            message: format!("Close workspace \"{name}\" and all its panes?"),
                            action: ConfirmAction::CloseWorkspace(ws),
                        };
                    }
                }
                (KeyCode::Char(ch), false) if !k.mods.alt => {
                    p.query.push(ch);
                    p.selected = 0;
                    p.refilter();
                }
                _ => {}
            },
            InputEvent::Paste(data) => {
                let text = String::from_utf8_lossy(&data);
                p.query.push_str(text.lines().next().unwrap_or(""));
                p.refilter();
            }
            InputEvent::Mouse(MouseEvent { kind: MouseKind::ScrollUp, .. }) => p.move_by(-1),
            InputEvent::Mouse(MouseEvent { kind: MouseKind::ScrollDown, .. }) => p.move_by(1),
            _ => {}
        }
        Mode::Picker(p)
    }

    fn pick(&mut self, id: ClientId, p: &Picker, item: PickItem) -> Mode {
        match item.action {
            PickAction::SwitchWorkspace(ws) => self.show_workspace(id, ws),
            PickAction::OpenDefinition(name) => match self.open_workspace(&name) {
                Ok(ws) => self.show_workspace(id, ws),
                Err(e) => self.toast(id, format!("{e:#}")),
            },
            PickAction::NewWorkspace => {
                let name = p.query.trim().to_string();
                let ws = self.new_workspace(name, None);
                if let Err(e) = self.spawn_pane_in(ws, None, PaneSpec::default(), Placement::Tab, None) {
                    self.toast(id, format!("{e:#}"));
                }
                self.show_workspace(id, ws);
            }
            PickAction::Ssh(spec) => self.connect(id, &spec, Placement::Auto),
            PickAction::SshGroup(group) => match self.open_group(&group, &[]) {
                Ok(ws) => self.show_workspace(id, ws),
                Err(e) => self.toast(id, format!("{e:#}")),
            },
            PickAction::FocusPane(pane) => self.reveal(id, pane),
            PickAction::FilesSet { pane, side, target } => self.files_set(pane, side, &target),
            PickAction::AgentLaunch { agent, account } => {
                let ws = self.clients.get(&id).map(|c| c.workspace).unwrap_or(0);
                let focus = self.client_focus(id);
                if let Err(e) = self.launch_agent(ws, focus, &agent, Some(&account), Placement::Auto, None) {
                    self.toast(id, format!("{e:#}"));
                }
            }
            PickAction::AgentSwitch { pane, agent, account } => {
                let result = self
                    .accounts
                    .get(&agent, &account)
                    .context("no such account")
                    .and_then(|a| self.switch_account(pane, &a));
                if let Err(e) = result {
                    self.toast(id, format!("{e:#}"));
                }
            }
            PickAction::AgentWorktree { agent } => {
                return Mode::Prompt(Prompt::new(
                    &format!("Name of the worktree for {agent} (branch kraal/<name>)"),
                    "",
                    PromptAction::NewWorktree { agent },
                ));
            }
            PickAction::Worktree { pane, op } => return self.worktree_op(id, pane, &op),
            PickAction::AgentAdd { agent } => {
                return Mode::Prompt(Prompt::new(
                    &format!("Name of the new {agent} account (e.g. pro, perso2)"),
                    "",
                    PromptAction::AddAccount { agent },
                ));
            }
            PickAction::None => return Mode::Picker(Picker::new(p.kind, &p.title, p.items.clone())),
        }
        Mode::Normal
    }

    fn on_prompt(&mut self, id: ClientId, mut p: Prompt, ev: InputEvent) -> Mode {
        match ev {
            InputEvent::Key(k) => match (k.code, k.mods.ctrl) {
                (KeyCode::Esc, _) | (KeyCode::Char('c'), true) | (KeyCode::Char('g'), true) => {
                    // A cancelled drop still pastes the path, as without kraal.
                    if let PromptAction::Upload { raw, pane, .. } = p.action
                        && let Some(pn) = self.panes.get_mut(&pane)
                    {
                        let bytes = encode_paste(&raw, pn.input_modes());
                        pn.write_input(bytes);
                    }
                    return Mode::Normal;
                }
                (KeyCode::Enter, _) => {
                    self.commit_prompt(id, p);
                    return Mode::Normal;
                }
                (KeyCode::Backspace, _) => {
                    p.value.pop();
                }
                (KeyCode::Char('u'), true) => p.value.clear(),
                (KeyCode::Char('w'), true) => {
                    let trimmed = p.value.trim_end().len();
                    p.value.truncate(trimmed);
                    let cut = p.value.rfind([' ', '/']).map(|i| i + 1).unwrap_or(0);
                    p.value.truncate(cut);
                }
                (KeyCode::Char(ch), false) if !k.mods.alt => p.value.push(ch),
                _ => {}
            },
            InputEvent::Paste(data) => {
                let text = String::from_utf8_lossy(&data);
                p.value.push_str(text.lines().next().unwrap_or(""));
            }
            _ => {}
        }
        Mode::Prompt(p)
    }

    fn commit_prompt(&mut self, id: ClientId, p: Prompt) {
        let value = p.value.trim().to_string();
        match p.action {
            PromptAction::RenameTab { workspace, tab } => {
                if let Some(wi) = self.ws_index(workspace)
                    && let Some(t) = self.workspaces[wi].tabs.iter_mut().find(|t| t.id == tab)
                    && !value.is_empty()
                {
                    t.name = value;
                }
            }
            PromptAction::RenamePane(pane) => {
                if let Some(pn) = self.panes.get_mut(&pane) {
                    pn.name = (!value.is_empty()).then_some(value);
                }
            }
            PromptAction::RenameWorkspace(ws) => {
                if value.is_empty() {
                    return;
                }
                if self.workspaces.iter().any(|w| w.name == value && w.id != ws) {
                    self.toast(id, format!("a workspace named \"{value}\" is already open"));
                    return;
                }
                if let Some(wi) = self.ws_index(ws) {
                    self.workspaces[wi].name = value;
                }
            }
            PromptAction::OpenPreview => {
                if value.is_empty() {
                    return;
                }
                let base = self.client_focus(id).and_then(|p| self.panes.get(&p)).and_then(|p| p.current_cwd());
                let mut path = paths::expand_tilde(&value);
                if path.is_relative()
                    && let Some(b) = base
                {
                    path = b.join(path);
                }
                let Some(ws) = self.clients.get(&id).map(|c| c.workspace) else { return };
                let spec = PaneSpec { preview: Some(path), ..Default::default() };
                if let Err(e) = self.spawn_pane_in(ws, None, spec, Placement::Auto, None) {
                    self.toast(id, format!("{e:#}"));
                }
            }
            PromptAction::Upload { paths, .. } => {
                let mut args: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
                args.push(value);
                {
                    args.insert(0, "--".into());
                    args.insert(0, "cp".into());
                    self.start_job(id, "upload", args);
                }
            }
            PromptAction::UploadPick { server } => {
                let base = dirs::home_dir().unwrap_or_default();
                let mut args: Vec<String> = Vec::new();
                for word in value.split_whitespace() {
                    let p = paths::expand_tilde(word);
                    args.push(if p.is_relative() { base.join(p) } else { p }.display().to_string());
                }
                if args.is_empty() {
                    return;
                }
                args.push(format!("{server}:~/"));
                {
                    args.insert(0, "--".into());
                    args.insert(0, "cp".into());
                    self.start_job(id, "upload", args);
                }
            }
            PromptAction::Download => {
                if !value.contains(':') {
                    self.toast(id, "use server:path");
                    return;
                }
                let dir = paths::expand_tilde(&self.config.servers.download_dir);
                if let Err(e) = std::fs::create_dir_all(&dir) {
                    self.toast(id, format!("{}: {e}", dir.display()));
                    return;
                }
                self.start_job(id, "download", vec!["cp".into(), "--".into(), value, format!("{}/", dir.display())]);
            }
            PromptAction::NewWorktree { agent } => {
                if let Err(e) = self.agent_in_worktree(id, &agent, &value) {
                    self.toast(id, format!("{e:#}"));
                }
            }
            PromptAction::Commit { pane } => self.commit_worktree(id, pane, &value),
            PromptAction::Tunnel { server } => match self.add_tunnel(&server, &value, None) {
                Ok(t) => self.toast(id, format!("tunnel {}", t.spec.describe())),
                Err(e) => self.toast(id, format!("{e:#}")),
            },
            PromptAction::RunOn { targets } => {
                if value.is_empty() {
                    return;
                }
                let Some(ws) = self.clients.get(&id).map(|c| c.workspace) else { return };
                let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "kraal".into());
                let mut command = vec![exe, "exec".to_string()];
                command.extend(targets.iter().cloned());
                command.push("--".into());
                command.push(value.clone());
                let spec = PaneSpec { command, ..Default::default() };
                let label = format!("» {} on {}", value.chars().take(30).collect::<String>(), targets.join(" "));
                if let Err(e) = self.spawn_pane_in(ws, None, spec, Placement::Floating, Some(label)) {
                    self.toast(id, format!("{e:#}"));
                }
            }
            PromptAction::AddAccount { agent } => {
                if let Err(e) = self.add_account(id, &agent, &value) {
                    self.toast(id, format!("{e:#}"));
                }
            }
            PromptAction::SaveWorkspace(ws) => {
                if value.is_empty() || value.contains('/') {
                    self.toast(id, "invalid name");
                    return;
                }
                let Some(wi) = self.ws_index(ws) else { return };
                match self.save_definition(wi, &value) {
                    Ok(path) => self.toast(id, format!("saved {}", paths::shorten_home(&path))),
                    Err(e) => self.toast(id, format!("{e:#}")),
                }
            }
        }
        self.mark_persist();
        self.mark_dirty();
    }

    pub(super) fn save_definition(&mut self, wi: usize, name: &str) -> anyhow::Result<std::path::PathBuf> {
        let def = self.snapshot_definition(wi, name);
        let dir = self.definition_dir();
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{name}.kdl"));
        if path.exists() {
            let _ = std::fs::copy(&path, path.with_extension("kdl.bak"));
        }
        std::fs::write(&path, workspace::to_kdl(&def))?;
        let ws = &mut self.workspaces[wi];
        ws.definition = Some(path.clone());
        Ok(path)
    }

    fn on_help(&mut self, _id: ClientId, scroll: u16, ev: InputEvent) -> Mode {
        match ev {
            InputEvent::Key(k) => match k.code {
                KeyCode::Down | KeyCode::Char('j') => Mode::Help { scroll: scroll + 1 },
                KeyCode::Up | KeyCode::Char('k') => Mode::Help { scroll: scroll.saturating_sub(1) },
                KeyCode::PageDown | KeyCode::Char(' ') => Mode::Help { scroll: scroll + 10 },
                KeyCode::PageUp => Mode::Help { scroll: scroll.saturating_sub(10) },
                _ => Mode::Normal,
            },
            InputEvent::Mouse(MouseEvent { kind: MouseKind::ScrollDown, .. }) => Mode::Help { scroll: scroll + 2 },
            InputEvent::Mouse(MouseEvent { kind: MouseKind::ScrollUp, .. }) => {
                Mode::Help { scroll: scroll.saturating_sub(2) }
            }
            InputEvent::Mouse(MouseEvent { kind: MouseKind::Press(_), .. }) => Mode::Normal,
            _ => Mode::Help { scroll },
        }
    }

    fn on_confirm(&mut self, id: ClientId, message: String, action: ConfirmAction, ev: InputEvent) -> Mode {
        let InputEvent::Key(k) = ev else { return Mode::Confirm { message, action } };
        match k.code {
            KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                match action {
                    ConfirmAction::ClosePane(p) => self.close_pane(p),
                    ConfirmAction::CloseTab { workspace, tab } => {
                        if let Some(wi) = self.ws_index(workspace)
                            && let Some(t) = self.workspaces[wi].tabs.iter().find(|t| t.id == tab)
                        {
                            for p in t.panes() {
                                self.close_pane(p);
                            }
                        }
                    }
                    ConfirmAction::CloseWorkspace(ws) => self.close_workspace(ws),
                    ConfirmAction::Job { label, args } => self.start_job(id, &label, args),
                    ConfirmAction::GitJob { label, script } => {
                        self.git_job(id, &label, script, super::worktrees::JobEnd::CloseOnSuccess)
                    }
                    ConfirmAction::RemoveWorktree { pane } => self.remove_worktree(id, pane),
                    ConfirmAction::SwitchAccount { pane, agent, account } => {
                        let result = self
                            .accounts
                            .get(&agent, &account)
                            .context("no such account")
                            .and_then(|a| self.switch_account(pane, &a));
                        if let Err(e) = result {
                            self.toast(id, format!("{e:#}"));
                        }
                    }
                }
                let _ = id;
                Mode::Normal
            }
            _ => Mode::Normal,
        }
    }

    // ---- panes ----------------------------------------------------------------------------

    fn key_to_pane(&mut self, id: ClientId, k: &Key) -> Mode {
        let Some(p) = self.client_focus(id) else { return Mode::Normal };
        let Some(pane) = self.panes.get_mut(&p) else { return Mode::Normal };
        if matches!(pane.content, crate::pane::Content::Files(_)) {
            return self.files_key(id, p, k);
        }
        pane.clear_selection();
        if pane.exit_code.is_some() {
            match (k.code, k.mods.ctrl) {
                (KeyCode::Enter, _) | (KeyCode::Char('r'), false) => {
                    if pane.spec.account.is_some() {
                        // Restarting an agent resumes its conversation instead of starting over.
                        let session = pane.agent.session_id.clone();
                        let spec = self.resume_spec(&self.panes[&p].spec, session.as_deref());
                        self.panes.get_mut(&p).unwrap().spec = spec;
                    }
                    let pane = self.panes.get_mut(&p).unwrap();
                    if let Err(e) = pane.respawn(&self.env, &self.tx) {
                        self.toast(id, format!("{e:#}"));
                    }
                    self.relayout();
                }
                (KeyCode::Char('x'), false)
                | (KeyCode::Char('q'), false)
                | (KeyCode::Char('c'), true)
                | (KeyCode::Esc, _) => self.close_pane(p),
                _ => {}
            }
            return Mode::Normal;
        }
        if pane.is_preview() {
            let half = (pane.rows / 2).max(1) as i32;
            match k.code {
                KeyCode::Down | KeyCode::Char('j') => pane.scroll(-1),
                KeyCode::Up | KeyCode::Char('k') => pane.scroll(1),
                KeyCode::PageDown | KeyCode::Char(' ') => pane.scroll_page(false),
                KeyCode::PageUp => pane.scroll_page(true),
                KeyCode::Char('d') if k.mods.ctrl => pane.scroll(-half),
                KeyCode::Char('u') if k.mods.ctrl => pane.scroll(half),
                KeyCode::Char('g') | KeyCode::Home => pane.scroll_to_top(),
                KeyCode::Char('G') | KeyCode::End => pane.scroll_to_bottom(),
                KeyCode::Char('r') => {
                    if let crate::pane::Content::Preview(pv) = &mut pane.content {
                        pv.reload();
                    }
                }
                KeyCode::Char('q') | KeyCode::Esc => self.close_pane(p),
                _ => {}
            }
            return Mode::Normal;
        }
        let targets = self.input_targets(id, p);
        for t in targets {
            if let Some(pane) = self.panes.get_mut(&t)
                && pane.exit_code.is_none()
                && pane.is_terminal()
            {
                let bytes = encode_key(k, pane.input_modes());
                pane.write_input(bytes);
            }
        }
        Mode::Normal
    }

    /// Panes that receive typed input: the focused one, plus the sync targets.
    fn input_targets(&self, id: ClientId, focused: PaneId) -> Vec<PaneId> {
        let Some(c) = self.clients.get(&id) else { return vec![focused] };
        let Some(wi) = self.ws_index(c.workspace) else { return vec![focused] };
        self.workspaces[wi].sync_targets(focused)
    }

    /// Mark or unmark a pane for typing in sync.
    pub(super) fn toggle_mark(&mut self, id: ClientId, pane: PaneId) {
        let Some((wi, _)) = self.workspace_of(pane) else { return };
        let ws = &mut self.workspaces[wi];
        if !ws.marked.remove(&pane) {
            ws.marked.insert(pane);
        }
        let n = ws.marked.len();
        let msg = if n == 0 {
            ws.sync = super::SyncScope::Off;
            "no pane marked: sync off".to_string()
        } else {
            ws.sync = super::SyncScope::Marked;
            format!(
                "typing goes to {n} marked pane{} · ctrl+b m to unmark · ctrl+b y to stop",
                if n == 1 { "" } else { "s" }
            )
        };
        self.toast(id, msg);
        self.mark_dirty();
    }

    /// Paste into the focused pane, or offer an upload when files were dropped on an SSH pane.
    fn paste_or_upload(&mut self, id: ClientId, data: Vec<u8>) -> Mode {
        if let Some(p) = self.client_focus(id)
            && let Some(server) = self.pane_server(p)
            && let Some(paths) = crate::transfer::dropped_paths(&String::from_utf8_lossy(&data))
        {
            let n = paths.len();
            let what = if n == 1 {
                paths[0].file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default()
            } else {
                format!("{n} items")
            };
            return Mode::Prompt(Prompt::new(
                &format!("Upload {what} to (esc pastes the path instead)"),
                &format!("{}:~/", server.name),
                PromptAction::Upload { paths, raw: data, pane: p },
            ));
        }
        self.paste_to_pane(id, &data);
        Mode::Normal
    }

    fn paste_to_pane(&mut self, id: ClientId, data: &[u8]) {
        let Some(p) = self.client_focus(id) else { return };
        for t in self.input_targets(id, p) {
            if let Some(pane) = self.panes.get_mut(&t)
                && pane.exit_code.is_none()
                && pane.is_terminal()
            {
                let bytes = encode_paste(data, pane.input_modes());
                pane.write_input(bytes);
            }
        }
    }

    fn copy_to_client(&mut self, id: ClientId, text: &str) {
        if let Some(c) = self.clients.get(&id) {
            let _ = c.out.try_send(super::frame_bytes(&osc52(text)));
        }
        let n = text.chars().count();
        self.toast(id, format!("copied {n} character{}", if n == 1 { "" } else { "s" }));
    }

    /// Open an SSH pane in the client's workspace to a server name or `user@host`.
    fn connect(&mut self, id: ClientId, spec: &str, placement: Placement) {
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
    fn edit_servers(&mut self, id: ClientId) {
        let path = crate::servers::Inventory::path();
        if !path.exists()
            && let Err(e) = crate::servers::Inventory::empty().save()
        {
            self.toast(id, format!("{e:#}"));
            return;
        }
        let editor = std::env::var("VISUAL").or_else(|_| std::env::var("EDITOR")).unwrap_or_else(|_| "vi".into());
        let mut command: Vec<String> = editor.split_whitespace().map(String::from).collect();
        command.push(path.display().to_string());
        let Some(ws) = self.clients.get(&id).map(|c| c.workspace) else { return };
        let spec = PaneSpec { command, close_on_exit: true, ..Default::default() };
        if let Err(e) = self.spawn_pane_in(ws, None, spec, Placement::Floating, Some("servers.toml".into())) {
            self.toast(id, format!("{e:#}"));
        }
    }

    fn show_workspace(&mut self, id: ClientId, ws: u32) {
        if let Some(c) = self.clients.get_mut(&id) {
            c.workspace = ws;
        }
        self.mark_seen_focused(id);
        self.mark_dirty();
    }

    // ---- actions --------------------------------------------------------------------------

    fn current_ws(&self, id: ClientId) -> Option<(u32, usize)> {
        let c = self.clients.get(&id)?;
        Some((c.workspace, self.ws_index(c.workspace)?))
    }

    fn action(&mut self, id: ClientId, action: Action) -> Mode {
        let Some((ws, wi)) = self.current_ws(id) else { return Mode::Normal };
        let focus = self.workspaces[wi].tab().focus;
        match action {
            Action::NewPane | Action::SplitRight | Action::SplitDown | Action::NewFloating | Action::NewTab => {
                let placement = match action {
                    Action::SplitRight => Placement::Right,
                    Action::SplitDown => Placement::Down,
                    Action::NewFloating => Placement::Floating,
                    Action::NewTab => Placement::Tab,
                    _ => Placement::Auto,
                };
                if let Err(e) = self.spawn_pane_in(ws, focus, PaneSpec::default(), placement, None) {
                    self.toast(id, format!("{e:#}"));
                }
            }
            Action::ClosePane => {
                if let Some(p) = focus {
                    let busy = self.panes.get(&p).is_some_and(|pn| {
                        pn.exit_code.is_none() && (pn.has_foreground_job() || !pn.spec.command.is_empty())
                    });
                    if busy {
                        let name = self.panes[&p].display_name();
                        return Mode::Confirm {
                            message: format!("Close \"{name}\"? Its program is still running."),
                            action: ConfirmAction::ClosePane(p),
                        };
                    }
                    self.close_pane(p);
                }
            }
            Action::ToggleZoom => {
                let tab = self.workspaces[wi].tab_mut();
                tab.zoom = match (tab.zoom, focus) {
                    (Some(_), _) => None,
                    (None, Some(f))
                        if !tab.is_floating(f) && tab.root.as_ref().is_some_and(|r| r.leaves().len() > 1) =>
                    {
                        Some(f)
                    }
                    _ => None,
                };
                self.relayout();
            }
            Action::ToggleFloating => {
                let tab = self.workspaces[wi].tab_mut();
                if tab.floating.is_empty() {
                    if let Err(e) = self.spawn_pane_in(ws, focus, PaneSpec::default(), Placement::Floating, None) {
                        self.toast(id, format!("{e:#}"));
                    }
                } else {
                    tab.show_floating = !tab.show_floating;
                    if tab.show_floating {
                        let top = tab.floating.last().map(|f| f.pane);
                        if let Some(t) = top {
                            tab.set_focus(t);
                        }
                    } else if focus.is_some_and(|f| tab.is_floating(f)) {
                        let tiled = tab.recent.iter().copied().find(|p| !tab.is_floating(*p));
                        tab.focus = tiled.or_else(|| tab.root.as_ref().map(|r| r.leaves()[0]));
                    }
                }
            }
            Action::FocusLeft
            | Action::FocusRight
            | Action::FocusUp
            | Action::FocusDown
            | Action::FocusOrTabLeft
            | Action::FocusOrTabRight => {
                let dir = match action {
                    Action::FocusLeft | Action::FocusOrTabLeft => Dir::Left,
                    Action::FocusRight | Action::FocusOrTabRight => Dir::Right,
                    Action::FocusUp => Dir::Up,
                    _ => Dir::Down,
                };
                let moved = self.move_focus(wi, dir);
                if !moved {
                    match action {
                        Action::FocusOrTabLeft => self.switch_tab(wi, -1),
                        Action::FocusOrTabRight => self.switch_tab(wi, 1),
                        _ => {}
                    }
                }
                self.mark_seen_focused(id);
            }
            Action::ResizeLeft | Action::ResizeRight | Action::ResizeUp | Action::ResizeDown => {
                let dir = match action {
                    Action::ResizeLeft => Dir::Left,
                    Action::ResizeRight => Dir::Right,
                    Action::ResizeUp => Dir::Up,
                    _ => Dir::Down,
                };
                self.resize_focused(wi, dir);
            }
            Action::NextTab => self.switch_tab(wi, 1),
            Action::PrevTab => self.switch_tab(wi, -1),
            Action::Tab(n) => {
                let ws = &mut self.workspaces[wi];
                if (n as usize) <= ws.tabs.len() {
                    ws.active = n as usize - 1;
                }
                self.mark_seen_focused(id);
            }
            Action::CloseTab => {
                let tab = self.workspaces[wi].tab();
                let n = tab.panes().len();
                let action = ConfirmAction::CloseTab { workspace: ws, tab: tab.id };
                return Mode::Confirm {
                    message: format!("Close tab \"{}\" and its {n} pane{}?", tab.name, if n == 1 { "" } else { "s" }),
                    action,
                };
            }
            Action::RenameTab => {
                let tab = self.workspaces[wi].tab();
                return Mode::Prompt(Prompt::new(
                    "Rename tab",
                    &tab.name,
                    PromptAction::RenameTab { workspace: ws, tab: tab.id },
                ));
            }
            Action::RenamePane => {
                if let Some(p) = focus {
                    let name = self.panes.get(&p).and_then(|pn| pn.name.clone()).unwrap_or_default();
                    return Mode::Prompt(Prompt::new("Rename pane", &name, PromptAction::RenamePane(p)));
                }
            }
            Action::RenameWorkspace => {
                let name = self.workspaces[wi].name.clone();
                return Mode::Prompt(Prompt::new("Rename workspace", &name, PromptAction::RenameWorkspace(ws)));
            }
            Action::NextWorkspace | Action::PrevWorkspace => {
                let n = self.workspaces.len() as i32;
                let delta = if action == Action::NextWorkspace { 1 } else { -1 };
                let next = self.workspaces[(wi as i32 + delta).rem_euclid(n) as usize].id;
                self.show_workspace(id, next);
            }
            Action::WorkspacePicker => return Mode::Picker(self.workspace_picker(ws)),
            Action::SshPicker => return Mode::Picker(self.ssh_picker()),
            Action::PanePicker => return Mode::Picker(self.pane_picker()),
            Action::AgentPicker => return Mode::Picker(self.agent_picker(id, None)),
            Action::GitActions => match focus.and_then(|f| self.worktree_picker(f)) {
                Some(p) => return Mode::Picker(p),
                None => self.toast(id, "this pane is not in a kraal worktree (alt+g: start an agent in one)"),
            },
            Action::SwitchAccount => match focus.filter(|f| self.pane_account(*f).is_some()) {
                Some(f) => return Mode::Picker(self.agent_picker(id, Some(f))),
                None => self.toast(id, "this agent was not started from the agent manager (alt+g)"),
            },
            Action::NextAttention => {
                let queue = self.attention_queue();
                let next = queue.iter().copied().find(|p| Some(*p) != focus).or(queue.first().copied());
                match next {
                    Some(p) => self.reveal(id, p),
                    None => self.toast(id, "no agent needs you"),
                }
            }
            Action::ToggleSidebar => {
                if let Some(c) = self.clients.get_mut(&id) {
                    c.sidebar = !c.sidebar;
                    c.encoder.invalidate();
                }
                self.relayout();
            }
            Action::ToggleSync => {
                use super::SyncScope;
                let ws = &mut self.workspaces[wi];
                ws.sync = match (ws.sync, ws.marked.is_empty()) {
                    (SyncScope::Off, false) => SyncScope::Marked,
                    (SyncScope::Off, true) => SyncScope::Tab,
                    (SyncScope::Tab, _) => SyncScope::Workspace,
                    _ => SyncScope::Off,
                };
                if ws.sync == SyncScope::Off {
                    ws.marked.clear();
                }
                let msg = match ws.sync {
                    SyncScope::Off => "sync off".to_string(),
                    SyncScope::Tab => {
                        "sync: typing goes to every pane of this tab (ctrl+b y again: whole workspace)".into()
                    }
                    SyncScope::Workspace => "sync: typing goes to every pane of this workspace, all tabs".into(),
                    SyncScope::Marked => format!("sync: typing goes to the {} marked panes", ws.marked.len()),
                };
                self.toast(id, msg);
            }
            Action::MarkPane => {
                if let Some(p) = focus {
                    self.toggle_mark(id, p);
                }
            }
            Action::ScrollMode => {
                if let Some(p) = focus
                    && let Some(pane) = self.panes.get_mut(&p)
                {
                    if pane.is_terminal() {
                        pane.copy_mode(true);
                    } else {
                        pane.scroll_page(true);
                    }
                    return Mode::Scroll;
                }
            }
            Action::Files => {
                let (left, right) = self.files_defaults(id);
                if let Err(e) = self.open_files(ws, left, right) {
                    self.toast(id, format!("{e:#}"));
                }
            }
            Action::Search => {
                if focus.is_some_and(|f| self.panes.get(&f).is_some_and(|p| p.term().is_some())) {
                    return Mode::Search { query: String::new(), found: false };
                }
            }
            Action::OpenPreview => {
                return Mode::Prompt(Prompt::new("Preview file (path)", "", PromptAction::OpenPreview));
            }
            Action::SaveWorkspace => {
                let name = self.workspaces[wi].name.clone();
                return Mode::Prompt(Prompt::new("Save workspace layout as", &name, PromptAction::SaveWorkspace(ws)));
            }
            Action::Upload | Action::Download => {
                let Some(server) = focus.and_then(|f| self.pane_server(f)) else {
                    self.toast(id, "this pane is not connected to a server");
                    return Mode::Normal;
                };
                if action == Action::Upload {
                    return Mode::Prompt(Prompt::new(
                        &format!("Upload to {}:~/ (local paths, or drop files on the pane)", server.name),
                        "~/",
                        PromptAction::UploadPick { server: server.name },
                    ));
                }
                // Prefill with the selected text, if it looks like a path.
                let selected = focus.and_then(|f| self.panes.get(&f)).and_then(|p| p.selection_text());
                let path = selected.filter(|s| !s.contains('\n') && s.len() < 300).unwrap_or_default();
                let dir = self.config.servers.download_dir.clone();
                return Mode::Prompt(Prompt::new(
                    &format!("Download into {dir}"),
                    &format!("{}:{}", server.name, path.trim()),
                    PromptAction::Download,
                ));
            }
            Action::Detach => self.detach(id, "detached"),
            Action::Help => return Mode::Help { scroll: 0 },
            Action::SendPrefix => {
                if let (Some(p), Some(spec)) = (focus, self.keymap.prefix.first().cloned())
                    && let Some(pane) = self.panes.get_mut(&p)
                {
                    let bytes = encode_key_code(spec.code, spec.mods, pane.input_modes());
                    pane.write_input(bytes);
                }
            }
        }
        self.mark_persist();
        Mode::Normal
    }

    fn switch_tab(&mut self, wi: usize, delta: i32) {
        let ws = &mut self.workspaces[wi];
        let n = ws.tabs.len() as i32;
        if n > 0 {
            ws.active = (ws.active as i32 + delta).rem_euclid(n) as usize;
        }
        self.mark_dirty();
    }

    fn move_focus(&mut self, wi: usize, dir: Dir) -> bool {
        let area = self.layout_area;
        let tab: &mut Tab = self.workspaces[wi].tab_mut();
        let Some(f) = tab.focus else { return false };
        if tab.is_floating(f) && tab.show_floating {
            let ids: Vec<PaneId> = tab.floating.iter().map(|x| x.pane).collect();
            if ids.len() < 2 {
                return false;
            }
            let i = ids.iter().position(|p| *p == f).unwrap_or(0) as i32;
            let step = if matches!(dir, Dir::Left | Dir::Up) { -1 } else { 1 };
            let next = ids[(i + step).rem_euclid(ids.len() as i32) as usize];
            tab.set_focus(next);
            return true;
        }
        if tab.zoom.is_some() {
            return false;
        }
        let Some(root) = &tab.root else { return false };
        let rects = root.layout(area);
        match neighbour(&rects, f, dir, &tab.recent) {
            Some(n) => {
                tab.set_focus(n);
                self.mark_dirty();
                true
            }
            None => false,
        }
    }

    fn resize_focused(&mut self, wi: usize, dir: Dir) {
        let area = self.layout_area;
        let tab = self.workspaces[wi].tab_mut();
        let Some(f) = tab.focus else { return };
        if let Some(fl) = tab.floating.iter_mut().find(|x| x.pane == f) {
            let (dw, dh) = match dir {
                Dir::Left => (-0.03, 0.0),
                Dir::Right => (0.03, 0.0),
                Dir::Up => (0.0, -0.04),
                Dir::Down => (0.0, 0.04),
            };
            fl.rect.w = (fl.rect.w + dw).clamp(0.1, 1.0);
            fl.rect.h = (fl.rect.h + dh).clamp(0.1, 1.0);
            fl.rect.x = fl.rect.x.min(1.0 - fl.rect.w);
            fl.rect.y = fl.rect.y.min(1.0 - fl.rect.h);
        } else if let Some(root) = &mut tab.root {
            let cells = if matches!(dir, Dir::Left | Dir::Right) { 4 } else { 2 };
            if !root.resize(f, dir, cells, area) {
                // No border on that side: shrink from the opposite one instead.
                let opposite = match dir {
                    Dir::Left => Dir::Right,
                    Dir::Right => Dir::Left,
                    Dir::Up => Dir::Down,
                    Dir::Down => Dir::Up,
                };
                root.resize(f, opposite, -cells, area);
            }
        }
        self.relayout();
    }

    // ---- pickers --------------------------------------------------------------------------

    fn workspace_picker(&self, current: u32) -> Picker {
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
                    if self.workspaces.iter().any(|w| w.name == def.name) {
                        continue;
                    }
                    let n = def.panes().len();
                    items.push(PickItem {
                        label: def.name.clone(),
                        detail: format!(
                            "not running · {n} pane{} · {}",
                            plural(n),
                            path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default()
                        ),
                        action: PickAction::OpenDefinition(def.name),
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
        Picker::new(PickerKind::Workspace, "Workspaces  (enter open · ctrl+x close · type a new name to create)", items)
    }

    fn pane_picker(&self) -> Picker {
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
                        label: format!("{} › {} › {}", ws.name, tab.name, pane.display_name()),
                        detail,
                        action: PickAction::FocusPane(p),
                        state: pane.agent.kind.as_ref().map(|_| pane.agent.state),
                        dim: false,
                    });
                }
            }
        }
        Picker::new(PickerKind::Pane, "Panes", items)
    }

    // ---- mouse ----------------------------------------------------------------------------

    fn on_mouse(&mut self, id: ClientId, m: MouseEvent, scroll_mode: bool) {
        if !self.config.mouse {
            return;
        }
        let Some(c) = self.clients.get(&id) else { return };
        let (cols, rows, sidebar) = (c.cols, c.rows, c.sidebar);
        let area = self.layout_area;

        // A drag in progress owns the mouse until release.
        if let Some(drag) = self.clients.get_mut(&id).and_then(|c| c.drag.take()) {
            let keep = self.continue_drag(id, drag, m);
            if let Some(d) = keep
                && let Some(c) = self.clients.get_mut(&id)
            {
                c.drag = Some(d);
            }
            return;
        }

        let sw = if sidebar { self.sidebar_width(cols) } else { 0 };
        let press_left = matches!(m.kind, MouseKind::Press(MouseButton::Left));
        if m.row == 0 {
            if press_left {
                match self.tab_bar_hit(id, m.col) {
                    Some(BarItem::Tab(i)) => {
                        if let Some((_, wi)) = self.current_ws(id) {
                            self.workspaces[wi].active = i;
                            self.mark_seen_focused(id);
                        }
                    }
                    Some(BarItem::NewTab) => {
                        self.action(id, Action::NewTab);
                    }
                    Some(BarItem::Workspace) => {
                        let ws = self.clients[&id].workspace;
                        let p = self.workspace_picker(ws);
                        if let Some(c) = self.clients.get_mut(&id) {
                            c.mode = Mode::Picker(p);
                        }
                    }
                    None => {}
                }
                self.mark_dirty();
            }
            return;
        }
        if m.row + 1 >= rows {
            return;
        }
        if m.col < sw {
            if press_left {
                match self.sidebar_hit(id, m.row) {
                    Some(SideItem::Workspace(ws)) => self.show_workspace(id, ws),
                    Some(SideItem::Pane(p)) => self.reveal(id, p),
                    Some(SideItem::Definition(name)) => match self.open_workspace(&name) {
                        Ok(ws) => self.show_workspace(id, ws),
                        Err(e) => self.toast(id, format!("{e:#}")),
                    },
                    // A dev server or a tunnel: open it in the browser with ctrl, else copy its address.
                    Some(SideItem::Port(port)) | Some(SideItem::Tunnel(port)) => {
                        let addr = format!("localhost:{port}");
                        if m.mods.ctrl || matches!(self.sidebar_hit(id, m.row), Some(SideItem::Port(_))) {
                            self.send_msg(id, &crate::protocol::ServerMsg::Open { url: format!("http://{addr}") });
                            self.toast(id, format!("opening http://{addr}"));
                        } else {
                            self.copy_to_client(id, &addr);
                        }
                    }
                    None => {}
                }
            }
            return;
        }

        let Some((_, wi)) = self.current_ws(id) else { return };
        let rects = self.tab_rects(self.workspaces[wi].tab(), area);
        let hit = rects.iter().rev().find(|(_, r, _)| contains(*r, m.col, m.row)).copied();

        match m.kind {
            MouseKind::ScrollUp | MouseKind::ScrollDown => {
                let Some((p, r, _)) = hit else { return };
                let inner = self.inner(r);
                self.wheel(p, m, inner, scroll_mode);
            }
            MouseKind::Press(button) => {
                // Borders of tiled panes.
                if button == MouseButton::Left
                    && hit.is_none_or(|(_, _, floating)| !floating)
                    && let Some(b) = self.border_at(wi, m.col, m.row)
                {
                    if let Some(c) = self.clients.get_mut(&id) {
                        c.drag = Some(Drag::Border(b));
                    }
                    return;
                }
                let Some((p, r, floating)) = hit else { return };
                if button == MouseButton::Left && m.mods.ctrl && m.row == r.y && self.config.pane_frames {
                    self.toggle_mark(id, p);
                    return;
                }
                {
                    let tab = self.workspaces[wi].tab_mut();
                    if tab.focus != Some(p) {
                        tab.set_focus(p);
                        self.mark_seen_focused(id);
                        self.mark_dirty();
                    }
                }
                if floating && button == MouseButton::Left {
                    if m.row == r.y {
                        if let Some(c) = self.clients.get_mut(&id) {
                            c.drag = Some(Drag::MoveFloat { pane: p, grab_x: m.col - r.x, grab_y: 0 });
                        }
                        return;
                    }
                    if m.row == r.y + r.height - 1 && m.col == r.x + r.width - 1 {
                        if let Some(c) = self.clients.get_mut(&id) {
                            c.drag = Some(Drag::ResizeFloat { pane: p });
                        }
                        return;
                    }
                }
                let inner = self.inner(r);
                if !contains(inner, m.col, m.row) {
                    return;
                }
                let (col, row) = (m.col - inner.x, m.row - inner.y);
                // Ctrl+click opens the link under the pointer.
                if button == MouseButton::Left
                    && m.mods.ctrl
                    && let Some(url) = self.panes.get(&p).and_then(|pn| pn.url_at(col, row))
                {
                    // The client opens it, on its own machine (which matters for remote sessions).
                    self.send_msg(id, &crate::protocol::ServerMsg::Open { url: url.clone() });
                    self.toast(id, format!("opening {url}"));
                    return;
                }
                if button == MouseButton::Left
                    && matches!(self.panes.get(&p).map(|x| &x.content), Some(crate::pane::Content::Files(_)))
                {
                    let now = Instant::now();
                    let double = matches!(self.clients.get(&id).and_then(|c| c.last_click),
                        Some((t, x, y, _)) if now.duration_since(t) < Duration::from_millis(400) && x == m.col && y == m.row);
                    if let Some(c) = self.clients.get_mut(&id) {
                        c.last_click = Some((now, m.col, m.row, if double { 2 } else { 1 }));
                    }
                    let next = self.files_click(id, p, col, row, double);
                    if !matches!(next, Mode::Normal)
                        && let Some(c) = self.clients.get_mut(&id)
                    {
                        c.mode = next;
                    }
                    return;
                }
                let Some(pane) = self.panes.get_mut(&p) else { return };
                if pane.wants_mouse() && !m.mods.shift {
                    if let Some(bytes) = encode_mouse(&m, col, row, pane.input_modes()) {
                        pane.write_input(bytes);
                    }
                    if let Some(c) = self.clients.get_mut(&id) {
                        c.drag = Some(Drag::Forward { pane: p });
                    }
                    return;
                }
                if button != MouseButton::Left || !pane.is_terminal() {
                    return;
                }
                // Single, double, triple click → character, word, line selection.
                let now = Instant::now();
                let clicks = match self.clients.get(&id).and_then(|c| c.last_click) {
                    Some((t, x, y, n))
                        if now.duration_since(t) < Duration::from_millis(400) && x == m.col && y == m.row =>
                    {
                        n % 3 + 1
                    }
                    _ => 1,
                };
                if let Some(c) = self.clients.get_mut(&id) {
                    c.last_click = Some((now, m.col, m.row, clicks));
                }
                let kind = match clicks {
                    1 => SelectionType::Simple,
                    2 => SelectionType::Semantic,
                    _ => SelectionType::Lines,
                };
                let pane = self.panes.get_mut(&p).unwrap();
                if clicks == 1 {
                    pane.clear_selection();
                }
                pane.selection_start(col, row, kind);
                if let Some(c) = self.clients.get_mut(&id) {
                    c.drag = Some(Drag::Select { pane: p });
                }
                self.mark_dirty();
            }
            _ => {}
        }
    }

    fn wheel(&mut self, p: PaneId, m: MouseEvent, inner: ratatui::layout::Rect, scroll_mode: bool) {
        let Some(pane) = self.panes.get_mut(&p) else { return };
        let up = matches!(m.kind, MouseKind::ScrollUp);
        if let crate::pane::Content::Files(f) = &mut pane.content {
            f.scroll(m.col.saturating_sub(inner.x), if up { -3 } else { 3 });
            self.mark_dirty();
            return;
        }
        if !scroll_mode && pane.wants_mouse() && !m.mods.shift {
            let col = m.col.saturating_sub(inner.x).min(inner.width.saturating_sub(1));
            let row = m.row.saturating_sub(inner.y).min(inner.height.saturating_sub(1));
            if let Some(bytes) = encode_mouse(&m, col, row, pane.input_modes()) {
                pane.write_input(bytes);
            }
            return;
        }
        let modes = pane.input_modes();
        if !scroll_mode && modes.alt_screen && modes.alternate_scroll && pane.is_terminal() {
            if let Some(arrow) = encode_alternate_scroll(&m, modes) {
                pane.write_input(arrow.repeat(3));
            }
            return;
        }
        pane.scroll(if up { 3 } else { -3 });
        self.mark_dirty();
    }

    fn continue_drag(&mut self, id: ClientId, drag: Drag, m: MouseEvent) -> Option<Drag> {
        let released = matches!(m.kind, MouseKind::Release(_));
        let area = self.layout_area;
        let (_, wi) = self.current_ws(id)?;
        match drag {
            Drag::Border(b) => {
                let pos = match b.axis {
                    crate::layout::Axis::Row => m.col,
                    crate::layout::Axis::Column => m.row,
                };
                let Some(root) = &mut self.workspaces[wi].tab_mut().root else { return None };
                // The grabbed frame line sits one cell before the next child.
                let target = if pos >= b.pos { pos } else { pos + 1 };
                let moved = root.drag_border(&b, target);
                // Borders move as we drag: refresh the one we hold.
                let fresh = root.borders(area).into_iter().find(|nb| nb.path == b.path && nb.index == b.index);
                if moved {
                    self.relayout();
                }
                if released {
                    self.mark_persist();
                    return None;
                }
                fresh.map(Drag::Border)
            }
            Drag::Select { pane } => {
                let rects = self.tab_rects(self.workspaces[wi].tab(), area);
                let r = rects.iter().find(|(p, ..)| *p == pane).map(|(_, r, _)| self.inner(*r))?;
                let pn = self.panes.get_mut(&pane)?;
                let col = m.col.saturating_sub(r.x).min(r.width.saturating_sub(1));
                // Dragging past the top/bottom edge scrolls.
                if m.row < r.y {
                    pn.scroll(1);
                } else if m.row >= r.y + r.height {
                    pn.scroll(-1);
                }
                let row = m.row.saturating_sub(r.y).min(r.height.saturating_sub(1));
                pn.selection_update(col, row);
                self.mark_dirty();
                if released {
                    if let Some(text) = self.panes.get(&pane).and_then(|p| p.selection_text()) {
                        self.copy_to_client(id, &text);
                    } else if let Some(p) = self.panes.get_mut(&pane) {
                        p.clear_selection();
                    }
                    return None;
                }
                Some(Drag::Select { pane })
            }
            Drag::MoveFloat { pane, grab_x, grab_y } => {
                let tab = self.workspaces[wi].tab_mut();
                if let Some(f) = tab.floating.iter_mut().find(|f| f.pane == pane) {
                    let r = f.rect.to_rect(area);
                    let x = m.col.saturating_sub(grab_x).clamp(area.x, area.x + area.width - r.width);
                    let y = m.row.saturating_sub(grab_y).clamp(area.y, area.y + area.height - r.height);
                    f.rect = super::FloatRect::from_rect(ratatui::layout::Rect::new(x, y, r.width, r.height), area);
                    self.mark_dirty();
                }
                if released {
                    self.mark_persist();
                    None
                } else {
                    Some(Drag::MoveFloat { pane, grab_x, grab_y })
                }
            }
            Drag::ResizeFloat { pane } => {
                let tab = self.workspaces[wi].tab_mut();
                if let Some(f) = tab.floating.iter_mut().find(|f| f.pane == pane) {
                    let r = f.rect.to_rect(area);
                    let w = (m.col + 1).saturating_sub(r.x).clamp(10, area.x + area.width - r.x);
                    let h = (m.row + 1).saturating_sub(r.y).clamp(5, area.y + area.height - r.y);
                    f.rect = super::FloatRect::from_rect(ratatui::layout::Rect::new(r.x, r.y, w, h), area);
                    self.relayout();
                }
                if released {
                    self.mark_persist();
                    None
                } else {
                    Some(Drag::ResizeFloat { pane })
                }
            }
            Drag::Forward { pane } => {
                let rects = self.tab_rects(self.workspaces[wi].tab(), area);
                let r = rects.iter().find(|(p, ..)| *p == pane).map(|(_, r, _)| self.inner(*r))?;
                let pn = self.panes.get_mut(&pane)?;
                let col = m.col.saturating_sub(r.x).min(r.width.saturating_sub(1));
                let row = m.row.saturating_sub(r.y).min(r.height.saturating_sub(1));
                if let Some(bytes) = encode_mouse(&m, col, row, pn.input_modes()) {
                    pn.write_input(bytes);
                }
                if released { None } else { Some(Drag::Forward { pane }) }
            }
        }
    }

    /// A tiled border under the cursor (the frame lines on both sides of a split count).
    fn border_at(&self, wi: usize, col: u16, row: u16) -> Option<crate::layout::Border> {
        let tab = self.workspaces[wi].tab();
        if tab.zoom.is_some() {
            return None;
        }
        let root = tab.root.as_ref()?;
        let frames = self.config.pane_frames;
        root.borders(self.layout_area).into_iter().find(|b| {
            let (along, across) = match b.axis {
                crate::layout::Axis::Row => (col, row),
                crate::layout::Axis::Column => (row, col),
            };
            let on_line = if frames { along + 1 == b.pos || along == b.pos } else { along == b.pos };
            on_line && across >= b.span.0 && across < b.span.1
        })
    }
}

fn contains(r: ratatui::layout::Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}
