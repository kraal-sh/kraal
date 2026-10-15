//! The mouse: clicks, drags, the wheel, and what they hit.

use std::time::{Duration, Instant};

use alacritty_terminal::selection::SelectionType;

use crate::input::{MouseButton, MouseEvent, MouseKind, encode_alternate_scroll, encode_mouse};
use crate::keymap::Action;
use crate::layout::PaneId;
use crate::server::bottom_bar::Click;
use crate::server::render::{BarItem, SideItem};
use crate::server::ui::{Drag, Mode, Picker, WorkspaceDrag};
use crate::server::{ClientId, Server};
use crate::theme::NoticeKind;

impl Server {
    /// Actions for an account row. Overlays call this only for exposed sidebar cells.
    pub(in crate::server) fn sidebar_account_actions(&self, id: ClientId, m: MouseEvent) -> Option<Mode> {
        let c = self.clients.get(&id)?;
        let sw = self.sidebar_cols(c);
        if !self.config.mouse
            || m.col >= sw
            || !matches!(m.kind, MouseKind::Press(MouseButton::Left | MouseButton::Right))
        {
            return None;
        }
        let Some(item @ SideItem::Account(_)) = self.sidebar_hit(id, m.row) else { return None };
        let marker = matches!(self.sidebar_presentation(c), crate::server::geometry::Sidebar::Full(_))
            && m.col >= sw.saturating_sub(4)
            && self.sidebar_hit(id, m.row.saturating_sub(1)).as_ref() != Some(&item);
        if !marker && !matches!(m.kind, MouseKind::Press(MouseButton::Right)) {
            return None;
        }
        let SideItem::Account(account) = item else { unreachable!() };
        let mut picker = self.accounts_picker(Some(&account));
        picker.actions = (picker.action_keys().len() > 1).then_some(0);
        Some(Mode::Picker(picker))
    }

