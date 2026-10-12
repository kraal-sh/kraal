//! Composes what one client sees: the sidebar, the top bar, panes in their frames, overlays.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Widget};
use unicode_width::UnicodeWidthStr;

use super::overlays;
use super::ui::Mode;
use super::{Client, ClientId, Server};
use crate::agent::AgentState;
use crate::ansi::{Cursor, Kept};
use crate::keymap::Action;
use crate::layout::PaneId;

fn th() -> crate::theme::Theme {
    crate::theme::current()
}

#[derive(Debug, Clone, PartialEq)]
pub enum BarItem {
    Workspace,
    Tab(usize),
    NewTab,
    /// A label a click does nothing on.
    Text,
}

/// One row of the sidebar.
#[derive(PartialEq)]
enum SideRow {
    /// The line that opens a section, with its name.
    Section(&'static str),
    /// A row of an entry: what clicking it does, and whether it is the current one.
    Entry {
        item: Option<SideItem>,
        line: Line<'static>,
        selected: bool,
    },
    Blank,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SideItem {
    Workspace(u32),
    Definition(String),
    Pane(PaneId),
    /// A tunnel, by local port.
    Tunnel(u16),
    /// A local port a pane's program listens on.
    Port(u16),
    /// An agent of a server's kraal session: the server, and its pane's id there.
    Remote {
        server: String,
        pane: u32,
    },
}

/// What a client's last frame was made of: the panes drawn from their screen rows with nothing
/// over them, where, and the versions of the rows drawn; what its chrome was drawn from, when
/// nothing went over it.
#[derive(Default)]
pub struct FrameMemo {
    panes: Vec<(PaneId, Rect, Vec<u64>)>,
    chrome: Option<Chrome>,
}

/// What a frame's chrome is drawn from: the same draws the same cells.
struct Chrome {
    theme: crate::theme::Theme,
    /// The sidebar's place, and each pane's tile, inner place and content, and whether only its
    /// screen rows are drawn there.
    side: Rect,
    places: Vec<(PaneId, Rect, Rect, Rect, bool)>,
    sidebar: Vec<SideRow>,
    bar: TopBar,
    frames: Vec<Option<FrameLook>>,
    /// The rows of the panes folded in stacks, and their frames' looks.
    folds: Vec<(PaneId, Rect)>,
    fold_looks: Vec<Option<FrameLook>>,
}

impl Chrome {
    /// Laid out as `other`: the same things go to the same places, in the same colours.
    fn same_places(&self, other: &Chrome) -> bool {
        self.theme == other.theme && self.side == other.side && self.places == other.places && self.folds == other.folds
    }
}

/// What a pane's frame shows: its place, its colour, its title and what it works on.
#[derive(PartialEq)]
struct FrameLook {
    tile: Rect,
    color: Color,
    title: Vec<Span<'static>>,
    details: Vec<Span<'static>>,
}

/// The panes are drawn apart, on their own cells: none floats, no tile (or row of a pane folded
/// in a stack, `folds`) overlaps another, the sidebar (`side`) or the top bar.
fn apart(placed: &[Placed], folds: &[(PaneId, Rect)], side: Rect) -> bool {
    let places: Vec<Rect> = placed.iter().map(|p| p.tile).chain(folds.iter().map(|(_, r)| *r)).collect();
    !placed.iter().any(|p| p.floating)
        && places
            .iter()
            .enumerate()
            .all(|(i, r)| r.y > 0 && !r.intersects(side) && places[i + 1..].iter().all(|q| !q.intersects(*r)))
}

/// The rows of pane contents the frame already has.
fn kept_rows(placed: &[Placed]) -> Vec<Kept> {
    let mut kept: Vec<Kept> = placed
        .iter()
        .filter_map(|p| Some((p, p.rows.as_ref()?)))
        .flat_map(|(p, rows)| {
            let c = p.content;
            rows.iter().enumerate().filter(|(_, k)| **k).map(move |(r, _)| Kept {
                y: c.y + r as u16,
                x0: c.x,
                x1: c.right(),
            })
        })
        .collect();
    kept.sort_unstable_by_key(|k| (k.y, k.x0));
    kept
}

/// The cells of a screen to draw again, row by row.
struct Changed {
    area: Rect,
    rows: Vec<Vec<(u16, u16)>>,
}

impl Changed {
    fn new(area: Rect) -> Changed {
        Changed { area, rows: vec![Vec::new(); usize::from(area.height)] }
    }

    fn row(&mut self, y: u16, x0: u16, x1: u16) {
        let (x0, x1) = (x0.max(self.area.x), x1.min(self.area.right()));
        if let Some(row) = y.checked_sub(self.area.y).and_then(|r| self.rows.get_mut(usize::from(r)))
            && x0 < x1
        {
            row.push((x0, x1));
        }
    }

    fn rect(&mut self, r: Rect) {
        for y in r.top()..r.bottom() {
            self.row(y, r.left(), r.right());
        }
    }

    /// The edges of `r`.
    fn border(&mut self, r: Rect) {
        if r.is_empty() {
            return;
        }
        self.row(r.top(), r.left(), r.right());
        self.row(r.bottom() - 1, r.left(), r.right());
        for y in r.top() + 1..r.bottom().saturating_sub(1) {
            self.row(y, r.left(), r.left() + 1);
            self.row(y, r.right() - 1, r.right());
        }
    }

    /// Every other cell, row by row.
    fn kept(mut self) -> Vec<Kept> {
        let mut kept = Vec::new();
        for (i, row) in self.rows.iter_mut().enumerate() {
            let y = self.area.y + i as u16;
            row.sort_unstable();
            let mut x = self.area.left();
            for &(x0, x1) in row.iter() {
                if x0 > x {
                    kept.push(Kept { y, x0: x, x1: x0 });
                }
                x = x.max(x1);
            }
            if x < self.area.right() {
                kept.push(Kept { y, x0: x, x1: self.area.right() });
            }
        }
        kept
    }
}

/// A pane as a frame shows it.
struct Placed {
    pane: PaneId,
    tile: Rect,
    inner: Rect,
    floating: bool,
    /// Where its content goes.
    content: Rect,
    /// Drawn from its screen rows: for each row, whether the frame already has it.
    rows: Option<Vec<bool>>,
    /// The versions of its rows, when nothing else is drawn over them.
    versions: Option<Vec<u64>>,
}

impl Server {
    /// Compose what client `id` sees into `buf`, which holds its last frame: the rows of panes
    /// that did not change since are left as they are (and returned, so that the encoder does
    /// not compare them), everything else is drawn again.
    pub fn compose(&mut self, id: ClientId, buf: &mut Buffer) -> (Option<Cursor>, crate::ansi::Links, Vec<Kept>) {
        let Some(c) = self.clients.get_mut(&id) else { return (None, Default::default(), Vec::new()) };
        let full = Rect::new(0, 0, c.cols.max(1), c.rows.max(1));
        let mut memo = std::mem::take(&mut c.memo);
        if buf.area != full {
            *buf = Buffer::empty(full);
            memo = FrameMemo::default();
        }
        if c.rows < 3 || c.cols < 10 {
            buf.reset();
            buf.set_string(0, 0, "kraal: terminal too small", Style::default().fg(Color::Yellow));
            return (None, Default::default(), Vec::new());
        }
        // Pickers, dialogs and menus go over the panes.
        let covered = !matches!(c.mode, Mode::Normal | Mode::Resize | Mode::Scroll | Mode::Search { .. });
        let c = &self.clients[&id];
        let sidebar = self.sidebar_shown(c);
        let rows = c.rows;
        let cols = c.cols;
        let sw = if sidebar { self.sidebar_width(cols) } else { 0 };
        let side = Rect::new(0, 0, sw, if sw > 0 { rows } else { 0 });

        let folds = self
            .ws_index(c.workspace)
            .map_or_else(Vec::new, |wi| self.stack_rows(self.workspaces[wi].tab(), self.layout_area));
        let placed = self.place_panes(id, (full, side), &folds, (!covered).then_some(&memo));
        let c = &self.clients[&id];
        let (frames, fold_looks) = self.frame_looks(id, &placed, &folds);
        let chrome = Chrome {
            theme: th(),
            side,
            places: placed.iter().map(|p| (p.pane, p.tile, p.inner, p.content, p.versions.is_some())).collect(),
            sidebar: if sw > 0 { self.sidebar_rows(c, side.height) } else { Vec::new() },
            bar: self.top_bar(c),
            frames,
            folds,
            fold_looks,
        };
        // The chrome is drawn again where it differs from the last frame's, when nothing went
        // over it then or does now, and it is laid out the same: then the cells between the
        // panes are blank in both.
        let cached = !covered && apart(&placed, &chrome.folds, side);
        let last = memo.chrome.take().filter(|last| cached && last.same_places(&chrome));
        let (kept, sidebar_changed, bar_changed, frames_changed, folds_changed) = match &last {
            Some(last) => {
                let mut changed = Changed::new(full);
                let sidebar_changed = last.sidebar != chrome.sidebar;
                if sidebar_changed {
                    changed.rect(side);
                }
                let bar_changed = last.bar != chrome.bar;
                if bar_changed {
                    changed.rect(Rect::new(side.right(), 0, cols.saturating_sub(side.right()), 1));
                }
                let frames: Vec<bool> = chrome.frames.iter().zip(&last.frames).map(|(now, then)| now != then).collect();
                for (p, _) in placed.iter().zip(&frames).filter(|(_, f)| **f) {
                    changed.border(p.tile);
                }
                let folds: Vec<bool> =
                    chrome.fold_looks.iter().zip(&last.fold_looks).map(|(now, then)| now != then).collect();
                for ((_, row), _) in chrome.folds.iter().zip(&folds).filter(|(_, f)| **f) {
                    changed.row(row.y, row.x, row.right());
                }
                for p in &placed {
                    match (&p.rows, p.versions.is_some()) {
                        (Some(rows), true) => {
                            for (r, _) in rows.iter().enumerate().filter(|(_, k)| !**k) {
                                changed.row(p.content.y + r as u16, p.content.x, p.content.right());
                            }
                        }
                        _ => changed.rect(p.inner),
                    }
                }
                (changed.kept(), sidebar_changed, bar_changed, frames, folds)
            }
            None => (kept_rows(&placed), true, true, vec![true; placed.len()], vec![true; chrome.folds.len()]),
        };
        clear_except(buf, &kept);

        if sw > 0 && sidebar_changed {
            Self::draw_sidebar(&chrome.sidebar, side, buf);
        }
        if bar_changed {
            Self::draw_top_bar(&chrome.bar, cols, buf);
        }
        let mut links = crate::ansi::Links::new();
        let mut cursor = self.draw_panes(
            id,
            buf,
            &mut links,
            &placed,
            (&chrome.frames, &frames_changed),
            (&chrome.folds, &chrome.fold_looks, &folds_changed),
        );
        if !covered {
            memo.panes = placed.into_iter().filter_map(|p| Some((p.pane, p.content, p.versions?))).collect();
        } else {
            memo.panes.clear();
        }
        memo.chrome = cached.then_some(chrome);
        if let Some(c) = self.clients.get_mut(&id) {
            c.memo = memo;
        }

        let c = &self.clients[&id];
        let overlay_cursor = match &c.mode {
            Mode::Picker(p) => Some(overlays::draw_picker(p, full, buf)),
            Mode::Prompt(p) => Some(overlays::draw_prompt(p, full, buf)),
            Mode::Help { scroll } => {
                let keys = [("↑↓", "scroll"), ("esc", "close")];
                overlays::draw_viewer("Help", &self.help_lines(), *scroll, &keys, full, buf);
                None
            }
            Mode::Messages { scroll } => {
                let keys = [("↑↓", "scroll"), ("esc", "close")];
                overlays::draw_viewer("Messages", &self.message_lines(), *scroll, &keys, full, buf);
                None
            }
            Mode::Confirm(q) => {
                overlays::draw_confirm(q, full, buf);
                None
            }
            Mode::Hints(h) => {
                // The labels go over the screen they were made for; if the pane moved, they would
                // point at something else, so there are none (a key then ends hints).
                let place = self.pane_inner_rect(id, h.pane);
                match &h.snapshot {
                    Some(snap) if place == Some(snap.area) => {
                        buf.merge(snap);
                        self.draw_hints(id, h.pane, &h.hints, &h.typed, buf);
                    }
                    Some(_) => {}
                    None => self.draw_hints(id, h.pane, &h.hints, &h.typed, buf),
                }
                None
            }
            Mode::Leader { typed, miss } => {
                let specs: Vec<crate::input::KeySpec> =
                    typed.iter().map(|k| crate::input::KeySpec::new(k.code, k.mods)).collect();
                let mut title = self.keymap.prefix_label();
                if !specs.is_empty() {
                    let keys: Vec<String> = specs.iter().map(|k| crate::keymap::pretty(k, th().ascii)).collect();
                    let named = self.keymap.group_title(&specs).unwrap_or("…");
                    title = format!("{title} {} · {named}", keys.join(" "));
                }
                let entries = self.keymap.menu(typed);
                let items = overlays::menu_items(&entries);
                let footer = overlays::menu_footer(&entries);
                let layout = overlays::menu_layout(&items, full);
                let back = !typed.is_empty();
                overlays::draw_menu(&items, &layout, (&title, &footer), back, miss.as_deref(), buf);
                // Links under the menu would open from it.
                links.retain(|&(x, y), _| !layout.area.contains(ratatui::layout::Position::new(x, y)));
                None
            }
            _ => None,
        };
        let c = &self.clients[&id];
        match &c.mode {
            Mode::Picker(_) | Mode::Prompt(_) => cursor = overlay_cursor.flatten(),
            Mode::Leader { .. } | Mode::Help { .. } | Mode::Messages { .. } | Mode::Confirm(_) | Mode::Hints(_) => {
                cursor = None
            }
            Mode::Search { .. } => {
                // After the query, in the top bar.
                cursor = self.top_bar(c).cursor.map(|x| Cursor {
                    x: x.min(c.cols.saturating_sub(1)),
                    y: 0,
                    shape: crate::ansi::CursorShape::Bar,
                    blinking: true,
                });
            }
            _ => {}
        }
        // Overlays (pickers, prompts…) hide the links under them.
        if !matches!(
            self.clients[&id].mode,
            Mode::Normal | Mode::Leader { .. } | Mode::Resize | Mode::Scroll | Mode::Search { .. }
        ) {
            links.clear();
        }
        (cursor, links, kept)
    }

    /// Where a pane's content is drawn for a client.
    pub(super) fn pane_inner_rect(&self, id: ClientId, pane: PaneId) -> Option<Rect> {
        let wi = self.ws_index(self.clients.get(&id)?.workspace)?;
        let (_, frame, _) =
            self.tab_rects(self.workspaces[wi].tab(), self.layout_area).into_iter().find(|(p, _, _)| *p == pane)?;
        Some(self.inner(frame))
    }

    /// Hint labels over the matches of `pane`: the label on the first cells, the rest of the
    /// match underlined. Labels that no longer fit what was typed disappear.
    fn draw_hints(&self, id: ClientId, pane: PaneId, hints: &[crate::hints::Hint], typed: &str, buf: &mut Buffer) {
        let c = &self.clients[&id];
        let Some(wi) = self.ws_index(c.workspace) else { return };
        let tab = self.workspaces[wi].tab();
        let Some((_, frame, _)) = self.tab_rects(tab, self.layout_area).into_iter().find(|(p, _, _)| *p == pane) else {
            return;
        };
        let (_, inner) = self.placed(frame, buf.area);
        let typed = typed.to_ascii_lowercase();
        let label_style = Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD);
        for h in hints {
            let (x, y) = (inner.x + h.m.col, inner.y + h.m.row);
            if y >= inner.y + inner.height || x >= inner.x + inner.width {
                continue;
            }
            let live = h.label.starts_with(&typed);
            let end = (x + h.m.width).min(inner.x + inner.width);
            for cx in x..end {
                let cell = &mut buf[(cx, y)];
                cell.set_style(if live {
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::UNDERLINED)
                } else {
                    Style::default().fg(th().dim)
                });
            }
            if live {
                for (i, ch) in h.label.chars().enumerate() {
                    let cx = x + i as u16;
                    if cx >= inner.x + inner.width {
                        break;
                    }
                    let style = if i < typed.len() { label_style.fg(Color::DarkGray) } else { label_style };
                    buf[(cx, y)].set_char(ch).set_style(style);
                }
            }
        }
    }

