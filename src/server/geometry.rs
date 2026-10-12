//! Where things are: the main area, pane rectangles, and which workspace holds a pane.

use ratatui::layout::Rect;

use super::{Server, Tab};
use crate::layout::{Node, PaneId};
/// A layout worked out: the tiles of the tree `root` in `area`, with frames or not.
pub(super) struct Tiling {
    root: Node,
    area: Rect,
    frames: bool,
    tiles: Vec<(PaneId, Rect, bool)>,
}

impl Server {
    /// Main pane area for a client of the given size: below the top bar, one column of space
    /// right of the sidebar.
    fn main_area(&self, cols: u16, rows: u16, sidebar: bool) -> Rect {
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

    /// Where every visible pane of a tab goes, in draw order (tiled, then floating): its frame,
    /// or its whole place when it has none. Panes folded in a stack show only a row
    /// ([`Server::stack_rows`]).
    pub fn tab_rects(&self, tab: &Tab, area: Rect) -> Vec<(PaneId, Rect, bool)> {
        let mut out = Vec::new();
        if let Some(z) = tab.zoom.filter(|z| !tab.is_floating(*z)) {
            out.push((z, self.tile(area), false));
        } else if let Some(root) = &tab.root {
            out.extend(
                self.tiled_rects(root, area).into_iter().filter(|(.., folded)| !folded).map(|(p, r, _)| (p, r, false)),
            );
        }
        if tab.show_floating {
            for f in &tab.floating {
                out.push((f.pane, f.rect.to_rect(area), true));
            }
        }
        out
    }

    /// The rows of the panes folded in the stacks of a tab, as it shows them.
    pub fn stack_rows(&self, tab: &Tab, area: Rect) -> Vec<(PaneId, Rect)> {
        match &tab.root {
            Some(root) if tab.zoom.is_none_or(|z| tab.is_floating(z)) => self
                .tiled_rects(root, area)
                .into_iter()
                .filter(|(.., folded)| *folded)
                .map(|(p, r, _)| (p, r))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Every tiled pane of `root` in `area`, in order: the frame of each pane shown whole, and
    /// the row of each pane folded in a stack (marked `true`).
    fn tiled_rects(&self, root: &Node, area: Rect) -> Vec<(PaneId, Rect, bool)> {
        let frames = self.config.pane_frames;
        // Frames ask for the same layouts over and over: the last few are remembered, by value.
        let mut known = self.tilings.borrow_mut();
        if let Some(i) = known.iter().position(|t| t.area == area && t.frames == frames && t.root == *root) {
            let hit = known.remove(i);
            let tiles = hit.tiles.clone();
            known.push(hit);
            return tiles;
        }
        let tiles = root.tiles(area, frames);
        if known.len() >= 8 {
            known.remove(0);
        }
        known.push(Tiling { root: root.clone(), area, frames, tiles: tiles.clone() });
        tiles
    }

    /// Where a tiled pane's frame goes: its place, less one column of space when a pane is to its
    /// right and one row when a pane is below.
    fn tile(&self, outer: Rect) -> Rect {
        if self.config.pane_frames { crate::layout::framed(outer, self.layout_area) } else { outer }
    }

    /// Where a pane's content goes: inside its frame.
    pub fn inner(&self, frame: Rect) -> Rect {
        if self.config.pane_frames && frame.width >= 4 && frame.height >= 3 {
            Rect::new(frame.x + 1, frame.y + 1, frame.width - 2, frame.height - 2)
        } else {
            frame
        }
    }

    /// A pane's frame and content as a screen of `screen` size shows them: the frame clipped to
    /// the screen, the content inside what shows of the frame.
    pub fn placed(&self, frame: Rect, screen: Rect) -> (Rect, Rect) {
        let frame = frame.intersection(screen);
        (frame, self.inner(frame))
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
                let mut tiled = std::collections::HashMap::new();
                if let Some(root) = &tab.root {
                    let frames: std::collections::HashMap<PaneId, Rect> = self
                        .tiled_rects(root, area)
                        .into_iter()
                        .filter(|(.., folded)| !folded)
                        .map(|(p, r, _)| (p, r))
                        .collect();
                    // A folded pane has the size of the one open in its stack: opening it
                    // resizes nothing.
                    for id in root.leaves() {
                        if let Some(r) = frames.get(&root.holder(id)) {
                            tiled.insert(id, *r);
                        }
                    }
                }
                if let Some(z) = tab.zoom {
                    tiled.insert(z, self.tile(area));
                }
                for (id, r) in tiled {
                    sizes.push((id, self.inner(r)));
                }
                for f in &tab.floating {
                    sizes.push((f.pane, self.inner(f.rect.to_rect(area))));
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