    /// A mouse event; returns the mode it opens (a picker, a question), if any.
    pub(super) fn on_mouse(&mut self, id: ClientId, m: MouseEvent, scroll_mode: bool) -> Option<Mode> {
        if !self.config.mouse {
            return None;
        }
        let c = self.clients.get(&id)?;
        let (rows, sw) = (c.rows, self.sidebar_cols(c));
        let full_sidebar = matches!(self.sidebar_presentation(c), crate::server::geometry::Sidebar::Full(_));
        let area = self.client_area(id);
        let hover = (full_sidebar && m.col > 0 && m.col < sw.saturating_sub(1))
            .then(|| self.sidebar_hit(id, m.row))
            .flatten()
            .filter(|item| matches!(item, SideItem::Account(_)));
        if self.clients[&id].sidebar_hover != hover {
            self.clients.get_mut(&id)?.sidebar_hover = hover;
            self.mark_dirty();
        }

        // A drag in progress owns the mouse until release.
        if let Some(drag) = self.clients.get_mut(&id).and_then(|c| c.drag.take()) {
            if let Drag::Workspace { mut drag, picker } = drag {
                let target = (m.col < sw).then(|| self.sidebar_workspace_key(id, m.row)).flatten();
                drag.moved |= (m.col, m.row) != drag.from;
                drag.target = target;
                if matches!(m.kind, MouseKind::Release(MouseButton::Left)) {
                    if !drag.moved {
                        let mut item = picker.current()?.clone();
                        if let crate::persist::WorkspaceKey::Source(path) = drag.workspace {
                            item.action = crate::server::ui::PickAction::OpenDefinition(path);
                        }
                        return Some(self.pick(id, &picker, item));
                    }
                    if !full_sidebar {
                        return None;
                    }
                    let mode = self.drop_workspace(id, *picker, drag);
                    return match mode {
                        Mode::Picker(p) if p.message.is_none() => None,
                        mode => Some(mode),
                    };
                }
                if matches!(m.kind, MouseKind::Drag(MouseButton::Left)) {
                    self.clients.get_mut(&id)?.drag = Some(Drag::Workspace { drag, picker });
                    self.mark_dirty();
                }
                return None;
            }
            // Copy mode came while a press was going to the program: the program gets the
            // release now, and copy mode the mouse from the next press on.
            // The release is that of the button the program saw pressed.
            if scroll_mode && let Drag::Forward { button, .. } = drag {
                self.continue_drag(id, drag, MouseEvent { kind: MouseKind::Release(button), ..m });
                return None;
            }
            let keep = self.continue_drag(id, drag, m);
            if let Some(d) = keep
                && let Some(c) = self.clients.get_mut(&id)
            {
                c.drag = Some(d);
            }
            return None;
        }

        let press_left = matches!(m.kind, MouseKind::Press(MouseButton::Left));
        // The bottom bar runs the whole width, under the sidebar.
        if self.bottom_bar && m.row + 1 == rows {
            // In copy mode and search, it shows no keys.
            if press_left && !scroll_mode {
                return match self.bottom_bar_hit(id, m.col)? {
                    Click::Run(action) => Some(self.action(id, action)),
                    Click::Menu => {
                        self.dismiss_welcome(id);
                        Some(Mode::leader())
                    }
                };
            }
            return None;
        }
        // The sidebar runs from the top, beside the top bar.
        if m.col < sw {
            if let Some(mode) = self.sidebar_account_actions(id, m) {
                return Some(mode);
            }
            if (matches!(m.kind, MouseKind::Press(MouseButton::Right))
                || press_left && full_sidebar && m.col >= sw.saturating_sub(4))
                && let Some(item @ SideItem::Workspace(ws)) = self.sidebar_hit(id, m.row)
                && (matches!(m.kind, MouseKind::Press(MouseButton::Right))
                    || self.sidebar_hit(id, m.row.saturating_sub(1)).as_ref() != Some(&item))
            {
                let mut picker = self.workspace_picker(self.clients[&id].workspace);
                picker.select(|i| i.action.workspace() == Some(ws));
                picker.actions = (picker.action_keys().len() > 1).then_some(0);
                return Some(Mode::Picker(picker));
            }
            if press_left {
                match self.sidebar_hit(id, m.row) {
                    Some(SideItem::Workspaces) => return Some(self.action(id, Action::WorkspacePicker)),
                    Some(SideItem::Panes) => return Some(self.action(id, Action::PanePicker)),
                    Some(SideItem::ToggleSidebar) => return Some(self.action(id, Action::ToggleSidebar)),
                    Some(SideItem::AddWorkspace) => return Some(self.action(id, Action::NewWorkspace)),
                    Some(SideItem::Accounts) => return Some(self.action(id, Action::Accounts)),
                    Some(SideItem::Account(account)) => return Some(self.account_details(&account)),
                    Some(SideItem::AddAccount) => return Some(self.action(id, Action::AddAccount)),
                    Some(SideItem::Workspace(ws)) => {
                        let mut picker = self.workspace_picker(self.clients[&id].workspace);
                        picker.select(|i| i.action.workspace() == Some(ws));
                        let drag = self.start_workspace_drag(&picker, (m.col, m.row))?;
                        self.clients.get_mut(&id)?.drag = Some(Drag::Workspace { drag, picker: Box::new(picker) });
                        return None;
                    }
                    Some(SideItem::ReviewAccount(account)) => {
                        if let Some(existing) = self.accounts.by_id(&account).filter(|a| a.review) {
                            return Some(Mode::Picker(self.review_account_picker(&existing)));
                        }
                    }
                    Some(SideItem::Pane(p)) => self.reveal(id, p),
                    Some(SideItem::Remote { server, pane }) => self.go_to_remote(id, &server, pane),
                    Some(SideItem::Definition(path)) => {
                        let mut picker = self.workspace_picker(self.clients[&id].workspace);
                        if !picker.items.iter().any(|i| i.action.definition() == Some(path.as_path())) {
                            picker.items.push(crate::server::ui::PickItem::new(
                                "",
                                "",
                                crate::server::ui::PickAction::OpenDefinition(path.clone()),
                            ));
                            picker.refilter();
                        }
                        picker.select(|i| i.action.definition() == Some(path.as_path()));
                        let drag = self.start_workspace_drag(&picker, (m.col, m.row))?;
                        self.clients.get_mut(&id)?.drag = Some(Drag::Workspace { drag, picker: Box::new(picker) });
                        return None;
                    }
                    Some(SideItem::Tunnel(port)) => {
                        if let Some(tunnel) = self.tunnels.iter().find(|t| t.spec.local == port) {
                            return Some(self.tunnel_details(tunnel.id, None));
                        }
                    }
                    Some(SideItem::Port(port)) => self.open_for(id, &format!("http://localhost:{port}")),
                    None => {}
                }
            }
            return None;
        }

        if m.row == 0 {
            // In copy mode and search, the bar shows their keys, not the tabs.
            if press_left && !scroll_mode {
                match self.tab_bar_hit(id, m.col) {
                    Some(BarItem::Tab(i)) => {
                        if let Some((_, wi)) = self.current_ws(id) {
                            self.workspaces[wi].active = i;
                            self.mark_seen_focused(id);
                        }
                    }
                    Some(BarItem::NewTab) => return Some(self.action(id, Action::NewTab)),
                    Some(BarItem::Toast(pane, action)) => return Some(self.toast_action(id, pane, action)),
                    Some(BarItem::Workspace) => {
                        let ws = self.clients[&id].workspace;
                        return Some(Mode::Picker(self.workspace_picker(ws)));
                    }
                    Some(BarItem::Text) | None => {}
                }
                self.mark_dirty();
            }
            return None;
        }
        if press_left && self.empty_workspace_button(id).is_some_and(|(r, _)| contains(r, m.col, m.row)) {
            return Some(self.empty_workspace_enter(id));
        }
        let (ws, wi) = self.current_ws(id)?;
        if self.workspaces[wi].kept.is_some() && !self.workspaces[wi].closed() {
            return Some(Mode::confirm(
                "The workspace has not finished closing. Retry Close to continue.",
                crate::server::ui::ConfirmAction::RetryCloseWorkspace(ws),
            ));
        }
        let rects = self.tab_rects(self.workspaces[wi].tab()?, area);
        let hit = rects.iter().rev().find(|(_, r, _)| contains(*r, m.col, m.row)).copied();
        // The row of a pane folded in a stack, unless a floating pane covers it.
        let folded = match hit {
            Some((.., true)) => None,
            _ => {
                self.stack_rows(self.workspaces[wi].tab()?, area).into_iter().find(|(_, r)| contains(*r, m.col, m.row))
            }
        };

        match m.kind {
            MouseKind::ScrollUp | MouseKind::ScrollDown => {
                let (p, r, _) = hit?;
                let inner = self.inner(r);
                self.wheel(id, p, m, inner, scroll_mode);
            }
            // Pointer motion and horizontal scrolling, for a program that asked for them.
            MouseKind::Move | MouseKind::ScrollLeft | MouseKind::ScrollRight if !scroll_mode => {
                let (p, r, _) = hit?;
                let inner = self.inner(r);
                if !contains(inner, m.col, m.row) {
                    return None;
                }
                let pane = self.panes.get_mut(&p)?;
                let modes = pane.input_modes();
                let wanted = match m.kind {
                    MouseKind::Move => modes.mouse_motion,
                    _ => pane.wants_mouse(),
                };
                if wanted
                    && !m.mods.shift
                    && let Some(bytes) = encode_mouse(&m, m.col - inner.x, m.row - inner.y, modes)
                {
                    pane.write_input(bytes);
                }
            }
            MouseKind::Press(button) => {
                if button == MouseButton::Left
                    && !m.mods.ctrl
                    && !scroll_mode
                    && let Some((pane, tile, floating)) = hit
                    && m.row == tile.y
                    && self.frame_account_hit(pane, tile, floating, self.workspaces[wi].marked.contains(&pane), m.col)
                {
                    return Some(self.change_account_mode(id, pane));
                }
                // Borders of tiled panes; ctrl+click on a frame's top marks the pane instead. A
                // folded pane's row is that pane's, but for its ends, on the borders beside it.
                if button == MouseButton::Left
                    && !m.mods.ctrl
                    && hit.is_none_or(|(_, _, floating)| !floating)
                    && let Some(b) = self.border_at(wi, area, m.col, m.row)
                    && (folded.is_none() || b.axis == crate::layout::Axis::Row)
                {
                    let along = match b.axis {
                        crate::layout::Axis::Row => m.col,
                        crate::layout::Axis::Column => m.row,
                    };
                    let grab = b.pos.saturating_sub(along);
                    if let Some(c) = self.clients.get_mut(&id) {
                        c.drag = Some(Drag::Border(b, grab));
                    }
                    return None;
                }
                if let Some((p, _)) = folded {
                    if button == MouseButton::Left && m.mods.ctrl {
                        self.toggle_mark(id, p);
                    } else {
                        // It opens, and the pane open in its stack folds.
                        self.workspaces[wi].tab_mut()?.set_focus(p);
                        self.mark_seen_focused(id);
                        self.mark_dirty();
                    }
                    return None;
                }
                let (p, r, floating) = hit?;
                let inner = self.inner(r);
                if button == MouseButton::Left && m.mods.ctrl && m.row == r.y && inner.y > r.y {
                    self.toggle_mark(id, p);
                    return None;
                }
                {
                    let tab = self.workspaces[wi].tab_mut()?;
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
                        return None;
                    }
                    if m.row == r.y + r.height - 1 && m.col == r.x + r.width - 1 {
                        if let Some(c) = self.clients.get_mut(&id) {
                            c.drag = Some(Drag::ResizeFloat { pane: p });
                        }
                        return None;
                    }
                }
                if !contains(inner, m.col, m.row) {
                    return None;
                }
                let (col, row) = (m.col - inner.x, m.row - inner.y);
                // Ctrl+click opens the link under the pointer.
                if button == MouseButton::Left
                    && m.mods.ctrl
                    && let Some(url) = self.panes.get(&p).and_then(|pn| pn.url_at(col, row))
                {
                    // The client opens it, on its own machine (which matters for remote sessions).
                    self.open_for(id, &url);
                    return None;
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
                    return Some(self.files_click(id, p, col, row, double));
                }
                let pane = self.panes.get_mut(&p)?;
                // In copy mode the mouse selects, whatever the program wants.
                if pane.wants_mouse() && !m.mods.shift && !scroll_mode {
                    if let Some(bytes) = encode_mouse(&m, col, row, pane.input_modes()) {
                        pane.write_input(bytes);
                    }
                    if let Some(c) = self.clients.get_mut(&id) {
                        c.drag = Some(Drag::Forward { pane: p, button });
                    }
                    return None;
                }
                if button != MouseButton::Left || !pane.is_terminal() {
                    return None;
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
                    // A double or triple click selects a word or a line at once.
                    c.drag = Some(Drag::Select { pane: p, from: (m.col, m.row), moved: clicks > 1 });
                }
                self.mark_dirty();
            }
            _ => {}
        }
        None
    }

    pub(super) fn wheel(
        &mut self,
        id: ClientId,
        p: PaneId,
        m: MouseEvent,
        inner: ratatui::layout::Rect,
        scroll_mode: bool,
    ) {
        let view = self.files_view(id, p);
        let Some(pane) = self.panes.get_mut(&p) else { return };
        let up = matches!(m.kind, MouseKind::ScrollUp);
        if let crate::pane::Content::Files(f) = &mut pane.content {
            // Counted in what this client shows, as a click is.
            f.set_viewer(id);
            if let Some((w, h)) = view {
                f.set_view(id, w, h);
            }
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

    pub(super) fn start_workspace_drag(&self, picker: &Picker, from: (u16, u16)) -> Option<WorkspaceDrag> {
        Some(WorkspaceDrag {
            workspace: self.workspace_key(&picker.current()?.action)?,
            order: picker.items.iter().filter_map(|i| self.workspace_key(&i.action)).collect(),
            from,
            list_start: 0,
            moved: false,
            target: None,
            hover: None,
        })
    }

    fn sidebar_workspace_key(&self, id: ClientId, row: u16) -> Option<crate::persist::WorkspaceKey> {
        use crate::server::ui::PickAction;
        match self.sidebar_hit(id, row)? {
            SideItem::Workspace(ws) => self.workspace_key(&PickAction::SwitchWorkspace(ws)),
            SideItem::Definition(path) => self.workspace_key(&PickAction::OpenDefinition(path)),
            _ => None,
        }
    }

    pub(super) fn drop_workspace(&mut self, id: ClientId, mut picker: Picker, drag: WorkspaceDrag) -> Mode {
        picker.select(|i| self.workspace_key(&i.action).as_ref() == Some(&drag.workspace));
        picker.message = None;
        if !picker.query.is_empty() {
            return picker.result(NoticeKind::Warning, "Clear search to reorder");
        }
        let Some(target) = drag.target else { return Mode::Picker(picker) };
        let before =
            drag.order.iter().position(|k| *k == target) < drag.order.iter().position(|k| *k == drag.workspace);
        let mode = self.reorder_workspace_checked(id, drag.workspace, target, before, Some(drag.order));
        self.return_to_workspaces(id, mode, picker.state())
    }

    pub(super) fn continue_drag(&mut self, id: ClientId, drag: Drag, m: MouseEvent) -> Option<Drag> {
        let released = matches!(m.kind, MouseKind::Release(_));
        let area = self.client_area(id);
        let (_, wi) = self.current_ws(id)?;
        match drag {
            Drag::Workspace { .. } => None,
            Drag::Border(b, grab) => {
                let pos = match b.axis {
                    crate::layout::Axis::Row => m.col,
                    crate::layout::Axis::Column => m.row,
                };
                let Some(root) = &mut self.workspaces[wi].tab_mut()?.root else { return None };
                // The border keeps its distance to the pointer: a click alone moves nothing, even
                // next to a pane narrower than a drag would leave it.
                let target = pos.saturating_add(grab);
                let moved = target != b.pos && root.drag_border(&b, target);
                // Borders move as we drag: refresh the one we hold.
                let fresh = root.borders(area).into_iter().find(|nb| nb.path == b.path && nb.index == b.index);
                if moved {
                    self.workspaces[wi].tab_mut()?.preset = None;
                    self.relayout();
                }
                if released {
                    self.mark_persist();
                    return None;
                }
                fresh.map(|b| Drag::Border(b, grab))
            }
            Drag::Select { pane, from, moved } => {
                // A click that does not move selects nothing: it only focuses the pane, and the
                // clipboard keeps what it had.
                let moved = moved || (m.col, m.row) != from;
                let rects = self.tab_rects(self.workspaces[wi].tab()?, area);
                let r = rects.iter().find(|(p, ..)| *p == pane).map(|(_, r, _)| self.inner(*r))?;
                let pn = self.panes.get_mut(&pane)?;
                if moved {
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
                }
                if released {
                    match self.panes.get(&pane).and_then(|p| p.selection_text()).filter(|_| moved) {
                        Some(text) => self.copy_to_client(id, &text),
                        None => {
                            if let Some(p) = self.panes.get_mut(&pane) {
                                p.clear_selection();
                            }
                            self.mark_dirty();
                        }
                    }
                    return None;
                }
                Some(Drag::Select { pane, from, moved })
            }
            Drag::MoveFloat { pane, grab_x, grab_y } => {
                let tab = self.workspaces[wi].tab_mut()?;
                if let Some(f) = tab.floating.iter_mut().find(|f| f.pane == pane) {
                    let r = f.rect.to_rect(area);
                    let x = m.col.saturating_sub(grab_x).clamp(area.x, area.x + area.width - r.width);
                    let y = m.row.saturating_sub(grab_y).clamp(area.y, area.y + area.height - r.height);
                    f.rect =
                        crate::server::FloatRect::from_rect(ratatui::layout::Rect::new(x, y, r.width, r.height), area);
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
                let tab = self.workspaces[wi].tab_mut()?;
                if let Some(f) = tab.floating.iter_mut().find(|f| f.pane == pane) {
                    let r = f.rect.to_rect(area);
                    // At least 10×5, unless the area is smaller than that from here.
                    let (max_w, max_h) =
                        ((area.x + area.width).saturating_sub(r.x), (area.y + area.height).saturating_sub(r.y));
                    let w = (m.col + 1).saturating_sub(r.x).max(10).min(max_w);
                    let h = (m.row + 1).saturating_sub(r.y).max(5).min(max_h);
                    f.rect = crate::server::FloatRect::from_rect(ratatui::layout::Rect::new(r.x, r.y, w, h), area);
                    self.relayout();
                }
                if released {
                    self.mark_persist();
                    None
                } else {
                    Some(Drag::ResizeFloat { pane })
                }
            }
            Drag::Forward { pane, button } => {
                let rects = self.tab_rects(self.workspaces[wi].tab()?, area);
                let r = rects.iter().find(|(p, ..)| *p == pane).map(|(_, r, _)| self.inner(*r))?;
                let pn = self.panes.get_mut(&pane)?;
                let col = m.col.saturating_sub(r.x).min(r.width.saturating_sub(1));
                let row = m.row.saturating_sub(r.y).min(r.height.saturating_sub(1));
                if let Some(bytes) = encode_mouse(&m, col, row, pn.input_modes()) {
                    pn.write_input(bytes);
                }
                if released { None } else { Some(Drag::Forward { pane, button }) }
            }
        }
    }

    /// A tiled border under the cursor, the layout being in `area` (see
    /// [`crate::layout::Border::grabbed_at`]).
    pub(super) fn border_at(
        &self,
        wi: usize,
        area: ratatui::layout::Rect,
        col: u16,
        row: u16,
    ) -> Option<crate::layout::Border> {
        let tab = self.workspaces[wi].tab()?;
        if tab.zoom.is_some() {
            return None;
        }
        let root = tab.root.as_ref()?;
        let frames = self.config.pane_frames;
        root.borders(area).into_iter().find(|b| b.grabbed_at(col, row, frames))
    }
}

fn contains(r: ratatui::layout::Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account_status::Status;
    use crate::server::agents::ExistingCheck;
    use crate::server::ui::PickerKind;

    #[test]
    fn strip_clicks_keep_the_strip_and_never_use_the_full_sidebar_actions_area() {
        let event = |kind, col, row| MouseEvent { kind, col, row, mods: Default::default() };
        for (cols, rows) in [(124, 36), (80, 24), (60, 20)] {
            let (tx, _rx) = crate::server::event_channel(32);
            let mut server = crate::server::tests::server(tx);
            let (client, _peer) = crate::server::tests::client(&mut server);
            let current = server.clients[&client].workspace;
            let other = server.new_workspace("other".into(), None);
            let c = server.clients.get_mut(&client).unwrap();
            (c.cols, c.rows, c.sidebar) = (cols, rows, Some(false));
            server.bottom_bar = true;
            let order = server.workspace_order.clone();
            let row = (0..rows).find(|&r| server.sidebar_hit(client, r) == Some(SideItem::Workspace(other))).unwrap();
            for col in 0..4 {
                server.clients.get_mut(&client).unwrap().workspace = current;
                let press = event(MouseKind::Press(MouseButton::Left), col, row);
                assert!(server.on_mouse(client, press, false).is_none());
                assert_eq!(server.clients[&client].workspace, current);
                assert!(matches!(
                    server.on_mouse(client, event(MouseKind::Release(MouseButton::Left), col, row), false),
                    Some(Mode::Normal)
                ));
                assert_eq!(server.clients[&client].workspace, other);
                assert_eq!(server.clients[&client].sidebar, Some(false));
                let Mode::Picker(p) =
                    server.on_mouse(client, event(MouseKind::Press(MouseButton::Right), col, row), false).unwrap()
                else {
                    panic!()
                };
                assert!(p.actions.is_some());
                assert_eq!(p.current().unwrap().action.workspace(), Some(other));
            }
            server.on_mouse(client, event(MouseKind::Press(MouseButton::Left), 1, row), false);
            server.on_mouse(client, event(MouseKind::Drag(MouseButton::Left), 1, 1), false);
            server.on_mouse(client, event(MouseKind::Release(MouseButton::Left), 1, 1), false);
            assert_eq!(server.workspace_order, order, "the strip does not reorder workspaces");
            for (item, kind) in [(SideItem::Workspaces, PickerKind::Workspace), (SideItem::Panes, PickerKind::Pane)] {
                let row = (0..rows).find(|&r| server.sidebar_hit(client, r) == Some(item.clone())).unwrap();
                let Mode::Picker(p) =
                    server.on_mouse(client, event(MouseKind::Press(MouseButton::Left), 1, row), false).unwrap()
                else {
                    panic!()
                };
                assert!(p.kind == kind);
            }
            let mode = server.on_mouse(client, event(MouseKind::Press(MouseButton::Left), 1, rows - 2), false).unwrap();
            if cols >= 84 {
                assert!(matches!(mode, Mode::Normal));
                assert_eq!(server.clients[&client].sidebar, Some(true));
            } else {
                assert!(matches!(mode, Mode::Picker(p) if p.kind == PickerKind::Workspace));
                assert_eq!(server.clients[&client].sidebar, Some(false));
            }
        }
    }

    #[test]
    fn strip_overflow_rows_open_the_complete_lists() {
        let _batch = crate::pane::batch();
        let (tx, _rx) = crate::server::event_channel(32);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        let ws = server.clients[&client].workspace;
        let c = server.clients.get_mut(&client).unwrap();
        (c.cols, c.rows) = (60, 8);
        for i in 0..8 {
            server.new_workspace(format!("extra-{i}"), None);
            let pane = server
                .spawn_pane_in(ws, None, crate::pane::PaneSpec::default(), crate::protocol::Placement::Tab, None)
                .unwrap();
            let agent = &mut server.panes.get_mut(&pane).unwrap().agent;
            agent.kind = Some("claude".into());
            agent.state = if i == 7 { crate::agent::AgentState::Blocked } else { crate::agent::AgentState::Working };
        }
        for (item, kind, count) in
            [(SideItem::Workspaces, PickerKind::Workspace, 9), (SideItem::Panes, PickerKind::Pane, 8)]
        {
            let rows: Vec<_> = (0..8).filter(|&row| server.sidebar_hit(client, row) == Some(item.clone())).collect();
            assert_eq!(rows.len(), 2, "the header and overflow both open the list");
            let click = MouseEvent {
                kind: MouseKind::Press(MouseButton::Left),
                col: 1,
                row: rows[1],
                mods: Default::default(),
            };
            let Mode::Picker(p) = server.on_mouse(client, click, false).unwrap() else { panic!() };
            assert!(p.kind == kind);
            assert!(p.items.len() >= count);
        }
        assert_eq!(server.clients[&client].sidebar, None);
    }

    #[test]
    fn a_stale_sidebar_file_click_keeps_its_target_and_offers_retry() {
        let (tx, _rx) = crate::server::queue::channel(32);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        let current = server.clients[&client].workspace;
        let path = std::path::PathBuf::from("/nonexistent/kraal-w6-file.kdl");
        server.def_files.push((path.clone(), "missing".into()));
        let row = (0..36).find(|&r| server.sidebar_hit(client, r) == Some(SideItem::Definition(path.clone()))).unwrap();
        let click = MouseEvent { kind: MouseKind::Press(MouseButton::Left), col: 3, row, mods: Default::default() };
        assert!(server.on_mouse(client, click, false).is_none());
        let release = MouseEvent { kind: MouseKind::Release(MouseButton::Left), ..click };
        let Mode::Confirm(q) = server.on_mouse(client, release, false).unwrap() else { panic!() };
        assert!(
            matches!(q.action, crate::server::ui::ConfirmAction::OpenWorkspace(crate::server::ui::PickAction::OpenDefinition(p)) if p == path)
        );
        assert_eq!(server.clients[&client].workspace, current);
        assert_eq!(server.workspaces.len(), 1);
    }

    #[test]
    fn workspace_entries_and_actions_work_in_all_sidebar_presentations() {
        for (cols, rows) in [(39, 20), (60, 20), (80, 24), (124, 36)] {
            let (tx, _rx) = crate::server::queue::channel(32);
            let mut server = crate::server::tests::server(tx);
            let (client, _peer) = crate::server::tests::client(&mut server);
            let c = server.clients.get_mut(&client).unwrap();
            (c.cols, c.rows) = (cols, rows);
            let current = server.clients[&client].workspace;
            let other = server.new_workspace("other".into(), None);
            let click =
                |button, col, row| MouseEvent { kind: MouseKind::Press(button), col, row, mods: Default::default() };
            let sidebar = server.sidebar_cols(&server.clients[&client]);
            assert_eq!(sidebar == 0, cols < 40);
            let col = if sidebar > 0 {
                2
            } else {
                (0..cols).find(|&col| matches!(server.tab_bar_hit(client, col), Some(BarItem::Workspace))).unwrap()
            };
            let Mode::Picker(p) = server.on_mouse(client, click(MouseButton::Left, col, 0), false).unwrap() else {
                panic!()
            };
            assert!(p.kind == PickerKind::Workspace);
            for line in [0, 1] {
                let p = server.workspace_picker(current);
                let layout = crate::server::overlays::picker_layout(&p, server.client_screen(client));
                let event = crate::input::InputEvent::Mouse(click(
                    MouseButton::Left,
                    layout.list.x + 2,
                    layout.list.y + 2 + line,
                ));
                let crate::input::InputEvent::Mouse(press) = event else { panic!() };
                let Mode::Picker(p) = server.on_picker(client, p, event) else { panic!() };
                assert_eq!(server.clients[&client].workspace, current);
                let release = MouseEvent { kind: MouseKind::Release(MouseButton::Left), ..press };
                assert!(matches!(server.on_picker(client, p, crate::input::InputEvent::Mouse(release)), Mode::Normal));
                assert_eq!(server.clients[&client].workspace, other, "one click on an unselected row");
                server.clients.get_mut(&client).unwrap().workspace = current;
            }
            if sidebar > 0 {
                let row =
                    (0..rows).find(|&r| server.sidebar_hit(client, r) == Some(SideItem::Workspace(other))).unwrap();
                let mut clicks = vec![(MouseButton::Right, 2)];
                if sidebar > 4 {
                    clicks.push((MouseButton::Left, sidebar - 3));
                }
                for (button, col) in clicks {
                    let Mode::Picker(p) = server.on_mouse(client, click(button, col, row), false).unwrap() else {
                        panic!()
                    };
                    assert!(p.actions.is_some());
                    assert_eq!(p.current().unwrap().action.workspace(), Some(other));
                    assert_eq!(server.clients[&client].workspace, current);
                }
                if sidebar == 4 {
                    continue;
                }
                for i in 0..50 {
                    server.new_workspace(format!("extra-{i}"), None);
                }
                let row = (0..rows).find(|&r| server.sidebar_hit(client, r) == Some(SideItem::AddWorkspace)).unwrap();
                let Mode::Prompt(p) = server.on_mouse(client, click(MouseButton::Left, 3, row), false).unwrap() else {
                    panic!()
                };
                assert_eq!(p.title, "Add workspace");
            }
        }
    }

    #[test]
    fn account_sidebar_clicks_open_details_or_the_accounts_own_actions() {
        for width in [28, 12] {
            let (tx, _rx) = crate::server::queue::channel(32);
            let mut server = crate::server::tests::server(tx);
            let (client, _peer) = crate::server::tests::client(&mut server);
            server.config.sidebar.width = width;
            let mut a = server.accounts.get("claude", "default").unwrap();
            a.name = "work".into();
            let a = server.accounts.add(a).unwrap();
            let row =
                (0..36).find(|&row| server.sidebar_hit(client, row) == Some(SideItem::Account(a.id.clone()))).unwrap();
            for offset in [0, 1] {
                for col in 1..width - 1 {
                    let click = MouseEvent {
                        kind: MouseKind::Press(MouseButton::Left),
                        col,
                        row: row + offset,
                        mods: Default::default(),
                    };
                    let mode = server.on_mouse(client, click, false).unwrap();
                    if offset == 0 && col >= width - 4 {
                        let Mode::Picker(p) = mode else { panic!() };
                        assert!(p.kind == PickerKind::Accounts);
                        assert!(p.actions.is_some());
                        assert!(matches!(&p.current().unwrap().action,
                            crate::server::ui::PickAction::AccountDetails(id) if id == &a.id));
                    } else {
                        assert!(matches!(mode, Mode::AccountDetails(d) if d.account == a.id));
                    }
                }
                let click = MouseEvent {
                    kind: MouseKind::Press(MouseButton::Right),
                    col: 3,
                    row: row + offset,
                    mods: Default::default(),
                };
                assert!(matches!(server.on_mouse(client, click, false), Some(Mode::Picker(p)) if p.actions.is_some()));
            }
        }
    }

    #[test]
    fn full_sidebar_requests_pointer_motion_and_releases_it_when_hidden() {
        let (tx, _rx) = crate::server::queue::channel(32);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        assert!(!server.wants_pointer_motion(&server.clients[&client]));
        let mut account = server.accounts.get("claude", "default").unwrap();
        account.name = "work".into();
        server.accounts.add(account).unwrap();
        assert!(server.wants_pointer_motion(&server.clients[&client]));
        server.clients.get_mut(&client).unwrap().sidebar = Some(false);
        assert!(!server.wants_pointer_motion(&server.clients[&client]));
        server.clients.get_mut(&client).unwrap().sidebar = Some(true);
        assert!(server.wants_pointer_motion(&server.clients[&client]));
        server.clients.get_mut(&client).unwrap().cols = 80;
        assert!(!server.wants_pointer_motion(&server.clients[&client]));
        server.clients.get_mut(&client).unwrap().cols = 124;
        server.config.mouse = false;
        assert!(!server.wants_pointer_motion(&server.clients[&client]));
    }

    #[test]
    fn repeated_follow_focus_does_not_check_account_homes() {
        let (tx, _rx) = crate::server::queue::channel(32);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        for name in ["work", "personal"] {
            let mut account = server.accounts.get("claude", "default").unwrap();
            account.id = name.into();
            account.name = name.into();
            account.home = format!("/nonexistent/kraal-focus-{name}");
            server.accounts.add(account).unwrap();
        }
        crate::accounts::HOME_CHECKS.with(|n| n.set(0));
        for _ in 0..100 {
            server.follow_focus();
        }
        assert!(server.clients[&client].pointer_motion);
        assert_eq!(crate::accounts::HOME_CHECKS.with(std::cell::Cell::get), 0);
    }

    #[test]
    fn oversized_mouse_coordinates_do_not_hover_an_account() {
        let (tx, _rx) = crate::server::queue::channel(32);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        let mut account = server.accounts.get("claude", "default").unwrap();
        account.name = "work".into();
        let account = server.accounts.add(account).unwrap();
        let row = (0..36)
            .find(|&row| server.sidebar_hit(client, row) == Some(SideItem::Account(account.id.clone())))
            .unwrap();
        server.client_input(client, format!("\x1b[<35;2;{}M", row + 1).as_bytes());
        assert_eq!(server.clients[&client].sidebar_hover, Some(SideItem::Account(account.id)));
        server.client_input(client, format!("\x1b[<35;99999;{}M", row + 1).as_bytes());
        assert_eq!(server.clients[&client].sidebar_hover, None);
        server.client_input(client, b"\x1b[<35;99999;1M");
        assert_eq!(server.clients[&client].sidebar_hover, None);
        assert!(matches!(server.clients[&client].mode, Mode::Normal));
    }

    #[test]
    fn exposed_account_markers_open_actions_from_details_and_the_picker() {
        let (tx, _rx) = crate::server::queue::channel(32);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        let mut account = server.accounts.get("claude", "default").unwrap();
        account.name = "work".into();
        let account = server.accounts.add(account).unwrap();
        server.bottom_bar = true;
        let row = (0..36)
            .find(|&row| server.sidebar_hit(client, row) == Some(SideItem::Account(account.id.clone())))
            .unwrap();
        let marker = server.sidebar_cols(&server.clients[&client]) - 3;
        for details in [true, false] {
            server.clients.get_mut(&client).unwrap().mode = Mode::Normal;
            if details {
                server.client_input(client, format!("\x1b[<0;2;{}M", row + 1).as_bytes());
                assert!(matches!(&server.clients[&client].mode, Mode::AccountDetails(d) if d.account == account.id));
            } else {
                server.client_input(client, b"\x02Al");
                assert!(
                    matches!(&server.clients[&client].mode, Mode::Picker(p) if p.kind == PickerKind::Accounts && p.actions.is_none())
                );
            }
            let row = (0..36)
                .find(|&row| server.sidebar_hit(client, row) == Some(SideItem::Account(account.id.clone())))
                .unwrap();
            let mut buf = ratatui::buffer::Buffer::empty(server.client_screen(client));
            server.compose(client, crate::accounts::now(), &mut buf);
            assert_eq!(buf[(marker, row)].symbol(), "⋮");
            server.client_input(client, format!("\x1b[<0;{};{}M", marker + 1, row + 1).as_bytes());
            let Mode::Picker(p) = &server.clients[&client].mode else { panic!() };
            assert!(p.kind == PickerKind::Accounts);
            assert_eq!(p.actions, Some(0));
            assert!(matches!(&p.current().unwrap().action,
                crate::server::ui::PickAction::AccountDetails(id) if id == &account.id));
        }
    }

    #[test]
    fn passive_hover_keeps_the_primary_client_activity_and_pane_sizes() {
        let (tx, _rx) = crate::server::queue::channel(32);
        let mut server = crate::server::tests::server(tx);
        let (primary, _peer) = crate::server::tests::client(&mut server);
        let (other, _other_peer) = crate::server::tests::client(&mut server);
        let mut account = server.accounts.get("claude", "default").unwrap();
        account.name = "work".into();
        let account = server.accounts.add(account).unwrap();
        let c = server.clients.get_mut(&other).unwrap();
        (c.cols, c.rows) = (150, 45);
        c.workspace = server.workspaces[0].id;
        let pane = server
            .spawn_pane(None, crate::pane::PaneSpec { command: vec!["true".into()], ..Default::default() })
            .unwrap();
        let tab = &mut server.workspaces[0].tabs[0];
        tab.root = Some(crate::layout::Node::Leaf(pane));
        tab.focus = Some(pane);
        server.primary = Some(primary);
        server.relayout();
        server.follow_focus();
        assert!(server.clients[&other].pointer_motion);
        let size = (server.panes[&pane].cols, server.panes[&pane].rows);
        let area = server.layout_area;
        let last_input = server.clients[&other].last_input;
        let row =
            (0..45).find(|&row| server.sidebar_hit(other, row) == Some(SideItem::Account(account.id.clone()))).unwrap();
        let motion = format!("\x1b[<35;2;{}M", row + 1);
        // Split reports must stay passive before and after the decoder completes them.
        for byte in motion.bytes() {
            server.client_input(other, &[byte]);
            assert_eq!(server.primary, Some(primary));
            assert_eq!(server.clients[&other].last_input, last_input);
            assert_eq!(server.layout_area, area);
            assert_eq!((server.panes[&pane].cols, server.panes[&pane].rows), size);
        }
        assert_eq!(server.clients[&other].sidebar_hover, Some(SideItem::Account(account.id)));
        server.client_input(other, b"\x1b[<35;40;5M");
        assert_eq!(server.clients[&other].sidebar_hover, None);
        assert_eq!(server.primary, Some(primary));
        assert_eq!(server.clients[&other].last_input, last_input);
        assert_eq!((server.panes[&pane].cols, server.panes[&pane].rows), size);
        server.client_input(other, b"\x1b[<0;40;5M");
        assert_eq!(server.primary, Some(other));
        assert!(server.clients[&other].last_input > last_input);
        assert_ne!((server.panes[&pane].cols, server.panes[&pane].rows), size);
    }

    #[test]
    fn sidebar_account_entry_points_retry_cached_unsigned_homes() {
        for add in [false, true] {
            let (tx, _rx) = crate::server::queue::channel(16);
            let mut server = crate::server::tests::server(tx);
            let (client, _peer) = crate::server::tests::client(&mut server);
            server.existing_checks.insert("claude".into(), ExistingCheck::Checked(Status::NotSignedIn));
            server.existing_checks.insert("codex".into(), ExistingCheck::Checking);
            let row = (0..36)
                .find(|&row| {
                    matches!(
                        (add, server.sidebar_hit(client, row)),
                        (false, Some(SideItem::Accounts)) | (true, Some(SideItem::AddAccount))
                    )
                })
                .unwrap();
            let click = MouseEvent { kind: MouseKind::Press(MouseButton::Left), col: 2, row, mods: Default::default() };
            let mode = server.on_mouse(client, click, false).unwrap();
            if add {
                assert!(matches!(mode, Mode::AccountForm(_)));
            } else {
                assert!(matches!(mode, Mode::Picker(p) if p.kind == PickerKind::Accounts));
            }
            assert!(matches!(server.existing_checks["claude"], ExistingCheck::Checking));
            assert_eq!(server.jobs.len(), 1, "retry the cached result without duplicating the check in progress");
        }
    }
}

#[cfg(test)]
mod workspace_drag_tests {
    use super::*;
    use crate::input::{InputEvent, Key, KeyCode, Mods};
    use crate::server::{persistence::tests::writer, queue, tests};

    fn event(kind: MouseKind, col: u16, row: u16) -> MouseEvent {
        MouseEvent { kind, col, row, mods: Mods::NONE }
    }

    fn names(server: &Server) -> Vec<String> {
        server
            .workspace_picker(0)
            .items
            .iter()
            .filter(|i| server.workspace_key(&i.action).is_some())
            .map(|i| i.label.clone())
            .collect()
    }

    fn press(server: &mut Server, client: ClientId, from: usize) -> (Picker, u16, u16) {
        let picker = server.workspace_picker(server.clients[&client].workspace);
        let layout = crate::server::overlays::picker_layout(&picker, server.client_screen(client));
        let (col, row) = (layout.list.x + 2, layout.list.y);
        let Mode::Picker(picker) = server.on_picker(
            client,
            picker,
            InputEvent::Mouse(event(MouseKind::Press(MouseButton::Left), col, row + from as u16 * 2)),
        ) else {
            panic!()
        };
        assert!(picker.drag.is_some());
        (picker, col, row)
    }

    fn release(server: &mut Server, client: ClientId, picker: Picker, col: u16, row: u16) -> Mode {
        let screen = server.client_screen(client);
        let area = crate::server::overlays::picker_layout(&picker, screen).area;
        let Mode::Picker(picker) =
            server.on_picker(client, picker, InputEvent::Mouse(event(MouseKind::Drag(MouseButton::Left), col, row)))
        else {
            panic!()
        };
        assert_eq!(crate::server::overlays::picker_layout(&picker, screen).area, area);
        if picker.query.is_empty() && picker.drag.as_ref().is_some_and(|d| d.hover.is_some()) {
            let mut buffer = ratatui::buffer::Buffer::empty(screen);
            crate::server::overlays::draw_picker(&picker, screen, &mut buffer);
            assert_eq!(buffer[(col, row)].bg, crate::theme::current().selection);
        }
        server.on_picker(client, picker, InputEvent::Mouse(event(MouseKind::Release(MouseButton::Left), col, row)))
    }

    #[test]
    fn list_drag_moves_across_rows_without_opening_or_changing_any_clients_workspace() {
        for (cols, rows) in [(80, 24), (124, 36)] {
            let (tx, _rx) = queue::channel(32);
            let mut server = tests::server(tx);
            let (client, _peer) = tests::client(&mut server);
            let (other, _peer2) = tests::client(&mut server);
            server.workspaces.pop();
            let current = server.clients[&client].workspace;
            let c = server.clients.get_mut(&client).unwrap();
            (c.cols, c.rows) = (cols, rows);
            let b = server.new_workspace("b".into(), None);
            server.new_workspace("c".into(), None);
            let d = server.new_workspace("d".into(), None);
            let saved = writer(&mut server, vec![]);
            server.keep_and_close_workspace(d).unwrap();
            server.clients.get_mut(&other).unwrap().workspace = b;
            let (picker, col, row) = press(&mut server, client, 3);
            let Mode::Picker(picker) = release(&mut server, client, picker, col, row + 1) else { panic!() };
            assert_eq!(names(&server), ["d", "test", "b", "c"]);
            assert_eq!(picker.current().unwrap().action.workspace(), Some(d));
            assert!(server.workspaces[0].closed());
            assert_eq!(server.clients[&client].workspace, current);
            assert_eq!(server.clients[&other].workspace, b);
            assert!(server.panes.is_empty());
            let snap = serde_json::from_value(saved.lock().unwrap().last().unwrap().clone()).unwrap();
            let (tx, _rx3) = queue::channel(32);
            let mut restored = tests::server(tx);
            crate::persist::apply(&mut restored, snap).unwrap();
            assert_eq!(names(&restored), names(&server));
            let (picker, col, row) = press(&mut server, client, 0);
            assert!(matches!(release(&mut server, client, picker, col, row + 6), Mode::Picker(_)));
            assert_eq!(names(&server), ["test", "b", "c", "d"]);
        }
    }

    #[test]
    fn list_drag_from_an_unselected_scrolled_row_keeps_the_viewport_and_destination() {
        let (tx, _rx) = queue::channel(32);
        let mut server = tests::server(tx);
        let (client, _peer) = tests::client(&mut server);
        let current = server.clients[&client].workspace;
        let c = server.clients.get_mut(&client).unwrap();
        (c.cols, c.rows) = (80, 24);
        for i in 2..=12 {
            server.new_workspace(format!("workspace-{i}"), None);
        }
        let saved = writer(&mut server, vec![]);
        let mut picker = server.workspace_picker(current);
        picker.selected = 11;
        let screen = server.client_screen(client);
        let layout = crate::server::overlays::picker_layout(&picker, screen);
        assert_eq!(layout.start, 6);
        let (col, row) = (layout.list.x + 2, layout.list.y);
        let Mode::Picker(picker) =
            server.on_picker(client, picker, InputEvent::Mouse(event(MouseKind::Press(MouseButton::Left), col, row)))
        else {
            panic!()
        };
        assert_eq!(picker.current().unwrap().label, "workspace-7");
        assert!(picker.drag.is_some());
        assert_eq!(crate::server::overlays::picker_layout(&picker, screen).start, layout.start);
        let Mode::Picker(picker) = server.on_picker(
            client,
            picker,
            InputEvent::Mouse(event(MouseKind::Drag(MouseButton::Left), col, row + layout.row_height)),
        ) else {
            panic!()
        };
        assert_eq!(crate::server::overlays::picker_layout(&picker, screen).start, layout.start);
        assert_eq!(picker.drag.as_ref().unwrap().hover, Some(7));
        let mut buffer = ratatui::buffer::Buffer::empty(screen);
        crate::server::overlays::draw_picker(&picker, screen, &mut buffer);
        for (offset, name) in [(0, "workspace-7"), (layout.row_height, "workspace-8")] {
            let text: String =
                (layout.list.x..layout.list.right()).map(|x| buffer[(x, row + offset)].symbol()).collect();
            assert!(text.contains(name), "{text}");
        }
        let Mode::Picker(picker) = release(&mut server, client, picker, col, row + layout.row_height) else { panic!() };
        let mut expected = vec!["test".to_string()];
        expected.extend((2..=12).map(|i| format!("workspace-{i}")));
        expected.swap(6, 7);
        assert_eq!(names(&server), expected);
        assert_eq!(picker.current().unwrap().label, "workspace-7");
        assert!(picker.drag.is_none());
        assert_eq!(server.clients[&client].workspace, current);
        assert!(server.panes.is_empty());
        assert_eq!(saved.lock().unwrap().len(), 1);
    }

    #[test]
    fn filtered_cancelled_and_stale_drags_never_reorder_or_open_rows() {
        let (tx, _rx) = queue::channel(32);
        let mut server = tests::server(tx);
        let (client, _peer) = tests::client(&mut server);
        let current = server.clients[&client].workspace;
        server.new_workspace("test-b".into(), None);
        let saved = writer(&mut server, vec![]);
        let (mut picker, col, row) = press(&mut server, client, 0);
        picker.query = "test".into();
        picker.refilter();
        let Mode::Picker(picker) = release(&mut server, client, picker, col, row + 2) else { panic!() };
        assert_eq!(picker.message.as_deref(), Some("Clear search to reorder"));
        assert_eq!(picker.current().unwrap().action.workspace(), Some(current));
        assert_eq!(picker.query, "test");
        let (picker, _, _) = press(&mut server, client, 0);
        let Mode::Picker(picker) = release(&mut server, client, picker, 0, 0) else { panic!() };
        assert!(picker.message.is_none());
        assert_eq!(picker.current().unwrap().action.workspace(), Some(current));
        let (picker, col, row) = press(&mut server, client, 0);
        server.new_workspace("new".into(), None);
        let Mode::Picker(picker) = release(&mut server, client, picker, col, row + 2) else { panic!() };
        assert_eq!(picker.message.as_deref(), Some("Order changed. Drag the workspace again."));
        assert_eq!(picker.message_kind, crate::theme::NoticeKind::Warning);
        assert_eq!(names(&server), ["test", "test-b", "new"]);
        assert_eq!(server.clients[&client].workspace, current);
        assert!(saved.lock().unwrap().is_empty());
    }

    #[test]
    fn keyboard_and_focus_changes_cancel_a_list_drag_and_clear_its_hint() {
        let (tx, _rx) = queue::channel(32);
        let mut server = tests::server(tx);
        let (client, _peer) = tests::client(&mut server);
        server.new_workspace("other".into(), None);
        let saved = writer(&mut server, vec![]);
        for code in [KeyCode::Esc, KeyCode::Char('x')] {
            let (picker, col, row) = press(&mut server, client, 0);
            let Mode::Picker(picker) = server.on_picker(
                client,
                picker,
                InputEvent::Mouse(event(MouseKind::Drag(MouseButton::Left), col, row + 2)),
            ) else {
                panic!()
            };
            assert!(picker.message.is_some());
            let Mode::Picker(picker) =
                server.on_picker(client, picker, InputEvent::Key(Key { code, mods: Mods::NONE, raw: vec![] }))
            else {
                panic!()
            };
            assert!(picker.drag.is_none());
            assert!(picker.message.is_none());
        }
        let (picker, _, _) = press(&mut server, client, 0);
        server.clients.get_mut(&client).unwrap().mode = Mode::Picker(picker);
        server.client_input(client, b"\x1b[O");
        let Mode::Picker(picker) = &server.clients[&client].mode else { panic!() };
        assert!(picker.drag.is_none());
        assert_eq!(names(&server), ["test", "other"]);
        assert!(saved.lock().unwrap().is_empty());
    }

    #[test]
    fn failed_drag_keeps_the_previous_order_and_retry_uses_the_same_destination() {
        let (tx, _rx) = queue::channel(32);
        let mut server = tests::server(tx);
        let (client, _peer) = tests::client(&mut server);
        server.new_workspace("b".into(), None);
        server.new_workspace("c".into(), None);
        let _saved = writer(&mut server, vec![Err("disk full".into()), Ok(()), Ok(())]);
        let (picker, col, row) = press(&mut server, client, 0);
        let Mode::Confirm(mut q) = release(&mut server, client, picker, col, row + 4) else { panic!() };
        assert!(q.message.contains("Previous order kept"));
        assert_eq!(names(&server), ["test", "b", "c"]);
        q.yes = true;
        assert!(matches!(
            server.on_confirm(client, q, InputEvent::Key(Key { code: KeyCode::Enter, mods: Mods::NONE, raw: vec![] })),
            Mode::Picker(_)
        ));
        assert_eq!(names(&server), ["b", "c", "test"]);
    }

    #[test]
    fn file_drags_preserve_sources_and_deleted_workspace_slots() {
        use crate::persist::WorkspaceKey;
        use crate::server::ui::PickAction;
        let (tx, _rx) = queue::channel(32);
        let mut server = tests::server(tx);
        let (client, _peer) = tests::client(&mut server);
        let current = server.clients[&client].workspace;
        let deleted = server.new_workspace("deleted".into(), None);
        let b = server.new_workspace("b".into(), None);
        let dir = std::env::temp_dir().join(format!("kraal-drag-file-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("recipe.kdl");
        let source = "// Keep me.\nlayout { pane command=\"never-run\"; }\n";
        std::fs::write(&path, source).unwrap();
        server.config.workspace_dirs.push(dir.display().to_string());
        let keys: Vec<_> = [current, deleted, b]
            .into_iter()
            .map(|ws| server.workspace_key(&PickAction::SwitchWorkspace(ws)).unwrap())
            .chain([WorkspaceKey::Source(crate::workspace::definition_path(&path))])
            .collect();
        server.workspace_order = keys;
        let _saved = writer(&mut server, vec![]);
        let identity = server.keep_and_delete_workspace(deleted).unwrap();
        let (picker, col, row) = press(&mut server, client, 2);
        let Mode::Picker(picker) = release(&mut server, client, picker, col, row) else { panic!() };
        assert_eq!(picker.current().unwrap().label, "recipe");
        assert_eq!(names(&server), ["recipe", "test", "b"]);
        server.restore_deleted_workspace(&identity, "deleted").unwrap();
        assert_eq!(names(&server), ["recipe", "deleted", "test", "b"]);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        assert!(!path.with_extension("kdl.bak").exists());
        assert!(server.panes.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sidebar_drag_keeps_focus_and_escape_cancels_the_pending_click() {
        let (tx, _rx) = queue::channel(32);
        let mut server = tests::server(tx);
        let (client, _peer) = tests::client(&mut server);
        let current = server.clients[&client].workspace;
        server.new_workspace("b".into(), None);
        let c = server.new_workspace("c".into(), None);
        let _saved = writer(&mut server, vec![]);
        let row = |server: &Server, ws| {
            (0..36).find(|&r| server.sidebar_hit(client, r) == Some(SideItem::Workspace(ws))).unwrap()
        };
        let from = row(&server, c);
        let to = row(&server, current);
        assert!(server.on_mouse(client, event(MouseKind::Press(MouseButton::Left), 3, from), false).is_none());
        assert!(server.on_mouse(client, event(MouseKind::Drag(MouseButton::Left), 3, to), false).is_none());
        assert!(server.on_mouse(client, event(MouseKind::Release(MouseButton::Left), 3, to), false).is_none());
        assert_eq!(names(&server), ["c", "test", "b"]);
        assert_eq!(server.clients[&client].workspace, current);
        let from = row(&server, c);
        server.on_mouse(client, event(MouseKind::Press(MouseButton::Left), 3, from), false);
        server.client_input(client, b"\x1b");
        server.flush_client_input(client);
        assert!(server.clients[&client].drag.is_none());
        server.on_mouse(client, event(MouseKind::Release(MouseButton::Left), 3, from), false);
        assert_eq!(server.clients[&client].workspace, current);
        assert!(server.panes.is_empty());
    }
}