    // ---- top bar ----------------------------------------------------------------------

    /// The one bar, on the first row, right of the sidebar. Left: the workspace when there is no
    /// sidebar to show it, a broadcast in progress (never hidden), then the tabs, or the keys of
    /// the mode in use instead. Right: a message, what the focused pane works on when it has no
    /// frame, agents across workspaces, the menu key, and the session unless it is the default
    /// one. When room runs out, the right side gives way first.
    pub fn top_bar(&self, c: &Client) -> TopBar {
        let mut bar = TopBar::default();
        let Some(wi) = self.ws_index(c.workspace) else { return bar };
        let ws = &self.workspaces[wi];
        let cols = c.cols;
        let sw = if self.sidebar_shown(c) { self.sidebar_width(cols) } else { 0 };
        let mut x = if sw > 0 { sw + 1 } else { 0 };
        if sw == 0 {
            // A broadcast in progress always shows, if need be with a shorter workspace name.
            let broadcasting = ws.sync != super::SyncScope::Off && ws.tab().focus.is_some();
            let name_room = if broadcasting { (cols as usize / 4).clamp(1, 20) } else { 20 };
            let name = format!(" {} ", crate::text::truncate(&ws.name, name_room));
            let w = name.width() as u16;
            let style = Style::default().fg(th().accent).add_modifier(Modifier::BOLD);
            bar.items.push((x, w, BarItem::Workspace, vec![Span::styled(name, style)]));
            x += w + 1;
        }
        let room = cols.saturating_sub(x).min(cols / 2 + 8);
        if let Some(spans) = self.broadcast_spans(c, room) {
            let w = spans_width(&spans).min(cols.saturating_sub(x));
            bar.items.push((x, w, BarItem::Text, spans));
            x += w + 1;
        }
        if let Some((spans, cursor)) = self.mode_spans(c, cols.saturating_sub(x)) {
            let w = spans_width(&spans).min(cols.saturating_sub(x));
            bar.cursor = cursor.map(|at| x.saturating_add(at));
            bar.items.push((x, w, BarItem::Text, spans));
            bar.right = self.bar_right(c, cols.saturating_sub(x + w + 1));
            return bar;
        }
        let tabs = self.tab_labels(ws);
        // The active tab keeps its room, with the counts of hidden ones around it.
        let active = tabs.get(ws.active).map_or(0, |t| t.0);
        bar.right = self.bar_right(c, cols.saturating_sub(x + active + 9));
        let limit = cols.saturating_sub(spans_width(&bar.right) + 1);
        place_tabs(&mut bar.items, x, limit, tabs, ws.active);
        bar
    }

