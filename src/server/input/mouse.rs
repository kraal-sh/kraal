//! The mouse: clicks, drags, the wheel, and what they hit.

use std::time::{Duration, Instant};

use alacritty_terminal::selection::SelectionType;

use crate::input::{MouseButton, MouseEvent, MouseKind, encode_alternate_scroll, encode_mouse};
use crate::keymap::Action;
use crate::layout::PaneId;
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
        let (cols, rows, sidebar) = (c.cols, c.rows, self.sidebar_shown(c));
        let area = self.layout_area;

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
                    Some(BarItem::NewTab) => return Some(self.action(id, Action::NewTab)),
                    Some(BarItem::Workspace) => {
                        let ws = self.clients[&id].workspace;
                        return Some(Mode::Picker(self.workspace_picker(ws)));
                    }
                    None => {}
                }
                self.mark_dirty();
            }
            return None;
        }
        if m.row + 1 >= rows {
            return None;
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
                            self.open_for(id, &format!("http://{addr}"));
                        } else {
                            self.copy_to_client(id, &addr);
                        }
                    }
                    None => {}
                }
            }
            return None;
        }

        let (_, wi) = self.current_ws(id)?;
        let rects = self.tab_rects(self.workspaces[wi].tab(), area);
        let hit = rects.iter().rev().find(|(_, r, _)| contains(*r, m.col, m.row)).copied();

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
                // Borders of tiled panes.
                if button == MouseButton::Left
                    && hit.is_none_or(|(_, _, floating)| !floating)
                    && let Some(b) = self.border_at(wi, m.col, m.row)
                {
                    if let Some(c) = self.clients.get_mut(&id) {
                        c.drag = Some(Drag::Border(b));
                    }
                    return None;
                }
                let (p, r, floating) = hit?;
                if button == MouseButton::Left && m.mods.ctrl && m.row == r.y && self.config.pane_frames {
                    self.toggle_mark(id, p);
                    return None;
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
                        return None;
                    }
                    if m.row == r.y + r.height - 1 && m.col == r.x + r.width - 1 {
                        if let Some(c) = self.clients.get_mut(&id) {
                            c.drag = Some(Drag::ResizeFloat { pane: p });
                        }
                        return None;
                    }
                }
                let inner = self.inner(r);
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
            Drag::Select { pane, from, moved } => {
                // A click that does not move selects nothing: it only focuses the pane, and the
                // clipboard keeps what it had.
                let moved = moved || (m.col, m.row) != from;
                let rects = self.tab_rects(self.workspaces[wi].tab(), area);
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
                let tab = self.workspaces[wi].tab_mut();
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
                let tab = self.workspaces[wi].tab_mut();
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
                let rects = self.tab_rects(self.workspaces[wi].tab(), area);
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

    /// A tiled border under the cursor (the frame lines on both sides of a split count).
    pub(super) fn border_at(&self, wi: usize, col: u16, row: u16) -> Option<crate::layout::Border> {
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
