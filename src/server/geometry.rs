//! Where things are: the main area, pane rectangles, and which workspace holds a pane.

use ratatui::layout::Rect;

use super::{Client, ClientId, Server, Tab};
use crate::layout::{Node, PaneId};
/// A layout worked out: the tiles of the tree `root` in `area`, with frames or not.
pub(super) struct Tiling {
    root: Node,
    area: Rect,
    frames: bool,
    tiles: Vec<(PaneId, Rect, bool)>,
}

impl Server {
    /// Main pane area of client `c`: below the top bar, above the bottom bar, one column of space
    /// right of the sidebar.
    pub(super) fn main_area(&self, c: &Client) -> Rect {
        let top = 1;
        let height = c.rows.saturating_sub(top + self.bottom_rows()).max(1);
        let left = self.panes_left(c);
        Rect::new(left, top, c.cols.saturating_sub(left).max(1), height)
    }

    /// The column client `c`'s panes start at: after its sidebar and a column of space.
    fn panes_left(&self, c: &Client) -> u16 {
        match self.sidebar_cols(c) {
            0 => 0,
            sidebar => sidebar + 1,
        }
    }

    /// Where client `id` sees the layout. The panes have one size, worked out for the primary
    /// client: every client draws them from where its own panes start, so one whose sidebar or
    /// terminal is not the primary's sees the same layout beside its own sidebar, cut where its
    /// screen ends.
    pub(super) fn client_area(&self, id: ClientId) -> Rect {
        let left = self.clients.get(&id).map_or(self.layout_area.x, |c| self.panes_left(c));
        Rect { x: left, ..self.layout_area }
    }

    /// Rows the bottom bar takes, at the bottom of every client's screen.
    pub fn bottom_rows(&self) -> u16 {
        u16::from(self.bottom_bar)
    }

    /// What the panes and everything drawn over them use of a screen of `cols × rows`: all of it
    /// but the bottom bar's row.
    pub fn screen_above_bar(&self, cols: u16, rows: u16) -> Rect {
        Rect::new(0, 0, cols, rows.saturating_sub(self.bottom_rows()))
    }

    /// [`Server::screen_above_bar`] of client `id`.
    pub(super) fn client_screen(&self, id: ClientId) -> Rect {
        let (cols, rows) = self.clients.get(&id).map_or((80, 24), |c| (c.cols, c.rows));
        self.screen_above_bar(cols, rows)
    }

    /// The sidebar's width on a screen of `cols` columns: none on a screen less than three times
    /// as wide as it, where it would leave the panes too little.
    pub fn sidebar_width(&self, cols: u16) -> u16 {
        let w = self.config.sidebar.width;
        if cols / 3 < w { 0 } else { w }
    }

    /// Whether a client has the sidebar: as `[sidebar] visible` says, until it shows or hides it
    /// itself.
    pub fn sidebar_shown(&self, c: &Client) -> bool {
        c.sidebar.unwrap_or(self.config.sidebar.visible)
    }

    /// The columns client `c`'s sidebar takes: none when it has none, or a screen too narrow for
    /// one.
    pub(super) fn sidebar_cols(&self, c: &Client) -> u16 {
        if self.sidebar_shown(c) { self.sidebar_width(c.cols) } else { 0 }
    }

    /// Where every visible pane of a tab goes, in draw order (tiled, then floating): its frame,
    /// or its whole place when it has none. Panes folded in a stack show only a row
    /// ([`Server::stack_rows`]).
    pub fn tab_rects(&self, tab: &Tab, area: Rect) -> Vec<(PaneId, Rect, bool)> {
        let mut out = Vec::new();
        if let Some(z) = tab.zoom.filter(|z| !tab.is_floating(*z)) {
            out.push((z, area, false));
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
            self.layout_area = self.main_area(c);
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
                    tiled.insert(z, area);
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