    /// The label of each tab with its width: number and name (the active tab filled in), then the
    /// strongest agent state, zoom, broadcast, a bell. A tab with a production pane is red.
    fn tab_labels(&self, ws: &super::Workspace) -> Vec<(u16, Vec<Span<'static>>)> {
        let mut tabs = Vec::new();
        for (i, tab) in ws.tabs.iter().enumerate() {
            let active = i == ws.active;
            let prod = tab.panes().iter().any(|p| self.pane_production_shown(*p).is_some());
            let mut spans = Vec::new();
            let base = match (active, prod) {
                (true, true) => Style::default().fg(Color::Black).bg(th().production).add_modifier(Modifier::BOLD),
                (true, false) => Style::default().fg(Color::Black).bg(th().accent).add_modifier(Modifier::BOLD),
                (false, true) => Style::default().fg(th().production),
                (false, false) => Style::default().fg(Color::Gray),
            };
            let number = if active || prod { base } else { base.fg(th().dim) };
            spans.push(Span::styled(format!(" {} ", i + 1), number));
            spans.push(Span::styled(format!("{} ", crate::text::truncate(&self.tab_label(tab), 24)), base));
            let mark = |text: &str, color: Color| Span::styled(format!(" {text}"), Style::default().fg(color));
            // Strongest agent state of the tab.
            let state = tab
                .panes()
                .iter()
                .filter_map(|p| self.panes.get(p))
                .filter(|p| p.agent.kind.is_some())
                .map(|p| p.agent.state)
                .max_by_key(|s| match s {
                    AgentState::Blocked => 4,
                    AgentState::Done => 3,
                    AgentState::Working => 2,
                    _ => 0,
                });
            if let Some(s) = state.filter(|s| matches!(s, AgentState::Blocked | AgentState::Done | AgentState::Working))
            {
                spans.push(mark(th().icon(s), th().state_color(s)));
            }
            if tab.zoom.is_some() {
                spans.push(mark("Z", Color::LightBlue));
            }
            let synced = match ws.sync {
                super::SyncScope::Tab => active,
                super::SyncScope::Workspace => true,
                super::SyncScope::Marked => tab.panes().iter().any(|p| ws.marked.contains(p)),
                super::SyncScope::Off => false,
            };
            if synced {
                spans.push(mark("S", th().sync));
            }
            if tab.panes().iter().any(|p| self.panes.get(p).is_some_and(|p| p.bell)) && !active {
                spans.push(mark("!", Color::Red));
            }
            tabs.push((spans_width(&spans), spans));
        }
        tabs
    }

