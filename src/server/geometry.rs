//! Where things are: the main area, pane rectangles, and which workspace holds a pane.

use ratatui::layout::Rect;

use super::{Server, Tab};
use crate::layout::PaneId;
impl Server {
    /// Main pane area for a client of the given size: below the top bar, one column of space
    /// right of the sidebar.
    pub fn main_area(&self, cols: u16, rows: u16, sidebar: bool) -> Rect {
        let top = 1;
        let height = rows.saturating_sub(top).max(1);
        let left = if sidebar { self.sidebar_width(cols) } else { 0 };
        let left = if left > 0 { left + 1 } else { 0 };
        Rect::new(left, top, cols.saturating_sub(left).max(1), height)
    }

    pub fn sidebar_width(&self, cols: u16) -> u16 {
        let w = self.config.sidebar.width;
        if cols < w * 3 { 0 } else { w }
    }

    /// Whether a client sees the sidebar: as it chose, else when there is more to show than the
    /// one workspace it is in (other workspaces, agents, ports, tunnels).
    pub fn sidebar_shown(&self, c: &super::Client) -> bool {
        c.sidebar.unwrap_or_else(|| self.config.sidebar.visible && self.sidebar_useful())
    }

    pub(super) fn sidebar_useful(&self) -> bool {
        self.workspaces.len() > 1
            || !self.def_names.is_empty()
            || !self.tunnels.is_empty()
            || !self.ports.is_empty()
            || self.panes.values().any(|p| p.agent.kind.is_some())
            || self.remote_agents().next().is_some()
    }

    /// Outer rectangle of every visible pane of a tab, in draw order (tiled, then floating).
    pub fn tab_rects(&self, tab: &Tab, area: Rect) -> Vec<(PaneId, Rect, bool)> {
        let mut out = Vec::new();
        if let Some(z) = tab.zoom.filter(|z| !tab.is_floating(*z)) {
            out.push((z, area, false));
        } else if let Some(root) = &tab.root {
            let rects = root.layout(area);
            for id in root.leaves() {
                out.push((id, rects[&id], false));
            }
        }
        if tab.show_floating {
            for f in &tab.floating {
                out.push((f.pane, f.rect.to_rect(area), true));
            }
        }
        out
    }

    /// Where a tiled pane's frame goes: its place, less one column of space when a pane is to its
    /// right and one row when a pane is below.
    pub fn tile(&self, outer: Rect) -> Rect {
        if !self.config.pane_frames {
            return outer;
        }
        let area = self.layout_area;
        let right = u16::from(outer.right() < area.right() && outer.width > 1);
        let below = u16::from(outer.bottom() < area.bottom() && outer.height > 1);
        Rect::new(outer.x, outer.y, outer.width - right, outer.height - below)
    }

    /// Where a pane's content goes inside its place: inside its frame, a box for a floating
    /// pane, a tile for the others.
    pub fn inner(&self, outer: Rect, floating: bool) -> Rect {
        self.inside_frame(if floating { outer } else { self.tile(outer) })
    }

    /// A pane's frame and content as a screen of `screen` size shows them: the frame clipped to
    /// the screen, the content inside what shows of the frame.
    pub fn placed(&self, outer: Rect, floating: bool, screen: Rect) -> (Rect, Rect) {
        let frame = if floating { outer } else { self.tile(outer) }.intersection(screen);
        (frame, self.inside_frame(frame))
    }

    fn inside_frame(&self, frame: Rect) -> Rect {
        if self.config.pane_frames && frame.width >= 4 && frame.height >= 3 {
            Rect::new(frame.x + 1, frame.y + 1, frame.width - 2, frame.height - 2)
        } else {
            frame
        }
    }

    /// Recompute the layout area from the primary client and resize every pane.
    pub fn relayout(&mut self) {
        if let Some(c) = self.primary.and_then(|p| self.clients.get(&p)) {
            self.layout_area = self.main_area(c.cols, c.rows, self.sidebar_shown(c));
        }
        let area = self.layout_area;
        let mut sizes = Vec::new();
        for ws in &self.workspaces {
            for tab in &ws.tabs {
                let mut tiled = tab.root.as_ref().map(|r| r.layout(area)).unwrap_or_default();
                if let Some(z) = tab.zoom {
                    tiled.insert(z, area);
                }
                for (id, r) in tiled {
                    sizes.push((id, self.inner(r, false)));
                }
                for f in &tab.floating {
                    sizes.push((f.pane, self.inner(f.rect.to_rect(area), true)));
                }
            }
        }
        for (id, r) in sizes {
            if let Some(p) = self.panes.get_mut(&id) {
                p.resize(r.width, r.height);
            }
        }
        for c in self.clients.values_mut() {
            c.dirty = true;
        }
        self.schedule_render();
    }

    pub fn ws_index(&self, id: u32) -> Option<usize> {
        self.workspaces.iter().position(|w| w.id == id)
    }

    /// (workspace index, tab index) holding a pane.
    pub fn workspace_of(&self, pane: PaneId) -> Option<(usize, usize)> {
        for (wi, ws) in self.workspaces.iter().enumerate() {
            for (ti, tab) in ws.tabs.iter().enumerate() {
                if tab.contains(pane) {
                    return Some((wi, ti));
                }
            }
        }
        None
    }
}
