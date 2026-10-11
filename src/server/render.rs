//! Composes what one client sees: tab bar, sidebar, panes with frames, status bar, overlays.

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
use crate::ansi::Cursor;
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
}

impl Server {
    pub fn compose(&mut self, id: ClientId) -> (Buffer, Option<Cursor>, crate::ansi::Links) {
        let c = &self.clients[&id];
        let full = Rect::new(0, 0, c.cols.max(1), c.rows.max(1));
        let mut buf = Buffer::empty(full);
        if c.rows < 3 || c.cols < 10 {
            buf.set_string(0, 0, "kraal: terminal too small", Style::default().fg(Color::Yellow));
            return (buf, None, Default::default());
        }
        let sidebar = self.sidebar_shown(c);
        let rows = c.rows;
        let cols = c.cols;

        self.draw_tab_bar(id, &mut buf);
        let sw = if sidebar { self.sidebar_width(cols) } else { 0 };
        if sw > 0 {
            self.draw_sidebar(id, Rect::new(0, 1, sw, rows - 2), &mut buf);
        }
        let mut links = crate::ansi::Links::new();
        let mut cursor = self.draw_panes(id, &mut buf, &mut links);
        self.draw_status(id, Rect::new(0, rows - 1, cols, 1), &mut buf);

        let c = &self.clients[&id];
        let overlay_cursor = match &c.mode {
            Mode::Picker(p) => Some(overlays::draw_picker(p, full, &mut buf)),
            Mode::Prompt(p) => Some(overlays::draw_prompt(p, full, &mut buf)),
            Mode::Help { scroll } => {
                let keys = [("↑↓", "scroll"), ("esc", "close")];
                overlays::draw_viewer("Help", &self.help_lines(), *scroll, &keys, full, &mut buf);
                None
            }
            Mode::Messages { scroll } => {
                let keys = [("↑↓", "scroll"), ("esc", "close")];
                overlays::draw_viewer("Messages", &self.message_lines(), *scroll, &keys, full, &mut buf);
                None
            }
            Mode::Confirm(q) => {
                overlays::draw_confirm(q, full, &mut buf);
                None
            }
            Mode::Hints(h) => {
                // The labels go over the screen they were made for; if the pane moved, they would
                // point at something else, so there are none (a key then ends hints).
                let place = self.pane_inner_rect(id, h.pane);
                match &h.snapshot {
                    Some(snap) if place == Some(snap.area) => {
                        buf.merge(snap);
                        self.draw_hints(id, h.pane, &h.hints, &h.typed, &mut buf);
                    }
                    Some(_) => {}
                    None => self.draw_hints(id, h.pane, &h.hints, &h.typed, &mut buf),
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
                let items = overlays::menu_items(&self.keymap.menu(typed));
                let layout = overlays::menu_layout(&items, full);
                overlays::draw_menu(&items, &layout, &title, !typed.is_empty(), miss.as_deref(), &mut buf);
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
            Mode::Search { query, .. } => {
                // After the badge, " " and "/" in the status bar.
                let x = (c.mode.label().len() as u16 + 4 + query.width() as u16).min(c.cols.saturating_sub(1));
                cursor = Some(Cursor { x, y: c.rows - 1, shape: crate::ansi::CursorShape::Bar, blinking: true });
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
        (buf, cursor, links)
    }

    /// Where a pane's content is drawn for a client.
    pub(super) fn pane_inner_rect(&self, id: ClientId, pane: PaneId) -> Option<Rect> {
        let wi = self.ws_index(self.clients.get(&id)?.workspace)?;
        let (_, outer, _) =
            self.tab_rects(self.workspaces[wi].tab(), self.layout_area).into_iter().find(|(p, _, _)| *p == pane)?;
        Some(self.inner(outer))
    }

    /// Hint labels over the matches of `pane`: the label on the first cells, the rest of the
    /// match underlined. Labels that no longer fit what was typed disappear.
    fn draw_hints(&self, id: ClientId, pane: PaneId, hints: &[crate::hints::Hint], typed: &str, buf: &mut Buffer) {
        let c = &self.clients[&id];
        let Some(wi) = self.ws_index(c.workspace) else { return };
        let tab = self.workspaces[wi].tab();
        let Some((_, outer, _)) = self.tab_rects(tab, self.layout_area).into_iter().find(|(p, _, _)| *p == pane) else {
            return;
        };
        let inner = self.inner(outer).intersection(buf.area);
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

    // ---- tab bar ----------------------------------------------------------------------

    /// Clickable items of the tab bar with their column ranges and labels. When the tabs do not
    /// fit, the active one stays in view and the hidden ones are counted at the edges.
    pub fn tab_bar_items(&self, c: &Client) -> Vec<(u16, u16, BarItem, Vec<Span<'static>>)> {
        let mut out = Vec::new();
        let Some(wi) = self.ws_index(c.workspace) else { return out };
        let ws = &self.workspaces[wi];
        let mut x = 0u16;
        let badge = format!(" {} ", crate::text::truncate(&ws.name, 20));
        let w = badge.width() as u16;
        out.push((
            x,
            w,
            BarItem::Workspace,
            vec![Span::styled(badge, Style::default().fg(Color::Black).bg(th().accent).add_modifier(Modifier::BOLD))],
        ));
        x += w + 1;
        let mut tabs: Vec<(u16, Vec<Span<'static>>)> = Vec::new();
        for (i, tab) in ws.tabs.iter().enumerate() {
            let active = i == ws.active;
            let mut spans = Vec::new();
            let base = if active {
                Style::default().fg(Color::Black).bg(Color::Gray).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            spans.push(Span::styled(format!(" {} ", i + 1), base.fg(if active { Color::Black } else { th().dim })));
            spans.push(Span::styled(crate::text::truncate(&self.tab_label(tab), 24), base));
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
                spans.push(Span::styled(format!(" {}", th().icon(s)), base.fg(th().state_color(s))));
            }
            if tab.zoom.is_some() {
                spans.push(Span::styled(" Z", base.fg(Color::LightBlue)));
            }
            let synced = match ws.sync {
                super::SyncScope::Tab => active,
                super::SyncScope::Workspace => true,
                super::SyncScope::Marked => tab.panes().iter().any(|p| ws.marked.contains(p)),
                super::SyncScope::Off => false,
            };
            if synced {
                spans.push(Span::styled(" S", base.fg(th().sync)));
            }
            if tab.panes().iter().any(|p| self.panes.get(p).is_some_and(|p| p.bell)) && !active {
                spans.push(Span::styled(" !", base.fg(Color::Red)));
            }
            spans.push(Span::styled(" ", base));
            let w: u16 = spans.iter().map(|s| s.content.width() as u16).sum();
            tabs.push((w, spans));
        }
        // The window of tabs shown: the active one, then neighbours while they fit.
        let limit = c.cols.saturating_sub(self.tab_bar_right_width() + 1);
        let room = limit.saturating_sub(x + 4);
        let active = ws.active.min(tabs.len().saturating_sub(1));
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
        if lo > 0 {
            out.push((x, 3, BarItem::Tab(lo - 1), more(lo, true)));
            x += 4;
        }
        for (i, (w, spans)) in tabs.into_iter().enumerate().skip(lo).take(hi + 1 - lo) {
            out.push((x, w, BarItem::Tab(i), spans));
            x += w + 1;
        }
        if hi + 1 < ws.tabs.len() {
            out.push((x, 3, BarItem::Tab(hi + 1), more(ws.tabs.len() - hi - 1, false)));
            x += 4;
        }
        if x + 3 <= limit {
            out.push((x, 3, BarItem::NewTab, vec![Span::styled(" + ", Style::default().fg(th().dim))]));
        }
        out
    }

    /// The right side of the tab bar: agents across all workspaces, then the session name.
    fn tab_bar_right(&self) -> Vec<Span<'static>> {
        let (mut blocked, mut working, mut done) = (0, 0, 0);
        for p in self.panes.values() {
            match p.agent.state {
                AgentState::Blocked => blocked += 1,
                AgentState::Working => working += 1,
                AgentState::Done => done += 1,
                _ => {}
            }
        }
        let mut right: Vec<Span<'static>> = Vec::new();
        for (n, s) in [(blocked, AgentState::Blocked), (done, AgentState::Done), (working, AgentState::Working)] {
            if n > 0 {
                right.push(Span::styled(
                    format!("{}{n} ", th().icon(s)),
                    Style::default().fg(th().state_color(s)).add_modifier(Modifier::BOLD),
                ));
            }
        }
        right.push(Span::styled(format!(" {} ", self.session), Style::default().fg(th().dim)));
        right
    }

    fn tab_bar_right_width(&self) -> u16 {
        self.tab_bar_right().iter().map(|s| s.content.width() as u16).sum()
    }

    pub fn tab_bar_hit(&self, id: ClientId, col: u16) -> Option<BarItem> {
        let c = self.clients.get(&id)?;
        self.tab_bar_items(c).into_iter().find(|(x, w, ..)| col >= *x && col < x + w).map(|(_, _, item, _)| item)
    }

    fn draw_tab_bar(&self, id: ClientId, buf: &mut Buffer) {
        let c = &self.clients[&id];
        let width = c.cols;
        for (x, w, _, spans) in self.tab_bar_items(c) {
            buf.set_line(x, 0, &Line::from(spans), w);
        }
        let right = self.tab_bar_right();
        let rw: u16 = right.iter().map(|s| s.content.width() as u16).sum();
        if rw < width {
            buf.set_line(width - rw, 0, &Line::from(right), rw);
        }
    }

    // ---- sidebar ----------------------------------------------------------------------

    /// Sidebar lines, top to bottom, with what clicking each one does.
    pub fn sidebar_rows(&self, c: &Client) -> Vec<(Option<SideItem>, Line<'static>)> {
        let mut rows = Vec::new();
        rows.push((
            None,
            Line::from(Span::styled(" WORKSPACES", Style::default().fg(th().dim).add_modifier(Modifier::BOLD))),
        ));
        for ws in &self.workspaces {
            let current = ws.id == c.workspace;
            let panes = ws.panes();
            let agents: Vec<PaneId> =
                panes.iter().copied().filter(|p| self.panes.get(p).is_some_and(|p| p.agent.kind.is_some())).collect();
            let mut spans = vec![
                Span::styled(
                    if current { format!(" {} ", th().pointer()) } else { "   ".into() },
                    Style::default().fg(th().accent),
                ),
                Span::styled(
                    ws.name.clone(),
                    if current {
                        Style::default().add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::Gray)
                    },
                ),
            ];
            for s in [AgentState::Blocked, AgentState::Done] {
                let n = agents.iter().filter(|p| self.panes[p].agent.state == s).count();
                if n > 0 {
                    spans.push(Span::styled(
                        format!(" {}{n}", th().icon(s)),
                        Style::default().fg(th().state_color(s)).add_modifier(Modifier::BOLD),
                    ));
                }
            }
            rows.push((Some(super::render::SideItem::Workspace(ws.id)), Line::from(spans)));
            for p in agents {
                let pane = &self.panes[&p];
                let s = pane.agent.state;
                let kind = match &pane.spec.account {
                    Some(a) => a.replace(':', "·"),
                    None => pane.agent.kind.clone().unwrap_or_default(),
                };
                rows.push((
                    Some(SideItem::Pane(p)),
                    Line::from(vec![
                        Span::raw("     "),
                        Span::styled(format!("{} ", th().icon(s)), Style::default().fg(th().state_color(s))),
                        Span::styled(format!("{kind} "), Style::default().fg(th().state_color(s))),
                        Span::styled(pane.display_name(), Style::default().fg(Color::Gray)),
                    ]),
                ));
            }
        }
        // Tunnels kraal keeps open, and dev servers seen in panes.
        if !self.tunnels.is_empty() {
            rows.push((None, Line::raw("")));
            rows.push((
                None,
                Line::from(Span::styled(" TUNNELS", Style::default().fg(th().dim).add_modifier(Modifier::BOLD))),
            ));
            for t in &self.tunnels {
                let (icon, color) = match t.state {
                    super::net::TunnelState::Up => (th().icon(AgentState::Working), th().done),
                    super::net::TunnelState::Connecting => (th().icon(AgentState::Idle), th().blocked),
                    super::net::TunnelState::Down => (th().icon(AgentState::Blocked), Color::LightRed),
                };
                rows.push((
                    Some(SideItem::Tunnel(t.spec.local)),
                    Line::from(vec![
                        Span::styled(format!("   {icon} "), Style::default().fg(color)),
                        Span::styled(
                            format!(":{} ", t.spec.local),
                            Style::default().fg(th().text).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("{}:{}", t.spec.server, t.spec.remote_port),
                            Style::default().fg(th().dim),
                        ),
                    ]),
                ));
            }
        }
        if !self.ports.is_empty() {
            rows.push((None, Line::raw("")));
            rows.push((
                None,
                Line::from(Span::styled(" PORTS", Style::default().fg(th().dim).add_modifier(Modifier::BOLD))),
            ));
            for (port, pane) in &self.ports {
                let name = self.panes.get(pane).map(|p| p.display_name()).unwrap_or_default();
                rows.push((
                    Some(SideItem::Port(*port)),
                    Line::from(vec![
                        Span::styled(
                            format!("   :{port} "),
                            Style::default().fg(th().accent).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(name, Style::default().fg(th().dim)),
                    ]),
                ));
            }
        }
        // Usage of the agent accounts kraal has heard about.
        let now = crate::accounts::now();
        let accounts: Vec<(String, f64, bool)> = self
            .accounts
            .all()
            .into_iter()
            .filter_map(|a| {
                let u = self.usage.get(&a)?;
                let peak = u.peak(now);
                Some((crate::accounts::label(&a), peak, peak >= self.accounts.threshold(&a) as f64))
            })
            .collect();
        if !accounts.is_empty() {
            rows.push((None, Line::raw("")));
            rows.push((
                None,
                Line::from(Span::styled(" ACCOUNTS", Style::default().fg(th().dim).add_modifier(Modifier::BOLD))),
            ));
            for (label, peak, full) in accounts {
                let color = if full {
                    Color::Yellow
                } else if peak >= 70.0 {
                    Color::LightYellow
                } else {
                    Color::Gray
                };
                rows.push((
                    None,
                    Line::from(vec![
                        Span::styled(format!("   {label} "), Style::default().fg(Color::Gray)),
                        Span::styled(format!("{peak:.0}%"), Style::default().fg(color).add_modifier(Modifier::BOLD)),
                    ]),
                ));
            }
        }
        let defined: Vec<&String> =
            self.def_names.iter().filter(|n| !self.workspaces.iter().any(|w| &w.name == *n)).collect();
        if !defined.is_empty() {
            rows.push((None, Line::raw("")));
            rows.push((
                None,
                Line::from(Span::styled(" NOT RUNNING", Style::default().fg(th().dim).add_modifier(Modifier::BOLD))),
            ));
            for n in defined {
                rows.push((
                    Some(SideItem::Definition(n.clone())),
                    Line::from(Span::styled(format!("   {n}"), Style::default().fg(th().dim))),
                ));
            }
        }
        rows
    }

    pub fn sidebar_hit(&self, id: ClientId, row: u16) -> Option<SideItem> {
        let c = self.clients.get(&id)?;
        let i = row.checked_sub(1)? as usize;
        self.sidebar_rows(c).into_iter().nth(i).and_then(|(item, _)| item)
    }

    fn draw_sidebar(&self, id: ClientId, area: Rect, buf: &mut Buffer) {
        let c = &self.clients[&id];
        let inner_w = area.width.saturating_sub(1);
        for (i, (_, line)) in self.sidebar_rows(c).into_iter().enumerate() {
            let y = area.y + i as u16;
            if y >= area.y + area.height {
                break;
            }
            buf.set_line(area.x, y, &line, inner_w);
        }
        // Next to pane frames, the line starts and stops half a cell in, level with their top and
        // bottom edges, instead of running past their rounded corners.
        let framed = self.config.pane_frames && !th().ascii && th().frames != crate::theme::Frames::Ascii;
        let last = area.y + area.height - 1;
        for y in area.y..=last {
            let symbol = match y {
                _ if !framed || area.height < 2 => th().vline(),
                y if y == area.y => "╷",
                y if y == last => "╵",
                _ => th().vline(),
            };
            if let Some(cell) = buf.cell_mut((area.x + area.width - 1, y)) {
                cell.set_symbol(symbol).set_fg(th().dim);
            }
        }
    }

    // ---- panes ------------------------------------------------------------------------

    fn draw_panes(&mut self, id: ClientId, buf: &mut Buffer, links: &mut crate::ansi::Links) -> Option<Cursor> {
        let c = &self.clients[&id];
        let wi = self.ws_index(c.workspace)?;
        let area = self.layout_area;
        let clip = buf.area;
        let ws = &self.workspaces[wi];
        let tab = ws.tab();
        let rects = self.tab_rects(tab, area);
        let focus = tab.focus;
        let synced: Vec<PaneId> = match (ws.sync, focus) {
            (super::SyncScope::Off, _) | (_, None) => Vec::new(),
            (_, Some(f)) => ws.sync_targets(f),
        };
        let marked = ws.marked.clone();
        let show_floating = tab.show_floating;
        let floating_focus = focus.is_some_and(|f| tab.is_floating(f));
        let mut cursor = None;
        // A pane alone in its tab needs no heavier frame to show it has the focus.
        let alone = rects.len() == 1;
        for (pid, outer, floating) in rects {
            let outer = outer.intersection(clip);
            if outer.is_empty() {
                continue;
            }
            if floating {
                Clear.render(outer, buf);
            }
            let focused = focus == Some(pid) && (!floating_focus || show_floating);
            let inner = self.inner(outer);
            if inner != outer {
                let emphasis = focused && !alone;
                self.draw_frame(pid, outer, emphasis, synced.contains(&pid), floating, marked.contains(&pid), buf);
            }
            let Some(pane) = self.panes.get_mut(&pid) else { continue };
            let content = Rect::new(inner.x, inner.y, inner.width.min(pane.cols), inner.height.min(pane.rows));
            if floating {
                // A floating pane hides the links of what is under it.
                links.retain(|(x, y), _| !outer.contains((*x, *y).into()));
            }
            if let crate::pane::Content::Files(f) = &mut pane.content {
                f.set_viewer(id);
            }
            pane.render(content, buf, links);
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

    #[allow(clippy::too_many_arguments)]
    fn draw_frame(
        &self,
        pid: PaneId,
        outer: Rect,
        focused: bool,
        sync: bool,
        floating: bool,
        marked: bool,
        buf: &mut Buffer,
    ) {
        let Some(pane) = self.panes.get(&pid) else { return };
        let state = pane.agent.state;
        let prod = self.pane_production_shown(pid).is_some();
        let color = if prod {
            th().production
        } else if sync {
            th().sync
        } else if focused {
            th().accent
        } else if pane.agent.kind.is_some() && state == AgentState::Blocked {
            Color::Yellow
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
            title.push(Span::styled(
                " PROD ",
                Style::default().fg(Color::Black).bg(th().production).add_modifier(Modifier::BOLD),
            ));
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
        let mut right = Vec::new();
        if let Some(g) = self.pane_git(pid) {
            let color = if g.dirty || g.untracked { Color::Yellow } else { th().dim };
            right.push(Span::styled(format!(" {} ", g.label(th().ascii)), Style::default().fg(color)));
        }
        for (label, is_prod) in self.context_labels(pid) {
            let color = if is_prod { th().production } else { th().dim };
            right.push(Span::styled(format!(" {label} "), Style::default().fg(color)));
        }
        if let Some(kind) = &pane.agent.kind {
            let kind = pane.spec.account.as_ref().map(|a| a.replace(':', "·")).unwrap_or_else(|| kind.clone());
            right.push(Span::styled(
                format!(" {} {kind} {} ", th().icon(state), state.label()),
                Style::default().fg(th().state_color(state)),
            ));
        }
        let offset = pane.display_offset();
        if offset > 0 {
            right.push(Span::styled(format!(" ↑{offset} "), Style::default().fg(Color::LightBlue)));
        }
        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_set(th().border(focused))
            .border_style(Style::default().fg(color))
            .title(Line::from(title));
        if !right.is_empty() {
            block = block.title(Line::from(right).right_aligned());
        }
        block.render(outer, buf);
    }

    // ---- status bar -------------------------------------------------------------------

    /// The bottom line: what the client is doing and the keys that matter now. In normal use it
    /// stays quiet: a broadcast in progress (never hidden), a message, agents needing you, and how
    /// to open the menu.
    fn draw_status(&self, id: ClientId, area: Rect, buf: &mut Buffer) {
        let c = &self.clients[&id];
        let t = th();
        let badge = |text: &str, color: Color| {
            Span::styled(format!(" {text} "), Style::default().fg(Color::Black).bg(color).add_modifier(Modifier::BOLD))
        };
        let key =
            |k: &str| Span::styled(overlays::shown_key(k), Style::default().fg(t.accent).add_modifier(Modifier::BOLD));
        let text = |s: &str| Span::styled(s.to_string(), Style::default().fg(t.dim));
        let keys = |list: &[(&str, &str)]| -> Vec<Span<'static>> {
            let mut out = Vec::new();
            for (i, (k, what)) in list.iter().enumerate() {
                if i > 0 {
                    out.push(text(" · "));
                }
                out.push(key(k));
                out.push(text(&format!(" {what}")));
            }
            out
        };
        let mut left: Vec<Span<'static>> = Vec::new();
        let mut right: Vec<Span<'static>> = Vec::new();
        // A broadcast stays in view whatever else happens.
        if let Some(ws) = self.ws_index(c.workspace).map(|i| &self.workspaces[i])
            && ws.sync != super::SyncScope::Off
            && let Some(f) = ws.tab().focus
        {
            let targets = ws.sync_targets(f);
            let n = targets.len();
            let prod = targets.iter().filter(|p| self.pane_production_shown(**p).is_some()).count();
            let prod_text = if prod > 0 { format!(", {prod} PROD") } else { String::new() };
            let full = format!("BROADCAST → {n} pane{}{prod_text}", if n == 1 { "" } else { "s" });
            let stop = self.keymap.hint(Action::BroadcastOff).or_else(|| self.keymap.hint(Action::BroadcastToggle));
            let stop_w = stop.as_ref().map_or(0, |k| k.width() + " stop  ".len());
            // On a narrow screen, the count and PROD stay; the words and the shortcut go first.
            let room = area.width as usize / 2 + 8;
            let what = if full.width() + 2 <= room { full } else { format!("BROADCAST {n}{prod_text}") };
            let color = if prod > 0 { t.production } else { t.sync };
            left.push(badge(&what, color));
            left.push(Span::raw(" "));
            if let Some(stop) = stop.filter(|_| what.width() + 3 + stop_w <= room) {
                left.extend(keys(&[(&stop, "stop")]));
                left.push(Span::raw("  "));
            }
        }
        let label = c.mode.label();
        match &c.mode {
            Mode::Normal => {
                let waiting = self.attention_queue().len();
                if let Some((msg, _)) = &c.toast {
                    left.push(Span::styled(msg.clone(), Style::default().fg(t.blocked)));
                } else if c.welcome && waiting == 0 {
                    // Until the menu is opened once.
                    left.push(Span::styled(
                        "Welcome to kraal",
                        Style::default().fg(t.accent).add_modifier(Modifier::BOLD),
                    ));
                    left.push(text("   "));
                    let find = self.keymap.hint(Action::CommandPalette);
                    let help = self.keymap.hint(Action::Help);
                    let list: Vec<(&str, &str)> = [(find.as_deref(), "find any command"), (help.as_deref(), "help")]
                        .into_iter()
                        .filter_map(|(k, what)| Some((k?, what)))
                        .collect();
                    left.extend(keys(&list));
                } else {
                    if waiting > 0 {
                        left.push(Span::styled(
                            format!(
                                "{} {waiting} agent{} need{} you",
                                t.icon(AgentState::Blocked),
                                if waiting == 1 { "" } else { "s" },
                                if waiting == 1 { "s" } else { "" }
                            ),
                            Style::default().fg(t.blocked),
                        ));
                        if let Some(k) = self.keymap.hint(Action::NextAttention) {
                            left.push(text("  "));
                            left.extend(keys(&[(&k, "go")]));
                        }
                    }
                }
                right.extend(keys(&[(&self.keymap.prefix_label(), "menu")]));
            }
            Mode::Leader { typed, .. } => {
                // The keys typed so far; the menu says the rest. A message that comes meanwhile
                // shows here.
                left.push(badge(label, t.accent));
                left.push(Span::raw(" "));
                let mut seq = vec![self.keymap.prefix_label()];
                seq.extend(
                    typed.iter().map(|k| crate::keymap::pretty(&crate::input::KeySpec::new(k.code, k.mods), t.ascii)),
                );
                left.push(Span::styled(seq.join(" "), Style::default().fg(t.text)));
                if let Some((msg, _)) = &c.toast {
                    left.push(Span::raw("   "));
                    left.push(Span::styled(msg.clone(), Style::default().fg(t.blocked)));
                }
            }
            Mode::Resize => {
                left.push(badge(label, t.accent));
                left.push(Span::raw(" "));
                left.extend(keys(&[("←↓↑→", "resize"), ("⏎", "done"), ("esc", "done")]));
            }
            Mode::Scroll => {
                left.push(badge(label, t.working));
                left.push(Span::raw(" "));
                left.extend(keys(&[
                    ("arrows", "move"),
                    ("v", "select"),
                    ("y", "copy"),
                    ("/", "search"),
                    ("esc", "exit"),
                ]));
            }
            Mode::Search { query, found } => {
                left.push(badge(label, t.working));
                left.push(Span::raw(" "));
                left.push(Span::styled("/", Style::default().fg(t.working).add_modifier(Modifier::BOLD)));
                left.push(Span::raw(query.clone()));
                left.push(Span::raw("  "));
                if !query.is_empty() && !found {
                    left.push(Span::styled("no match", Style::default().fg(t.production)));
                } else {
                    left.extend(keys(&[("↑↓", "older/newer"), ("⏎", "keep"), ("esc", "cancel")]));
                }
            }
            Mode::Hints(crate::server::ui::HintsMode { hints, act, .. }) => {
                left.push(badge(if *act { "HINTS: ACT" } else { "HINTS: COPY" }, t.blocked));
                left.push(Span::raw(" "));
                if *act {
                    let mut acts: Vec<&str> = Vec::new();
                    for h in hints {
                        let a = h.m.kind.action();
                        if !acts.contains(&a) {
                            acts.push(a);
                        }
                    }
                    left.push(text(&format!("type a label to {} · ", acts.join(", "))));
                    left.extend(keys(&[("tab", "copy instead"), ("esc", "cancel")]));
                } else {
                    left.push(text("type a label to copy it · "));
                    left.extend(keys(&[("tab", "act on it instead"), ("esc", "cancel")]));
                }
            }
            // Pickers, prompts and dialogs say what they are themselves.
            _ => {}
        }
        let rw: u16 = right.iter().map(|s| s.content.width() as u16).sum();
        // Two columns between what is on the left and what is on the right.
        let lw = area.width.saturating_sub(rw + 3);
        buf.set_line(area.x, area.y, &Line::from(left), lw);
        if rw > 0 && rw + 1 < area.width {
            buf.set_line(area.x + area.width - rw - 1, area.y, &Line::from(right), rw);
        }
    }

    // ---- help and messages -------------------------------------------------------------

    /// The reference, from the keymap in use: commands by menu, direct keys, then the keys of
    /// lists, dialogs and modes, and the mouse.
    pub(super) fn help_lines(&self) -> Vec<Line<'static>> {
        use crate::keymap::Command;
        let km = &self.keymap;
        let p = km.prefix_label();
        let ascii = th().ascii;
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
            Line::raw(""),
        ];
        let seq = |keys: &[crate::input::KeySpec]| -> String {
            let mut parts = vec![p.clone()];
            parts.extend(keys.iter().map(|k| crate::keymap::pretty(k, ascii)));
            parts.join(" ")
        };
        // Sections: commands with one key, then each menu.
        let mut sections: Vec<(String, Vec<(String, String)>)> = vec![("Common".into(), Vec::new())];
        let mut seen: Vec<Command> = Vec::new();
        for (keys, cmd) in &km.leader {
            if seen.contains(cmd) {
                continue;
            }
            seen.push(cmd.clone());
            use crate::keymap::Action::{FocusDown, FocusLeft, FocusRight, FocusUp};
            let what = match cmd {
                Command::Action(crate::keymap::Action::Tab(n)) if *n > 1 => continue,
                Command::Action(FocusDown | FocusRight | FocusUp) if keys.len() == 1 => continue,
                Command::Action(FocusLeft) if keys.len() == 1 => "Focus the pane on that side".to_string(),
                Command::Action(crate::keymap::Action::Tab(_)) => "Go to tab 1-9".to_string(),
                Command::Action(a) => a.describe(),
                Command::Tool(t) => format!("Tool: {t}"),
            };
            let title = match keys.len() {
                1 => "Common".to_string(),
                _ => km.group_title(&keys[..1]).map(str::to_string).unwrap_or_else(|| format!("{} …", keys[0])),
            };
            let shown = match cmd {
                Command::Action(crate::keymap::Action::Tab(_)) => format!("{p} 1…9"),
                Command::Action(FocusLeft) if keys.len() == 1 => {
                    format!("{p} {}", if ascii { "arrows" } else { "←↓↑→" })
                }
                _ => seq(keys),
            };
            match sections.iter_mut().find(|(t, _)| *t == title) {
                Some((_, v)) => v.push((shown, what)),
                None => sections.push((title, vec![(shown, what)])),
            }
        }
        let direct: Vec<(String, String)> = km.direct.iter().map(|(k, a)| (k.to_string(), a.describe())).collect();
        if !direct.is_empty() {
            sections.push(("Without the prefix".into(), direct));
        }
        let fixed =
            |rows: &[(&str, &str)]| rows.iter().map(|(k, w)| (k.to_string(), w.to_string())).collect::<Vec<_>>();
        sections.push((
            "In lists".into(),
            fixed(&[
                ("type", "filter"),
                ("↑ ↓", "move"),
                ("⏎", "choose"),
                ("tab", "more actions on the selected item"),
                ("esc", "close"),
            ]),
        ));
        sections.push((
            "In text fields".into(),
            fixed(&[
                ("← → home end", "move (alt+← → by word)"),
                ("ctrl+w ctrl+u ctrl+k", "delete a word, to the start, to the end"),
                ("⏎ / esc", "accept / cancel"),
            ]),
        ));
        sections.push((
            "Copy mode".into(),
            fixed(&[
                ("arrows hjkl w b", "move"),
                ("v V ctrl+v", "select characters, lines, a block"),
                ("y or ⏎", "copy and leave"),
                ("/ n N", "search, next, previous"),
                ("esc q", "leave"),
            ]),
        ));
        sections.push((
            "Mouse".into(),
            fixed(&[
                ("click", "focus a pane, tab, workspace or agent"),
                ("drag a border", "resize panes"),
                ("drag in a pane", "select and copy (double: word, triple: line)"),
                ("ctrl+click", "open a link; on a title: select for broadcast"),
                ("shift + mouse", "your terminal's own selection"),
            ]),
        ));
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