    /// A broadcast in progress, within `room` columns: how many panes get the keys, how many of
    /// them are production panes, and how to stop. With less room the shortcut and the words go
    /// first; the count and PROD stay.
    fn broadcast_spans(&self, c: &Client, room: u16) -> Option<Vec<Span<'static>>> {
        let ws = self.ws_index(c.workspace).map(|i| &self.workspaces[i])?;
        if ws.sync == super::SyncScope::Off {
            return None;
        }
        let targets = ws.sync_targets(ws.tab().focus?);
        let n = targets.len();
        let prod = targets.iter().filter(|p| self.pane_production_shown(**p).is_some()).count();
        let prod_text = if prod > 0 { format!(", {prod} PROD") } else { String::new() };
        let full = format!("BROADCAST → {n} pane{}{prod_text}", if n == 1 { "" } else { "s" });
        // The toggle stops any broadcast, and its key is usually the shorter.
        let stop = self.keymap.hint(Action::BroadcastToggle).or_else(|| self.keymap.hint(Action::BroadcastOff));
        let stop_w = stop.as_ref().map_or(0, |k| k.width() + " stop".len());
        let room = room as usize;
        let tiny = format!("B{n}{}", if prod > 0 { " PROD" } else { "" });
        let what =
            [full, format!("BROADCAST {n}{prod_text}")].into_iter().find(|w| w.width() + 2 <= room).unwrap_or(tiny);
        let color = if prod > 0 { th().production } else { th().sync };
        let mut out = vec![badge_span(&what, color)];
        if let Some(stop) = stop.filter(|_| what.width() + 3 + stop_w <= room) {
            out.push(Span::raw(" "));
            out.extend(key_list(&[(&stop, "stop")]));
        }
        Some(out)
    }

    /// The keys of the mode in use, shown in place of the tabs within `room` columns; for a
    /// search, also the column of its cursor. The leader menu, pickers and dialogs say what they
    /// are themselves.
    fn mode_spans(&self, c: &Client, room: u16) -> Option<(Vec<Span<'static>>, Option<u16>)> {
        let t = th();
        let label = c.mode.label();
        let mut out = Vec::new();
        let mut cursor = None;
        match &c.mode {
            Mode::Resize => {
                out.push(badge_span(label, t.accent));
                out.push(Span::raw(" "));
                out.extend(key_list(&[("←↓↑→", "resize"), ("⏎", "done"), ("esc", "done")]));
            }
            Mode::Scroll => {
                out.push(badge_span(label, t.working));
                out.push(Span::raw(" "));
                let prompts = self.client_focus(c.id).and_then(|p| self.panes.get(&p)).is_some_and(|p| p.has_prompts());
                let mut list = vec![("arrows", "move"), ("v", "select"), ("y", "copy"), ("/", "search")];
                if prompts {
                    list.extend([("p P", "prompts"), ("o", "output")]);
                }
                list.push(("esc", "exit"));
                out.extend(key_list(&list));
            }
            Mode::Search { query, found } => {
                out.push(badge_span(label, t.working));
                out.push(Span::raw(" "));
                out.push(Span::styled("/", Style::default().fg(t.working).add_modifier(Modifier::BOLD)));
                // A long query shows its end, where the typing is.
                let shown = (room as usize / 2).saturating_sub(spans_width(&out) as usize).max(8);
                out.push(Span::raw(crate::text::truncate_start(query, shown)));
                cursor = Some(spans_width(&out));
                out.push(Span::raw("  "));
                if !query.is_empty() && !found {
                    out.push(Span::styled("no match", Style::default().fg(t.production)));
                } else {
                    out.extend(key_list(&[("↑↓", "older/newer"), ("⏎", "keep"), ("esc", "cancel")]));
                }
            }
            Mode::Hints(crate::server::ui::HintsMode { hints, act, .. }) => {
                out.push(badge_span(if *act { "HINTS: ACT" } else { "HINTS: COPY" }, t.blocked));
                out.push(Span::raw(" "));
                if *act {
                    let mut acts: Vec<&str> = Vec::new();
                    for h in hints {
                        let a = h.m.kind.action();
                        if !acts.contains(&a) {
                            acts.push(a);
                        }
                    }
                    out.push(dim_span(&format!("type a label to {} · ", acts.join(", "))));
                    out.extend(key_list(&[("tab", "copy instead"), ("esc", "cancel")]));
                } else {
                    out.push(dim_span("type a label to copy it · "));
                    out.extend(key_list(&[("tab", "act on it instead"), ("esc", "cancel")]));
                }
            }
            _ => return None,
        }
        Some((out, cursor))
    }

    /// The right side of the top bar, within `room` columns, most important first: a message,
    /// the focused pane's details, agents, the menu key, the session. A message too long is cut;
    /// the other pieces show whole or not at all.
    fn bar_right(&self, c: &Client, room: u16) -> Vec<Span<'static>> {
        let t = th();
        let normal = matches!(c.mode, Mode::Normal);
        let waiting = self.attention_queue().len();
        let mut notice = Vec::new();
        if let Some((msg, _)) = &c.toast {
            notice.push(Span::styled(msg.clone(), Style::default().fg(t.blocked)));
        } else if normal && c.welcome && waiting == 0 {
            // Until the menu is opened once.
            notice.push(Span::styled("Welcome to kraal", Style::default().fg(t.accent).add_modifier(Modifier::BOLD)));
            let find = self.keymap.hint(Action::CommandPalette);
            let help = self.keymap.hint(Action::Help);
            let list: Vec<(&str, &str)> = [(find.as_deref(), "find any command"), (help.as_deref(), "help")]
                .into_iter()
                .filter_map(|(k, what)| Some((k?, what)))
                .collect();
            if !list.is_empty() {
                notice.push(Span::raw("  "));
                notice.extend(key_list(&list));
            }
        } else if normal && waiting > 0 {
            // Long commands that ended out of sight wait too: then they are panes, not agents.
            let agents = self.panes.values().filter(|p| p.agent.state.needs_attention()).count()
                + self.remote_agents().filter(|(_, a)| a.state.needs_attention()).count()
                == waiting;
            notice.push(Span::styled(
                format!(
                    "{} {waiting} {}{} need{} you",
                    t.icon(AgentState::Blocked),
                    if agents { "agent" } else { "pane" },
                    if waiting == 1 { "" } else { "s" },
                    if waiting == 1 { "s" } else { "" }
                ),
                Style::default().fg(t.blocked),
            ));
            if let Some(k) = self.keymap.hint(Action::NextAttention) {
                notice.push(Span::raw(" "));
                notice.extend(key_list(&[(&k, "go")]));
            }
        }
        let menu = if normal { key_list(&[(&self.keymap.prefix_label(), "menu")]) } else { Vec::new() };
        let session = if self.session == "main" {
            Vec::new()
        } else {
            vec![Span::styled(self.session.clone(), Style::default().fg(t.dim))]
        };
        let mut pieces = [notice, self.focused_details(c), self.agent_counts(), menu, session];
        // Two columns between pieces, one free at the right edge.
        let room = room.saturating_sub(1);
        let mut used = 0u32;
        let mut keep = [false; 5];
        for (i, p) in pieces.iter_mut().enumerate() {
            if p.is_empty() {
                continue;
            }
            let gap = if used > 0 { 2 } else { 0 };
            if i == 0 && spans_width(p) > room && room >= 12 {
                *p = fit_spans(std::mem::take(p), room);
            }
            let w = u32::from(spans_width(p));
            if used + gap + w <= u32::from(room) {
                keep[i] = true;
                used += gap + w;
            }
        }
        let mut out = Vec::new();
        for (p, kept) in pieces.into_iter().zip(keep) {
            if kept {
                if !out.is_empty() {
                    out.push(Span::raw("  "));
                }
                out.extend(p);
            }
        }
        if !out.is_empty() {
            out.push(Span::raw(" "));
        }
        out
    }

    /// What the focused pane works on, when it has no frame to show it (pane frames off).
    fn focused_details(&self, c: &Client) -> Vec<Span<'static>> {
        let Some(wi) = self.ws_index(c.workspace) else { return Vec::new() };
        let tab = self.workspaces[wi].tab();
        let Some(f) = tab.focus else { return Vec::new() };
        let Some((_, frame, floating)) = self.tab_rects(tab, self.layout_area).into_iter().find(|(p, ..)| *p == f)
        else {
            return Vec::new();
        };
        if floating || self.inner(frame).y > frame.y {
            return Vec::new();
        }
        let mut out = Vec::new();
        if self.pane_production_shown(f).is_some() {
            out.push(badge_span("PROD", th().production));
        }
        out.extend(self.pane_details(f));
        out
    }

    /// Agents across all workspaces and the servers watched: how many are blocked, done and
    /// working.
    fn agent_counts(&self) -> Vec<Span<'static>> {
        let (mut blocked, mut working, mut done) = (0, 0, 0);
        let remote = self.remote_agents().map(|(_, a)| a.state);
        for state in self.panes.values().map(|p| p.agent.state).chain(remote) {
            match state {
                AgentState::Blocked => blocked += 1,
                AgentState::Working => working += 1,
                AgentState::Done => done += 1,
                _ => {}
            }
        }
        let mut out: Vec<Span<'static>> = Vec::new();
        for (n, s) in [(blocked, AgentState::Blocked), (done, AgentState::Done), (working, AgentState::Working)] {
            if n > 0 {
                if !out.is_empty() {
                    out.push(Span::raw(" "));
                }
                out.push(Span::styled(
                    format!("{}{n}", th().icon(s)),
                    Style::default().fg(th().state_color(s)).add_modifier(Modifier::BOLD),
                ));
            }
        }
        out
    }

    pub fn tab_bar_hit(&self, id: ClientId, col: u16) -> Option<BarItem> {
        let c = self.clients.get(&id)?;
        self.top_bar(c).items.into_iter().find(|(x, w, ..)| col >= *x && col < x + w).map(|(_, _, item, _)| item)
    }

    fn draw_top_bar(bar: &TopBar, cols: u16, buf: &mut Buffer) {
        for (x, w, _, spans) in &bar.items {
            buf.set_line(*x, 0, &Line::from(spans.clone()), *w);
        }
        let rw = spans_width(&bar.right);
        if rw > 0 && rw < cols {
            buf.set_line(cols - rw, 0, &Line::from(bar.right.clone()), rw);
        }
    }

    // ---- sidebar ----------------------------------------------------------------------

    /// The sidebar's rows from the top of the screen, `height` rows in all. Each section opens on
    /// a line that carries its name, the first one on the top border; its entries take two rows,
    /// a name after a state dot and a detail below. Accounts sit at the bottom when there is room.
    fn sidebar_rows(&self, c: &Client, height: u16) -> Vec<SideRow> {
        let t = th();
        let mut rows = vec![SideRow::Section("WORKSPACES")];
        let entry = |rows: &mut Vec<SideRow>,
                     item: SideItem,
                     dot: Span<'static>,
                     name: Span<'static>,
                     detail: Span<'static>,
                     selected: bool| {
            rows.push(SideRow::Entry { item: Some(item.clone()), line: Line::from(vec![dot, name]), selected });
            rows.push(SideRow::Entry { item: Some(item), line: Line::from(vec![Span::raw("   "), detail]), selected });
        };
        let dot = |icon: &str, color: Color| Span::styled(format!(" {icon} "), Style::default().fg(color));
        let strong = |text: String| Span::styled(text, Style::default().fg(t.text).add_modifier(Modifier::BOLD));
        let faint = |text: String, color: Color| Span::styled(text, Style::default().fg(color));
        let plural = |n: usize, what: &str| format!("{n} {what}{}", if n == 1 { "" } else { "s" });
        let sep = if t.ascii { " - " } else { " · " };
        let urgency = |s: &AgentState| match s {
            AgentState::Blocked => 4,
            AgentState::Done => 3,
            AgentState::Working => 2,
            AgentState::Idle => 1,
            AgentState::Unknown => 0,
        };

        // Workspaces: open ones with their most urgent agent, then those defined but not running.
        let mut agents = Vec::new();
        for ws in &self.workspaces {
            let panes = ws.panes();
            let states: Vec<(PaneId, AgentState)> = panes
                .iter()
                .filter_map(|p| {
                    self.panes.get(p).filter(|pane| pane.agent.kind.is_some()).map(|pane| (*p, pane.agent.state))
                })
                .collect();
            let (icon, color) = match states.iter().map(|(_, s)| *s).max_by_key(urgency) {
                Some(s) => (t.icon(s), t.state_color(s)),
                None => (t.icon(AgentState::Working), t.dim),
            };
            let detail = format!("{}{sep}{}", plural(ws.tabs.len(), "tab"), plural(panes.len(), "pane"));
            entry(
                &mut rows,
                SideItem::Workspace(ws.id),
                dot(icon, color),
                strong(ws.name.clone()),
                faint(detail, t.dim),
                ws.id == c.workspace,
            );
            agents.extend(states.into_iter().map(|(p, s)| (p, s, ws)));
        }
        for n in self.def_names.iter().filter(|n| !self.workspaces.iter().any(|w| &w.name == *n)) {
            let name = faint(n.clone(), t.dim);
            let detail = faint("not running".into(), t.dim);
            entry(
                &mut rows,
                SideItem::Definition(n.clone()),
                dot(t.icon(AgentState::Idle), t.dim),
                name,
                detail,
                false,
            );
        }

        // Agents, with their state and what runs them; the workspace when it is not this one.
        // Then those of the servers watched, after their server's name.
        let remote: Vec<(&str, &super::remote::RemoteAgent)> = self.remote_agents().collect();
        if !agents.is_empty() || !remote.is_empty() {
            rows.push(SideRow::Blank);
            rows.push(SideRow::Section("AGENTS"));
            for (p, s, ws) in agents {
                let pane = &self.panes[&p];
                let kind = match &pane.spec.account {
                    Some(a) => a.replace(':', "·"),
                    None => pane.agent.kind.clone().unwrap_or_default(),
                };
                let mut detail = format!("{}{sep}{kind}", s.label());
                if ws.id != c.workspace {
                    detail = format!("{detail}{sep}{}", ws.name);
                }
                let color = t.state_color(s);
                entry(
                    &mut rows,
                    SideItem::Pane(p),
                    dot(t.icon(s), color),
                    strong(pane.display_name()),
                    faint(detail, color),
                    false,
                );
            }
            let path = if t.ascii { " > " } else { " › " };
            for (server, a) in remote {
                let color = t.state_color(a.state);
                let item = SideItem::Remote { server: server.to_string(), pane: a.pane };
                let name =
                    vec![dot(t.icon(a.state), color), faint(format!("{server}{path}"), t.dim), strong(a.name.clone())];
                let detail = format!("{}{sep}{}{sep}{}", a.state.label(), a.agent, a.workspace);
                rows.push(SideRow::Entry { item: Some(item.clone()), line: Line::from(name), selected: false });
                let detail = Line::from(vec![Span::raw("   "), faint(detail, color)]);
                rows.push(SideRow::Entry { item: Some(item), line: detail, selected: false });
            }
        }

        // Tunnels kraal keeps open, and dev servers seen in panes.
        if !self.tunnels.is_empty() {
            rows.push(SideRow::Blank);
            rows.push(SideRow::Section("TUNNELS"));
            for tunnel in &self.tunnels {
                let (icon, color) = match tunnel.state {
                    super::net::TunnelState::Up => (t.icon(AgentState::Working), t.done),
                    super::net::TunnelState::Connecting => (t.icon(AgentState::Idle), t.working),
                    super::net::TunnelState::Down => (t.icon(AgentState::Blocked), t.blocked),
                };
                let detail = format!("{}:{}", tunnel.spec.server, tunnel.spec.remote_port);
                let name = strong(format!(":{}", tunnel.spec.local));
                entry(
                    &mut rows,
                    SideItem::Tunnel(tunnel.spec.local),
                    dot(icon, color),
                    name,
                    faint(detail, t.dim),
                    false,
                );
            }
        }
        if !self.ports.is_empty() {
            rows.push(SideRow::Blank);
            rows.push(SideRow::Section("PORTS"));
            for (port, pane) in &self.ports {
                let mut name = self.panes.get(pane).map(|p| p.display_name()).unwrap_or_default();
                // Which worktree it serves, when that is not the pane's name already.
                if let Some(wt) = self.pane_worktree_name(*pane).filter(|w| !name.split_whitespace().any(|n| n == w)) {
                    name = format!("{name}{sep}{wt}");
                }
                let icon = t.icon(AgentState::Working);
                entry(
                    &mut rows,
                    SideItem::Port(*port),
                    dot(icon, t.accent),
                    strong(format!(":{port}")),
                    faint(name, t.dim),
                    false,
                );
            }
        }

        // Usage of the agent accounts kraal has heard about, one row each, at the bottom.
        let now = crate::accounts::now();
        let accounts: Vec<Line<'static>> = self
            .accounts
            .all()
            .into_iter()
            .filter_map(|a| {
                let peak = self.usage.get(&a)?.peak(now);
                let color = if peak >= self.accounts.threshold(&a) as f64 {
                    t.blocked
                } else if peak >= 70.0 {
                    t.working
                } else {
                    t.text
                };
                Some(Line::from(vec![
                    Span::styled(format!("   {} ", crate::accounts::label(&a)), Style::default().fg(Color::Gray)),
                    Span::styled(format!("{peak:.0}%"), Style::default().fg(color).add_modifier(Modifier::BOLD)),
                ]))
            })
            .collect();
        // They end on the last row before the bottom border, and give way when there is no room.
        let top = (height as usize).saturating_sub(2).saturating_sub(accounts.len());
        if !accounts.is_empty() && rows.len() <= top {
            rows.resize_with(top, || SideRow::Blank);
            rows.push(SideRow::Section("ACCOUNTS"));
            rows.extend(accounts.into_iter().map(|line| SideRow::Entry { item: None, line, selected: false }));
        }
        rows
    }

    pub fn sidebar_hit(&self, id: ClientId, row: u16) -> Option<SideItem> {
        let c = self.clients.get(&id)?;
        // The last row is the box's bottom border, whatever would follow there.
        if row.saturating_add(1) >= c.rows {
            return None;
        }
        match self.sidebar_rows(c, c.rows).into_iter().nth(row as usize)? {
            SideRow::Entry { item, .. } => item,
            _ => None,
        }
    }

    /// The sidebar: one box over the whole height, its sections split by lines across it.
    fn draw_sidebar(rows: &[SideRow], area: Rect, buf: &mut Buffer) {
        if area.width < 4 || area.height < 3 {
            return;
        }
        let set = th().border(false);
        let line = Style::default().fg(th().dim);
        Block::default().borders(Borders::ALL).border_set(set).border_style(line).render(area, buf);
        let (left, right) = th().tees();
        let width = area.width - 2;
        for (i, row) in rows.iter().enumerate() {
            let y = area.y + i as u16;
            if y + 1 >= area.bottom() {
                break;
            }
            match row {
                SideRow::Section(name) => {
                    if i > 0 {
                        buf[(area.x, y)].set_symbol(left).set_style(line);
                        for x in area.x + 1..area.right() - 1 {
                            buf[(x, y)].set_symbol(set.horizontal_top).set_style(line);
                        }
                        buf[(area.right() - 1, y)].set_symbol(right).set_style(line);
                    }
                    let title = Line::from(vec![
                        Span::styled(set.horizontal_top, line),
                        Span::styled(
                            format!(" {name} "),
                            Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD),
                        ),
                    ]);
                    buf.set_line(area.x + 1, y, &title, width);
                }
                SideRow::Entry { line: text, selected, .. } => {
                    if *selected {
                        for x in area.x + 1..area.right() - 1 {
                            buf[(x, y)].set_bg(th().selection);
                        }
                    }
                    buf.set_line(area.x + 1, y, text, width);
                }
                SideRow::Blank => {}
            }
        }
    }

    // ---- panes ------------------------------------------------------------------------

    /// The panes client `id` sees, in drawing order, and for those drawn from their screen rows,
    /// which rows its last frame already has: with `memo` (nothing is to go over the panes), the
    /// rows of a pane at the same place with the same versions, when nothing else is drawn on
    /// its content (`side` is the sidebar's place) and they show no image.
    fn place_panes(
        &mut self,
        id: ClientId,
        (screen, side): (Rect, Rect),
        folds: &[(PaneId, Rect)],
        memo: Option<&FrameMemo>,
    ) -> Vec<Placed> {
        let Some(wi) = self.clients.get(&id).and_then(|c| self.ws_index(c.workspace)) else { return Vec::new() };
        let rects = self.tab_rects(self.workspaces[wi].tab(), self.layout_area);
        let mut placed: Vec<Placed> = rects
            .into_iter()
            .map(|(pane, frame, floating)| {
                let (tile, inner) = self.placed(frame, screen);
                Placed { pane, tile, inner, floating, content: inner, rows: None, versions: None }
            })
            .filter(|p| !p.tile.is_empty())
            .collect();
        // The rows of panes folded in stacks (`folds`) are drawn too.
        let folded = folds.iter().map(|(p, r)| (*p, r.intersection(screen)));
        let tiles: Vec<(PaneId, Rect)> = placed.iter().map(|p| (p.pane, p.tile)).chain(folded).collect();
        for p in &mut placed {
            let Some(pane) = self.panes.get_mut(&p.pane) else { continue };
            p.content = Rect::new(p.inner.x, p.inner.y, p.inner.width.min(pane.cols), p.inner.height.min(pane.rows));
            if p.inner.is_empty() {
                continue;
            }
            let content = p.content;
            let exited = pane.exit_code.is_some();
            let Some(rows) = pane.refresh_screen(content.width, content.height) else { continue };
            let alone = memo.is_some()
                && !p.floating
                && !exited
                && content.y > 0
                && !content.intersects(side)
                && tiles.iter().all(|(q, t)| *q == p.pane || !t.intersects(content));
            let versions = rows.versions();
            let same = memo.and_then(|m| m.panes.iter().find(|(q, r, _)| *q == p.pane && *r == content));
            p.rows = Some(match same {
                Some((_, _, old)) if alone => {
                    versions.iter().enumerate().map(|(r, v)| old.get(r) == Some(v) && !rows.has_images(r)).collect()
                }
                _ => vec![false; versions.len()],
            });
            p.versions = alone.then(|| versions.to_vec());
        }
        placed
    }

    /// Draw the rows of the panes folded in stacks (`folds`: where, their frames' looks, and
    /// whether to draw each), then the panes `placed`: their frames (`looks`) where `frames` says
    /// to, and the rows of their content the frame does not have yet.
    #[allow(clippy::type_complexity)]
    fn draw_panes(
        &mut self,
        id: ClientId,
        buf: &mut Buffer,
        links: &mut crate::ansi::Links,
        placed: &[Placed],
        (looks, frames): (&[Option<FrameLook>], &[bool]),
        folds: (&[(PaneId, Rect)], &[Option<FrameLook>], &[bool]),
    ) -> Option<Cursor> {
        let c = &self.clients[&id];
        let wi = self.ws_index(c.workspace)?;
        let images = self.can_show_images(id);
        let tab = self.workspaces[wi].tab();
        let focus = tab.focus;
        let show_floating = tab.show_floating;
        let floating_focus = focus.is_some_and(|f| tab.is_floating(f));
        // A pane folded in a stack shows the top line of its frame, and nothing else.
        for (((_, row), look), _) in folds.0.iter().zip(folds.1).zip(folds.2).filter(|(_, f)| **f) {
            let mut whole = Buffer::empty(Rect::new(row.x, row.y, row.width, 3));
            if let Some(look) = look {
                Self::draw_frame(look, &mut whole);
            }
            for x in row.x..row.right() {
                if buf.area.contains((x, row.y).into()) {
                    buf[(x, row.y)] = whole[(x, row.y)].clone();
                }
            }
        }
        let mut cursor = None;
        for (i, p) in placed.iter().enumerate() {
            let (pid, tile, inner, floating, content) = (p.pane, p.tile, p.inner, p.floating, p.content);
            if floating {
                Clear.render(tile, buf);
                // A floating pane hides the links of what is under it.
                links.retain(|(x, y), _| !tile.contains((*x, *y).into()));
            }
            let focused = focus == Some(pid) && (!floating_focus || show_floating);
            if let Some(look) = looks.get(i).and_then(Option::as_ref)
                && frames.get(i).copied().unwrap_or(true)
            {
                Self::draw_frame(look, buf);
            }
            // A smaller client may see only the frame's top.
            if inner.is_empty() {
                continue;
            }
            let Some(pane) = self.panes.get_mut(&pid) else { continue };
            match (&p.rows, pane.screen_cache()) {
                (Some(kept), Some(screen)) => {
                    screen.draw(content, buf, |r| !kept[r], links);
                    // Only rows with image placeholders change with the images.
                    let rows: Vec<u16> = (0..content.height).filter(|r| screen.has_images(usize::from(*r))).collect();
                    for r in rows {
                        pane.show_images(Rect::new(content.x, content.y + r, content.width, 1), buf, images);
                    }
                }
                _ => {
                    if let crate::pane::Content::Files(f) = &mut pane.content {
                        f.set_viewer(id);
                    }
                    pane.render(content, buf, links);
                    pane.show_images(content, buf, images);
                }
            }
            if let Some(code) = pane.exit_code {
                let msg = match code {
                    _ if pane.suspended => " not started · enter run · x close ".to_string(),
                    Some(c) => format!(" exited ({c}) · enter restart · x close "),
                    None => " exited · enter restart · x close ".to_string(),
                };
                let y = inner.y + inner.height.saturating_sub(1);
                let color = if pane.suspended {
                    Color::Yellow
                } else if code == Some(0) {
                    Color::Gray
                } else {
                    Color::LightRed
                };
                let style = Style::default().fg(Color::Black).bg(color);
                buf.set_stringn(inner.x, y, msg, inner.width as usize, style);
            }
            // The copy-mode cursor shows where a selection will start.
            if focused && matches!(self.clients[&id].mode, Mode::Normal | Mode::Leader { .. } | Mode::Scroll) {
                cursor = self.panes[&pid].cursor(content);
            }
        }
        cursor
    }

    /// What a pane works on: git, contexts (production ones in red), the agent and its state,
    /// how far back it is scrolled.
    fn pane_details(&self, pid: PaneId) -> Vec<Span<'static>> {
        let Some(pane) = self.panes.get(&pid) else { return Vec::new() };
        let mut out = Vec::new();
        if let Some(g) = self.pane_git(pid) {
            let color = if g.dirty || g.untracked { Color::Yellow } else { th().dim };
            out.push(Span::styled(format!(" {} ", g.label(th().ascii)), Style::default().fg(color)));
        }
        for (label, is_prod) in self.context_labels(pid) {
            let color = if is_prod { th().production } else { th().dim };
            out.push(Span::styled(format!(" {label} "), Style::default().fg(color)));
        }
        if let Some(kind) = &pane.agent.kind {
            let state = pane.agent.state;
            let kind = pane.spec.account.as_ref().map(|a| a.replace(':', "·")).unwrap_or_else(|| kind.clone());
            let sep = if th().ascii { "-" } else { "·" };
            out.push(Span::styled(
                format!(" {} {} {sep} {kind} ", th().icon(state), state.label()),
                Style::default().fg(th().state_color(state)),
            ));
        }
        // The last command the shell marked failed: its status, and how long it ran if that
        // was long enough to matter.
        if let Some((code, took)) = pane.last_failure() {
            let took = took.filter(|d| d.as_secs() >= 10).map(|d| format!(" {}", crate::pane::short_duration(d)));
            let mark = if th().ascii { "x" } else { "✗" };
            out.push(Span::styled(
                format!(" {mark} {code}{} ", took.unwrap_or_default()),
                Style::default().fg(Color::LightRed),
            ));
        }
        let offset = pane.display_offset();
        if offset > 0 {
            out.push(Span::styled(format!(" ↑{offset} "), Style::default().fg(Color::LightBlue)));
        }
        out
    }

    /// The frames of the panes `placed` that have one, and of the panes folded in stacks
    /// (`folds`: their rows, each the top of a frame), as client `id` sees them.
    fn frame_looks(
        &self,
        id: ClientId,
        placed: &[Placed],
        folds: &[(PaneId, Rect)],
    ) -> (Vec<Option<FrameLook>>, Vec<Option<FrameLook>>) {
        let Some(wi) = self.clients.get(&id).and_then(|c| self.ws_index(c.workspace)) else {
            return (placed.iter().map(|_| None).collect(), folds.iter().map(|_| None).collect());
        };
        let ws = &self.workspaces[wi];
        let tab = ws.tab();
        let focus = tab.focus;
        let synced: Vec<PaneId> = match (ws.sync, focus) {
            (super::SyncScope::Off, _) | (_, None) => Vec::new(),
            (_, Some(f)) => ws.sync_targets(f),
        };
        let floating_focus = focus.is_some_and(|f| tab.is_floating(f));
        let frames = placed
            .iter()
            .map(|p| {
                if p.inner == p.tile {
                    return None;
                }
                let focused = focus == Some(p.pane) && (!floating_focus || tab.show_floating);
                let (sync, mark) = (synced.contains(&p.pane), ws.marked.contains(&p.pane));
                self.frame_look(p.pane, p.tile, focused, sync, p.floating, mark)
            })
            .collect();
        let folded = folds
            .iter()
            .map(|(pane, row)| {
                let (sync, mark) = (synced.contains(pane), ws.marked.contains(pane));
                self.frame_look(*pane, Rect::new(row.x, row.y, row.width, 3), false, sync, false, mark)
            })
            .collect();
        (frames, folded)
    }

    /// A pane's frame: its name on the top border, what it works on at the right of it. The
    /// focused pane's frame is in the accent colour, a production one in red, a broadcast target
    /// in the broadcast colour.
    fn frame_look(
        &self,
        pid: PaneId,
        tile: Rect,
        focused: bool,
        sync: bool,
        floating: bool,
        marked: bool,
    ) -> Option<FrameLook> {
        let pane = self.panes.get(&pid)?;
        let prod = self.pane_production_shown(pid).is_some();
        let color = if prod {
            th().production
        } else if sync {
            th().sync
        } else if focused {
            th().accent
        } else {
            th().dim
        };
        let mut title = vec![Span::styled(
            format!(" {} ", pane.display_name()),
            if focused {
                Style::default().fg(color).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            },
        )];
        if prod {
            title.push(badge_span("PROD", th().production));
            if sync {
                title.push(Span::styled(" sync ", Style::default().fg(th().sync).add_modifier(Modifier::BOLD)));
            }
        }
        if floating {
            title.insert(0, Span::styled(" ◇", Style::default().fg(color)));
        }
        if marked {
            title.insert(0, Span::styled(" ◆", Style::default().fg(th().sync).add_modifier(Modifier::BOLD)));
        }
        Some(FrameLook { tile, color, title, details: self.pane_details(pid) })
    }

    fn draw_frame(look: &FrameLook, buf: &mut Buffer) {
        let tile = look.tile;
        let tw = spans_width(&look.title);
        Block::default()
            .borders(Borders::ALL)
            .border_set(th().border(false))
            .border_style(Style::default().fg(look.color))
            .title(Line::from(look.title.clone()))
            .render(tile, buf);
        // The details end one line cell before the corner, and only show whole.
        let rw = spans_width(&look.details);
        if rw > 0 && u32::from(tw) + u32::from(rw) + 4 <= u32::from(tile.width) {
            buf.set_line(tile.right() - 2 - rw, tile.y, &Line::from(look.details.clone()), rw);
        }
    }

    // ---- help and messages -------------------------------------------------------------

    /// The reference, from the keymap in use: the keys after the prefix, each menu, the direct
    /// keys, then the keys of lists, dialogs and modes, and the mouse.
    pub(super) fn help_lines(&self) -> Vec<Line<'static>> {
        let km = &self.keymap;
        let p = km.prefix_label();
        let mut out = vec![
            Line::from(vec![
                Span::raw("Press "),
                Span::styled(p.clone(), Style::default().fg(th().accent).add_modifier(Modifier::BOLD)),
                Span::raw(", then a key: a menu shows what each key does. "),
            ]),
            Line::from(match km.hint(crate::keymap::Action::CommandPalette) {
                Some(k) => format!("Search every command with {k}. Keys preset: {}.", km.preset),
                None => format!("Keys preset: {}.", km.preset),
            }),
        ];
        if !km.menus.is_empty() {
            out.push(Line::from(format!("Lowercase: a theme's main action ({p} s). Capital: its menu ({p} S).")));
        }
        out.push(Line::raw(""));
        let sections = km.reference(th().ascii);
        let key_w = sections.iter().flat_map(|(_, v)| v.iter().map(|(k, _)| k.width())).max().unwrap_or(10).min(26);
        for (title, rows) in sections {
            if rows.is_empty() {
                continue;
            }
            out.push(overlays::heading(&title));
            for (k, what) in rows {
                out.push(overlays::entry(&k, &what, key_w));
            }
            out.push(Line::raw(""));
        }
        out.push(Line::from(Span::styled(
            format!(
                "Exited command panes: ⏎ restarts, x closes. Settings: {}",
                crate::paths::shorten_home(&crate::paths::config_path())
            ),
            Style::default().fg(th().dim),
        )));
        out
    }

    /// The messages log, newest first.
    pub(super) fn message_lines(&self) -> Vec<Line<'static>> {
        if self.messages.is_empty() {
            return vec![Line::from(Span::styled("No messages yet.", Style::default().fg(th().dim)))];
        }
        self.messages
            .iter()
            .rev()
            .map(|m| {
                let when = crate::text::local_datetime(crate::text::unix_seconds(m.at));
                let style = if m.warning { Style::default().fg(th().blocked) } else { Style::default() };
                Line::from(vec![
                    Span::styled(format!("{}  ", &when[11..]), Style::default().fg(th().dim)),
                    Span::styled(m.text.clone(), style),
                ])
            })
            .collect()
    }
}

