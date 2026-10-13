//! What the keymap's actions do.

use super::plural;
use crate::input::encode_key;
use crate::keymap::Action;
use crate::layout::{Dir, Node, PaneId, Preset, neighbour};
use crate::pane::PaneSpec;
use crate::protocol::Placement;
use crate::server::ui::{ConfirmAction, Mode, Prompt, PromptAction};
use crate::server::{ClientId, Server, Tab};

impl Server {
    /// Give the focused pane a prefix key: `key` as pressed, or the first prefix.
    pub(super) fn send_prefix(&mut self, id: ClientId, key: Option<crate::input::Key>) {
        let key = key.or_else(|| {
            let spec = self.keymap.prefix.first()?;
            Some(crate::input::Key { code: spec.code, mods: spec.mods, raw: Vec::new() })
        });
        if let (Some(p), Some(key)) = (self.client_focus(id), key)
            && let Some(pane) = self.panes.get_mut(&p)
        {
            pane.write_input(encode_key(&key, pane.input_modes()));
        }
    }

    pub(super) fn action(&mut self, id: ClientId, action: Action) -> Mode {
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
                        return Mode::confirm(
                            format!("Close \"{name}\"? Its program is still running."),
                            ConfirmAction::ClosePane(p),
                        );
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
                        tab.focus = tiled.or_else(|| tab.root.as_ref().and_then(Node::first_shown));
                        tab.expand_focus();
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
            Action::NextLayout => {
                let tab = self.workspaces[wi].tab_mut();
                match tab.root.as_ref().and_then(|root| Preset::next(tab.preset, root, tab.focus)) {
                    Some((preset, tree)) => {
                        tab.root = Some(tree);
                        tab.preset = Some(preset);
                        tab.zoom = None;
                        self.relayout();
                        self.toast(id, format!("Layout: {}", preset.describe()));
                    }
                    None => self.toast(id, "One pane fills the tab: there is no layout to choose."),
                }
            }
            Action::StackLeft | Action::StackRight | Action::StackUp | Action::StackDown => {
                let dir = match action {
                    Action::StackLeft => Dir::Left,
                    Action::StackRight => Dir::Right,
                    Action::StackUp => Dir::Up,
                    _ => Dir::Down,
                };
                if let Err(why) = self.stack_focused(wi, dir) {
                    self.toast(id, why);
                }
            }
            Action::Unstack => {
                let tab = self.workspaces[wi].tab_mut();
                let unstacked = match (focus, &mut tab.root) {
                    (Some(f), Some(root)) => root.unstack(f).then_some(f),
                    _ => None,
                };
                match unstacked {
                    Some(f) => {
                        tab.preset = None;
                        tab.zoom = None;
                        tab.set_focus(f);
                        self.relayout();
                    }
                    None => self.toast(id, "This pane is not in a stack."),
                }
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
                return Mode::confirm(
                    format!("Close tab \"{}\" and its {n} pane{}?", self.tab_label(tab), plural(n)),
                    action,
                );
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
            Action::Accounts => {
                self.refresh_existing_accounts();
                return Mode::Picker(self.accounts_picker(None));
            }
            Action::AddAccount => {
                self.refresh_existing_accounts();
                return Mode::AccountForm(self.account_form(id));
            }
            Action::AgentPicker => {
                self.refresh_existing_accounts();
                return Mode::Picker(self.agent_picker(id, None));
            }
            Action::GitActions => match focus.and_then(|f| self.worktree_picker(f)) {
                Some(p) => return Mode::Picker(p),
                None => {
                    let how = self.keymap.hint(Action::AgentPicker).unwrap_or_default();
                    self.toast(
                        id,
                        format!("This pane is not in a git repository ({how} starts an agent in a worktree)."),
                    )
                }
            },
            Action::ToolPicker => return Mode::Picker(self.tool_picker(focus)),
            Action::Hints => return self.start_hints(id),
            Action::Runbooks => return self.runbooks_action(id),
            Action::SwitchAccount => {
                if let Some(pane) = focus {
                    return self.change_account_mode(id, pane);
                }
            }
            Action::NextAttention => {
                let queue = self.attention_queue();
                let next = queue.iter().find(|a| !self.attention_shown(id, a)).or(queue.first()).cloned();
                match next {
                    Some(a) => self.go_to(id, a),
                    None => self.toast(id, "no agent needs you"),
                }
            }
            Action::ToggleSidebar => {
                let shown = self.clients.get(&id).is_some_and(|c| self.sidebar_shown(c));
                if let Some(c) = self.clients.get_mut(&id) {
                    c.sidebar = Some(!shown);
                    c.encoder.invalidate();
                }
                self.relayout();
            }
            Action::ToggleBottomBar => {
                self.bottom_bar = !self.bottom_bar;
                self.relayout();
            }
            Action::SelectForBroadcast => {
                if let Some(p) = focus {
                    self.toggle_mark(id, p);
                }
            }
            Action::BroadcastSelected
            | Action::BroadcastTab
            | Action::BroadcastWorkspace
            | Action::BroadcastOff
            | Action::BroadcastToggle => {
                use crate::server::SyncScope;
                let ws = &mut self.workspaces[wi];
                let scope = match action {
                    Action::BroadcastSelected => SyncScope::Marked,
                    Action::BroadcastTab => SyncScope::Tab,
                    Action::BroadcastWorkspace => SyncScope::Workspace,
                    Action::BroadcastToggle if ws.sync == SyncScope::Off => SyncScope::Tab,
                    _ => SyncScope::Off,
                };
                if scope == SyncScope::Marked && ws.marked.is_empty() {
                    let how = self.keymap.hint(Action::SelectForBroadcast).unwrap_or_default();
                    self.toast(id, format!("Mark the panes first ({how} in each)."));
                    return Mode::Normal;
                }
                ws.sync = scope;
                if scope == SyncScope::Off {
                    ws.marked.clear();
                    ws.sync_approved.clear();
                    self.toast(id, "Broadcast stopped: typing goes to the focused pane only.");
                } else if action == Action::BroadcastToggle && !std::mem::replace(&mut self.broadcast_tip_shown, true) {
                    // Once per server run: the key that starts it also stops it.
                    let again = self.keymap.hint(Action::BroadcastToggle).unwrap_or_default();
                    let msg = match self.keymap.menu_hint("broadcast") {
                        Some(more) => format!("{again} again stops it; {more} for more."),
                        None => format!("{again} again stops it."),
                    };
                    self.toast(id, msg);
                }
                self.mark_dirty();
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
            Action::CopyLastOutput => self.copy_last_output(id, focus),
            Action::Buffers => return Mode::Picker(self.buffer_picker()),
            Action::OpenPreview => {
                return Mode::Prompt(Prompt::new("Preview file (path)", "", PromptAction::OpenPreview));
            }
            Action::SaveWorkspace => {
                let name = self.definition_file_name(wi);
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
            Action::Messages => return Mode::Messages { scroll: 0 },
            Action::CommandPalette => return Mode::Picker(self.command_picker()),
            Action::ResizeMode => {
                let tiled = self.workspaces[wi].tab().root.as_ref().is_some_and(|r| r.places().len() > 1);
                if !tiled {
                    self.toast(id, "One pane fills the tab: there is nothing to resize.");
                    return Mode::Normal;
                }
                return Mode::Resize;
            }
            Action::NewWorkspace => return Mode::Prompt(Prompt::new("New workspace", "", PromptAction::NewWorkspace)),
            Action::CloseWorkspace => return self.ask_close_workspace(ws),
            Action::Restart => return self.ask_restart(),
            Action::Quit => return self.ask_quit(),
            Action::EditServers => self.edit_servers(id),
            Action::SendPrefix => self.send_prefix(id, None),
        }
        self.mark_persist();
        // What changed (the tab shown, the focus, floating panes) changed for every client
        // looking at this workspace.
        self.mark_dirty();
        Mode::Normal
    }

    pub(super) fn show_workspace(&mut self, id: ClientId, ws: u32) {
        if let Some(c) = self.clients.get_mut(&id) {
            c.workspace = ws;
        }
        self.mark_seen_focused(id);
        self.mark_dirty();
    }

    pub(super) fn current_ws(&self, id: ClientId) -> Option<(u32, usize)> {
        let c = self.clients.get(&id)?;
        Some((c.workspace, self.ws_index(c.workspace)?))
    }

    pub(super) fn switch_tab(&mut self, wi: usize, delta: i32) {
        let ws = &mut self.workspaces[wi];
        let n = ws.tabs.len() as i32;
        if n > 0 {
            ws.active = (ws.active as i32 + delta).rem_euclid(n) as usize;
        }
        self.mark_dirty();
    }

    pub(super) fn move_focus(&mut self, wi: usize, dir: Dir) -> bool {
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
        // Up and down go through the panes of a stack before leaving it.
        let in_stack = root.stack_of(f).filter(|_| matches!(dir, Dir::Up | Dir::Down)).and_then(|(panes, _)| {
            let i = panes.iter().position(|p| *p == f)?;
            let j = if dir == Dir::Up { i.checked_sub(1)? } else { i + 1 };
            panes.get(j).copied()
        });
        if let Some(n) = in_stack {
            tab.set_focus(n);
            self.mark_dirty();
            return true;
        }
        let rects = root.layout(area);
        match neighbour(&rects, root.holder(f), dir, &tab.recent) {
            Some(n) => {
                tab.set_focus(n);
                self.mark_dirty();
                true
            }
            None => false,
        }
    }

    /// Stack the focused pane with the pane on side `dir`: it moves into that pane's place, after
    /// the panes there when it comes from below or the right, before them otherwise, and keeps
    /// the focus.
    pub(super) fn stack_focused(&mut self, wi: usize, dir: Dir) -> Result<(), &'static str> {
        let area = self.layout_area;
        let tab = self.workspaces[wi].tab_mut();
        let Some(f) = tab.focus.filter(|f| !tab.is_floating(*f)) else { return Err("Floating panes do not stack.") };
        let Some(root) = &mut tab.root else { return Err("Floating panes do not stack.") };
        let onto = neighbour(&root.layout(area), root.holder(f), dir, &tab.recent).ok_or("No pane on that side.")?;
        if !root.stack(f, onto, matches!(dir, Dir::Left | Dir::Up)) {
            return Err("No pane on that side.");
        }
        tab.preset = None;
        tab.zoom = None;
        tab.set_focus(f);
        self.relayout();
        Ok(())
    }

    pub(super) fn resize_focused(&mut self, wi: usize, dir: Dir) {
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
            tab.preset = None;
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
}
