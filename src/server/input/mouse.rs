//! The mouse: clicks, drags, the wheel, and what they hit.

use std::time::{Duration, Instant};

use alacritty_terminal::selection::SelectionType;

use crate::input::{MouseButton, MouseEvent, MouseKind, encode_alternate_scroll, encode_mouse};
use crate::keymap::Action;
use crate::layout::PaneId;
use crate::server::bottom_bar::Click;
use crate::server::render::{BarItem, SideItem};
use crate::server::ui::{Drag, Mode};
use crate::server::{ClientId, Server};

impl Server {
    /// A mouse event; returns the mode it opens (a picker, a question), if any.
    pub(super) fn on_mouse(&mut self, id: ClientId, m: MouseEvent, scroll_mode: bool) -> Option<Mode> {
        if !self.config.mouse {
            return None;
        }
        let c = self.clients.get(&id)?;
        let (rows, sw) = (c.rows, self.sidebar_cols(c));
        let area = self.client_area(id);

        // A drag in progress owns the mouse until release.
        if let Some(drag) = self.clients.get_mut(&id).and_then(|c| c.drag.take()) {
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
            if (matches!(m.kind, MouseKind::Press(MouseButton::Right)) || press_left && m.col >= sw.saturating_sub(4))
                && let Some(SideItem::Workspace(ws)) = self.sidebar_hit(id, m.row)
                && (matches!(m.kind, MouseKind::Press(MouseButton::Right))
                    || self.sidebar_hit(id, m.row.saturating_sub(1)) != Some(SideItem::Workspace(ws)))
            {
                let current = self.clients[&id].workspace;
                let mut picker = self.workspace_picker(current);
                picker.select(|i| i.action.workspace() == Some(ws));
                picker.actions = (picker.action_keys().len() > 1).then_some(0);
                return Some(Mode::Picker(picker));
            }
            if press_left {
                match self.sidebar_hit(id, m.row) {
                    Some(SideItem::Workspaces) => return Some(self.action(id, Action::WorkspacePicker)),
                    Some(SideItem::AddWorkspace) => return Some(self.action(id, Action::NewWorkspace)),
                    Some(SideItem::Accounts) => return Some(self.action(id, Action::Accounts)),
                    Some(SideItem::Account(account)) => return Some(self.account_details(&account)),
                    Some(SideItem::AddAccount) => return Some(self.action(id, Action::AddAccount)),
                    Some(SideItem::Workspace(ws)) => {
                        let mut picker = self.workspace_picker(self.clients[&id].workspace);
                        picker.select(|i| i.action.workspace() == Some(ws));
                        let item = picker.current()?.clone();
                        return Some(self.pick(id, &picker, item));
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
                        picker.select(
                            |i| matches!(&i.action, crate::server::ui::PickAction::OpenDefinition(p) if p == &path),
                        );
                        let item = crate::server::ui::PickItem::new(
                            "",
                            "",
                            crate::server::ui::PickAction::OpenDefinition(path),
                        );
                        return Some(self.pick(id, &picker, item));
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

    pub(super) fn continue_drag(&mut self, id: ClientId, drag: Drag, m: MouseEvent) -> Option<Drag> {
        let released = matches!(m.kind, MouseKind::Release(_));
        let area = self.client_area(id);
        let (_, wi) = self.current_ws(id)?;
        match drag {
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
    fn a_stale_sidebar_file_click_keeps_its_target_and_offers_retry() {
        let (tx, _rx) = crate::server::queue::channel(32);
        let mut server = crate::server::tests::server(tx);
        let (client, _peer) = crate::server::tests::client(&mut server);
        let current = server.clients[&client].workspace;
        let path = std::path::PathBuf::from("/nonexistent/kraal-w6-file.kdl");
        server.def_files.push((path.clone(), "missing".into()));
        let row = (0..36).find(|&r| server.sidebar_hit(client, r) == Some(SideItem::Definition(path.clone()))).unwrap();
        let click = MouseEvent { kind: MouseKind::Press(MouseButton::Left), col: 3, row, mods: Default::default() };
        let Mode::Confirm(q) = server.on_mouse(client, click, false).unwrap() else { panic!() };
        assert!(
            matches!(q.action, crate::server::ui::ConfirmAction::OpenWorkspace(crate::server::ui::PickAction::OpenDefinition(p)) if p == path)
        );
        assert_eq!(server.clients[&client].workspace, current);
        assert_eq!(server.workspaces.len(), 1);
    }

    #[test]
    fn workspace_entries_and_actions_work_with_and_without_the_sidebar() {
        for (cols, rows) in [(80, 24), (124, 36)] {
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
            assert_eq!(sidebar == 0, cols == 80);
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
                assert!(matches!(server.on_picker(client, p, event), Mode::Normal));
                assert_eq!(server.clients[&client].workspace, other, "one click on an unselected row");
                server.clients.get_mut(&client).unwrap().workspace = current;
            }
            if sidebar > 0 {
                let row =
                    (0..rows).find(|&r| server.sidebar_hit(client, r) == Some(SideItem::Workspace(other))).unwrap();
                for (button, col) in [(MouseButton::Right, 3), (MouseButton::Left, sidebar - 3)] {
                    let Mode::Picker(p) = server.on_mouse(client, click(button, col, row), false).unwrap() else {
                        panic!()
                    };
                    assert!(p.actions.is_some());
                    assert_eq!(p.current().unwrap().action.workspace(), Some(other));
                    assert_eq!(server.clients[&client].workspace, current);
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