/// One entry of the top bar: column, width, what a click does, and the label.
pub type BarEntry = (u16, u16, BarItem, Vec<Span<'static>>);

/// What the top bar shows: entries from the left, the right side, and the column of the cursor
/// while a search is typed.
#[derive(Default, PartialEq)]
pub struct TopBar {
    pub items: Vec<BarEntry>,
    pub right: Vec<Span<'static>>,
    pub cursor: Option<u16>,
}

/// The window of tabs shown from column `x` to `limit`: the active one, then neighbours while
/// they fit. The hidden ones are counted at the edges.
fn place_tabs(out: &mut Vec<BarEntry>, mut x: u16, limit: u16, tabs: Vec<(u16, Vec<Span<'static>>)>, active: usize) {
    let room = limit.saturating_sub(x + 4);
    let active = active.min(tabs.len().saturating_sub(1));
    let (mut lo, mut hi) = (active, active);
    let mut used = tabs.get(active).map_or(0, |t| t.0 + 1);
    loop {
        let marks = |lo: usize, hi: usize| if lo > 0 { 4 } else { 0 } + if hi + 1 < tabs.len() { 4 } else { 0 };
        let mut grew = false;
        if hi + 1 < tabs.len() && used + tabs[hi + 1].0 + 1 + marks(lo, hi + 1) <= room {
            hi += 1;
            used += tabs[hi].0 + 1;
            grew = true;
        }
        if lo > 0 && used + tabs[lo - 1].0 + 1 + marks(lo - 1, hi) <= room {
            lo -= 1;
            used += tabs[lo].0 + 1;
            grew = true;
        }
        if !grew {
            break;
        }
    }
    let more = |n: usize, left: bool| {
        let text = if left { format!("‹{n} ") } else { format!(" {n}›") };
        vec![Span::styled(text, Style::default().fg(th().dim))]
    };
    let n = tabs.len();
    if lo > 0 {
        out.push((x, 3, BarItem::Tab(lo - 1), more(lo, true)));
        x += 4;
    }
    for (i, (w, spans)) in tabs.into_iter().enumerate().skip(lo).take(hi + 1 - lo) {
        out.push((x, w, BarItem::Tab(i), spans));
        x += w + 1;
    }
    if hi + 1 < n {
        out.push((x, 3, BarItem::Tab(hi + 1), more(n - hi - 1, false)));
        x += 4;
    }
    if x + 3 <= limit {
        out.push((x, 3, BarItem::NewTab, vec![Span::styled(" + ", Style::default().fg(th().dim))]));
    }
}

/// Blank every cell of `buf` but those `kept` (sorted by row, then column).
fn clear_except(buf: &mut Buffer, kept: &[Kept]) {
    let area = buf.area;
    let width = usize::from(area.width);
    let mut from = 0;
    for k in kept {
        if k.y < area.y || k.x0 < area.x {
            continue;
        }
        let row = usize::from(k.y - area.y) * width;
        let (start, end) = (row + usize::from(k.x0 - area.x), row + usize::from(k.x1 - area.x));
        if let Some(cells) = buf.content.get_mut(from..start) {
            cells.iter_mut().for_each(ratatui::buffer::Cell::reset);
        }
        from = from.max(end);
    }
    if let Some(cells) = buf.content.get_mut(from..) {
        cells.iter_mut().for_each(ratatui::buffer::Cell::reset);
    }
}

fn spans_width(spans: &[Span]) -> u16 {
    spans.iter().map(|s| s.content.width()).sum::<usize>().min(u16::MAX as usize) as u16
}

/// `spans` cut to `max` columns, ending with "…".
fn fit_spans(spans: Vec<Span<'static>>, max: u16) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut left = max as usize;
    for s in spans {
        let w = s.content.width();
        if w <= left {
            left -= w;
            out.push(s);
        } else {
            if left > 0 {
                out.push(Span::styled(crate::text::truncate(&s.content, left), s.style));
            }
            break;
        }
    }
    out
}

fn badge_span(text: &str, color: Color) -> Span<'static> {
    Span::styled(format!(" {text} "), Style::default().fg(Color::Black).bg(color).add_modifier(Modifier::BOLD))
}

fn dim_span(text: &str) -> Span<'static> {
    Span::styled(text.to_string(), Style::default().fg(th().dim))
}

/// Keys and what they do: "esc close · ⏎ open".
fn key_list(list: &[(&str, &str)]) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    for (i, (k, what)) in list.iter().enumerate() {
        if i > 0 {
            out.push(dim_span(" · "));
        }
        out.push(Span::styled(overlays::shown_key(k), Style::default().fg(th().accent).add_modifier(Modifier::BOLD)));
        out.push(dim_span(&format!(" {what}")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cells_kept_are_all_but_those_changed() {
        let mut changed = Changed::new(Rect::new(0, 0, 10, 4));
        changed.row(0, 2, 4);
        changed.row(0, 3, 6);
        changed.border(Rect::new(5, 1, 3, 3));
        changed.rect(Rect::new(8, 3, 5, 1));
        let k = |y, x0, x1| Kept { y, x0, x1 };
        let want =
            vec![k(0, 0, 2), k(0, 6, 10), k(1, 0, 5), k(1, 8, 10), k(2, 0, 5), k(2, 6, 7), k(2, 8, 10), k(3, 0, 5)];
        assert_eq!(changed.kept(), want);
    }

    #[test]
    fn messages_too_long_are_cut() {
        let spans = vec![Span::raw("deploy "), Span::raw("failed on web01")];
        let cut = fit_spans(spans, 12);
        assert_eq!(spans_width(&cut), 12);
        assert_eq!(cut.iter().map(|s| s.content.as_ref()).collect::<String>(), "deploy fail…");
    }
}
