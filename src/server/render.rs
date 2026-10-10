//! Composes what one client sees: tab bar, sidebar, panes with frames, status bar, overlays.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Widget};
use unicode_width::UnicodeWidthStr;

use super::ui::{Mode, Picker, Prompt};
use super::{Client, ClientId, Server};
use crate::agent::AgentState;
use crate::ansi::Cursor;
use crate::config::Action;
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
        let sidebar = c.sidebar;
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
            Mode::Picker(p) => Some(draw_picker(p, full, &mut buf)),
            Mode::Prompt(p) => Some(draw_prompt(p, full, &mut buf)),
            Mode::Help { scroll } => {
                self.draw_help(*scroll, full, &mut buf);
                None
            }
            Mode::Confirm { message, .. } => {
                draw_confirm(message, full, &mut buf);
                None
            }
            _ => None,
        };
        let c = &self.clients[&id];
        match &c.mode {
            Mode::Picker(_) | Mode::Prompt(_) => cursor = overlay_cursor.flatten(),
            Mode::Help { .. } | Mode::Confirm { .. } | Mode::Scroll => cursor = None,
            Mode::Search { query, .. } => {
                // After " SEARCH " + " " + "/" in the status bar.
                let x = (c.mode.label().len() as u16 + 4 + query.width() as u16).min(c.cols.saturating_sub(1));
                cursor = Some(Cursor { x, y: c.rows - 1, shape: crate::ansi::CursorShape::Bar, blinking: true });
            }
            _ => {}
        }
        // Overlays (pickers, prompts…) hide the links under them.
        if !matches!(self.clients[&id].mode, Mode::Normal | Mode::Prefix | Mode::Scroll | Mode::Search { .. }) {
            links.clear();
        }
        (buf, cursor, links)
    }

    // ---- tab bar ----------------------------------------------------------------------

    /// Clickable items of the tab bar with their column ranges and labels.
    pub fn tab_bar_items(&self, c: &Client) -> Vec<(u16, u16, BarItem, Vec<Span<'static>>)> {
        let mut out = Vec::new();
        let Some(wi) = self.ws_index(c.workspace) else { return out };
        let ws = &self.workspaces[wi];
        let mut x = 0u16;
        let badge = format!(" {} ", ws.name);
        let w = badge.width() as u16;
        out.push((
            x,
            w,
            BarItem::Workspace,
            vec![Span::styled(badge, Style::default().fg(Color::Black).bg(th().accent).add_modifier(Modifier::BOLD))],
        ));
        x += w + 1;
        for (i, tab) in ws.tabs.iter().enumerate() {
            let active = i == ws.active;
            let mut spans = Vec::new();
            let base = if active {
                Style::default().fg(Color::Black).bg(Color::Gray).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            spans.push(Span::styled(format!(" {}:{}", i + 1, tab.name), base));
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
            out.push((x, w, BarItem::Tab(i), spans));
            x += w + 1;
        }
        out.push((x, 3, BarItem::NewTab, vec![Span::styled(" + ", Style::default().fg(th().dim))]));
        out
    }

    pub fn tab_bar_hit(&self, id: ClientId, col: u16) -> Option<BarItem> {
        let c = self.clients.get(&id)?;
        self.tab_bar_items(c).into_iter().find(|(x, w, ..)| col >= *x && col < x + w).map(|(_, _, item, _)| item)
    }

    fn draw_tab_bar(&self, id: ClientId, buf: &mut Buffer) {
        let c = &self.clients[&id];
        let width = c.cols;
        // Right side: agents across all workspaces, then the session name.
        let (mut blocked, mut working, mut done) = (0, 0, 0);
        for p in self.panes.values() {
            match p.agent.state {
                AgentState::Blocked => blocked += 1,
                AgentState::Working => working += 1,
                AgentState::Done => done += 1,
                _ => {}
            }
        }
        let mut right: Vec<Span> = Vec::new();
        for (n, s) in [(blocked, AgentState::Blocked), (done, AgentState::Done), (working, AgentState::Working)] {
            if n > 0 {
                right.push(Span::styled(
                    format!("{}{n} ", th().icon(s)),
                    Style::default().fg(th().state_color(s)).add_modifier(Modifier::BOLD),
                ));
            }
        }
        right.push(Span::styled(format!(" {} ", self.session), Style::default().fg(th().dim)));
        let rw: u16 = right.iter().map(|s| s.content.width() as u16).sum();
        let limit = width.saturating_sub(rw + 1);
        for (x, w, _, spans) in self.tab_bar_items(c) {
            if x + w > limit {
                buf.set_string(x.min(limit), 0, "…", Style::default().fg(th().dim));
                break;
            }
            buf.set_line(x, 0, &Line::from(spans), w);
        }
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
            spans.push(Span::styled(format!(" {}", panes.len()), Style::default().fg(th().dim)));
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
        // Hints at the bottom.
        let hints = [(Action::WorkspacePicker, "workspaces"), (Action::NextAttention, "next agent")];
        let mut y = area.y + area.height;
        for (a, label) in hints.iter().rev() {
            if let Some(k) = self.keymap.hint(*a) {
                y -= 1;
                if y <= area.y {
                    break;
                }
                let line = Line::from(vec![
                    Span::styled(format!(" {k}"), Style::default().fg(Color::Gray)),
                    Span::styled(format!(" {label}"), Style::default().fg(th().dim)),
                ]);
                buf.set_line(area.x, y, &line, inner_w);
            }
        }
        for y in area.y..area.y + area.height {
            if let Some(cell) = buf.cell_mut((area.x + area.width - 1, y)) {
                cell.set_symbol(th().vline()).set_fg(th().dim);
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
                self.draw_frame(pid, outer, focused, synced.contains(&pid), floating, marked.contains(&pid), buf);
            }
            let Some(pane) = self.panes.get_mut(&pid) else { continue };
            let content = Rect::new(inner.x, inner.y, inner.width.min(pane.cols), inner.height.min(pane.rows));
            if floating {
                // A floating pane hides the links of what is under it.
                links.retain(|(x, y), _| !outer.contains((*x, *y).into()));
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
            if focused && matches!(self.clients[&id].mode, Mode::Normal | Mode::Prefix) {
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
        let color = if sync {
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
        if floating {
            title.insert(0, Span::styled(" ◇", Style::default().fg(color)));
        }
        if marked {
            title.insert(0, Span::styled(" ◆", Style::default().fg(th().sync).add_modifier(Modifier::BOLD)));
        }
        let mut right = Vec::new();
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

    fn draw_status(&self, id: ClientId, area: Rect, buf: &mut Buffer) {
        let c = &self.clients[&id];
        let (label, color) = match &c.mode {
            Mode::Normal => ("NORMAL", th().accent),
            Mode::Prefix => ("PREFIX", Color::Yellow),
            Mode::Scroll | Mode::Search { .. } => (c.mode.label(), Color::LightBlue),
            m => (m.label(), Color::Magenta),
        };
        let mut spans = vec![
            Span::styled(
                format!(" {label} "),
                Style::default().fg(Color::Black).bg(color).add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
        ];
        if let Some((msg, _)) = &c.toast {
            spans.push(Span::styled(msg.clone(), Style::default().fg(Color::Yellow)));
        } else {
            let hint = |a: Action, label: &str| -> Vec<Span<'static>> {
                match self.keymap.hint(a) {
                    Some(k) => vec![
                        Span::styled(k, Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD)),
                        Span::styled(format!(" {label}  "), Style::default().fg(th().dim)),
                    ],
                    None => Vec::new(),
                }
            };
            let p = self.keymap.prefix_label();
            let items: Vec<(Action, &str)> = match &c.mode {
                Mode::Prefix => vec![
                    (Action::SplitRight, "split →"),
                    (Action::SplitDown, "split ↓"),
                    (Action::NewTab, "tab"),
                    (Action::ToggleZoom, "zoom"),
                    (Action::ClosePane, "close"),
                    (Action::SshPicker, "ssh"),
                    (Action::ToggleSync, "sync"),
                    (Action::ScrollMode, "scroll"),
                    (Action::Detach, "detach"),
                    (Action::Help, "help"),
                ],
                Mode::Search { query, found } => {
                    spans.push(Span::styled("/", Style::default().fg(Color::LightBlue).add_modifier(Modifier::BOLD)));
                    spans.push(Span::raw(query.clone()));
                    let status = match (query.is_empty(), *found) {
                        (true, _) => "  type to search · ↑↓ older/newer · enter keep · esc cancel",
                        (false, true) => "  ↑↓ older/newer · enter keep · esc cancel",
                        (false, false) => "  no match",
                    };
                    let color = if !query.is_empty() && !*found { Color::LightRed } else { th().dim };
                    spans.push(Span::styled(status, Style::default().fg(color)));
                    vec![]
                }
                Mode::Scroll => {
                    spans.push(Span::styled(
                        "hjkl w b 0 $ g G move · v V select · y copy · / search · n N next · q exit",
                        Style::default().fg(th().dim),
                    ));
                    vec![]
                }
                Mode::Normal => {
                    spans.push(Span::styled(p.clone(), Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD)));
                    spans.push(Span::styled(" prefix  ", Style::default().fg(th().dim)));
                    vec![
                        (Action::NewPane, "new pane"),
                        (Action::FocusOrTabLeft, "focus"),
                        (Action::WorkspacePicker, "workspaces"),
                        (Action::NextAttention, "next agent"),
                        (Action::ToggleFloating, "floating"),
                    ]
                }
                _ => vec![],
            };
            for (a, l) in items {
                spans.extend(hint(a, l));
            }
            if matches!(c.mode, Mode::Normal) {
                spans.push(Span::styled(
                    format!("{p} ?"),
                    Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(" help", Style::default().fg(th().dim)));
            }
        }
        buf.set_line(area.x, area.y, &Line::from(spans), area.width);
    }

    // ---- help -------------------------------------------------------------------------

    fn draw_help(&self, scroll: u16, full: Rect, buf: &mut Buffer) {
        let mut lines: Vec<Line> = Vec::new();
        let key_style = Style::default().fg(th().accent).add_modifier(Modifier::BOLD);
        lines.push(Line::from(Span::styled("Without prefix", Style::default().add_modifier(Modifier::BOLD))));
        let group = |entries: &[(crate::input::KeySpec, Action)], prefix: Option<&str>, lines: &mut Vec<Line>| {
            let mut by_action: Vec<(Action, Vec<String>)> = Vec::new();
            for (k, a) in entries {
                // "go to tab N" bindings collapse into one line.
                let a = if let Action::Tab(_) = a { Action::Tab(0) } else { *a };
                match by_action.iter_mut().find(|(x, _)| *x == a) {
                    Some((_, v)) => v.push(k.to_string()),
                    None => by_action.push((a, vec![k.to_string()])),
                }
            }
            by_action.sort_by_key(|(a, _)| *a);
            for (a, keys) in by_action {
                let (keys, what) = match a {
                    Action::Tab(_) if keys.len() > 2 => {
                        (format!("{}…{}", keys[0], keys[keys.len() - 1]), "go to tab N".to_string())
                    }
                    Action::Tab(_) => (keys.join(" "), "go to tab N".to_string()),
                    _ => (keys.join(" "), a.describe()),
                };
                let keys = match prefix {
                    Some(p) => format!("{p} {keys}"),
                    None => keys,
                };
                lines.push(Line::from(vec![Span::styled(format!("  {keys:<22}"), key_style), Span::raw(what)]));
            }
        };
        group(&self.keymap.direct, None, &mut lines);
        lines.push(Line::raw(""));
        let p = self.keymap.prefix_label();
        lines.push(Line::from(Span::styled(
            format!("After the prefix ({p})"),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        group(&self.keymap.prefixed, Some(&p), &mut lines);
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled("Mouse", Style::default().add_modifier(Modifier::BOLD))));
        for (k, d) in [
            ("click", "focus a pane, tab, workspace or agent"),
            ("drag a border", "resize panes"),
            ("drag in a pane", "select and copy (double: word, triple: line)"),
            ("wheel", "scroll back"),
            ("shift + mouse", "use your terminal's own selection"),
        ] {
            lines.push(Line::from(vec![Span::styled(format!("  {k:<22}"), key_style), Span::raw(d)]));
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            "Exited command panes: enter restarts, x closes.  Config: ~/.config/kraal/config.toml",
            Style::default().fg(th().dim),
        )));

        let w = (full.width.saturating_sub(4)).min(76);
        let h = (full.height.saturating_sub(2)).min(lines.len() as u16 + 2);
        let area = centered(full, w, h);
        Clear.render(area, buf);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_set(th().overlay_border())
            .border_style(Style::default().fg(th().accent))
            .title(" kraal help · ↑↓ scroll · any key closes ");
        let inner = block.inner(area);
        block.render(area, buf);
        let max_scroll = (lines.len() as u16).saturating_sub(inner.height);
        let scroll = scroll.min(max_scroll) as usize;
        for (i, line) in lines.iter().skip(scroll).take(inner.height as usize).enumerate() {
            buf.set_line(inner.x + 1, inner.y + i as u16, line, inner.width.saturating_sub(1));
        }
    }
}

fn centered(full: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(full.width);
    let h = h.min(full.height);
    Rect::new(full.x + (full.width - w) / 2, full.y + (full.height - h) / 2, w, h)
}

fn draw_picker(p: &Picker, full: Rect, buf: &mut Buffer) -> Option<Cursor> {
    let w = full.width.saturating_sub(6).min(90);
    let list_h = (p.filtered.len() as u16).clamp(1, full.height.saturating_sub(8).max(1));
    let area = centered(full, w, list_h + 4);
    Clear.render(area, buf);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(th().overlay_border())
        .border_style(Style::default().fg(th().accent))
        .title(Span::styled(format!(" {} ", p.title), Style::default().add_modifier(Modifier::BOLD)));
    let inner = block.inner(area);
    block.render(area, buf);
    if inner.height < 2 {
        return None;
    }
    let prompt =
        Line::from(vec![Span::styled(th().prompt(), Style::default().fg(th().accent)), Span::raw(p.query.clone())]);
    buf.set_line(inner.x + 1, inner.y, &prompt, inner.width.saturating_sub(1));
    let sep_y = inner.y + 1;
    for x in inner.x..inner.x + inner.width {
        buf[(x, sep_y)].set_symbol(th().hline()).set_fg(th().dim);
    }
    let rows = inner.height.saturating_sub(2) as usize;
    let start = p.selected.saturating_sub(rows.saturating_sub(1));
    for (row, &idx) in p.filtered.iter().skip(start).take(rows).enumerate() {
        let item = &p.items[idx];
        let selected = start + row == p.selected;
        let y = inner.y + 2 + row as u16;
        let mut spans = vec![Span::raw(if selected { "▌ " } else { "  " })];
        if let Some(s) = item.state {
            spans.push(Span::styled(format!("{} ", th().icon(s)), Style::default().fg(th().state_color(s))));
        }
        let label_style =
            if item.dim { Style::default().fg(Color::Gray) } else { Style::default().add_modifier(Modifier::BOLD) };
        spans.push(Span::styled(item.label.clone(), label_style));
        if !item.detail.is_empty() {
            spans.push(Span::styled(format!("  {}", item.detail), Style::default().fg(th().dim)));
        }
        let style = if selected { Style::default().bg(Color::Indexed(236)) } else { Style::default() };
        if selected {
            for x in inner.x..inner.x + inner.width {
                buf[(x, y)].set_style(style);
            }
        }
        let line = Line::from(spans).patch_style(style);
        buf.set_line(inner.x, y, &line, inner.width);
    }
    if p.filtered.is_empty() {
        buf.set_string(inner.x + 2, inner.y + 2, "no match", Style::default().fg(th().dim));
    }
    let x = inner.x + 3 + p.query.width() as u16;
    (x < inner.x + inner.width).then_some(Cursor {
        x,
        y: inner.y,
        shape: crate::ansi::CursorShape::Bar,
        blinking: true,
    })
}

fn draw_prompt(p: &Prompt, full: Rect, buf: &mut Buffer) -> Option<Cursor> {
    let w = full.width.saturating_sub(6).min(70);
    let area = centered(full, w, 3);
    Clear.render(area, buf);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(th().overlay_border())
        .border_style(Style::default().fg(th().accent))
        .title(Span::styled(
            format!(" {} · enter ok · esc cancel ", p.title),
            Style::default().add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    block.render(area, buf);
    let visible: String = {
        let max = inner.width.saturating_sub(2) as usize;
        let w = p.value.width();
        if w <= max {
            p.value.clone()
        } else {
            let mut s = String::new();
            let mut acc = 0;
            for ch in p.value.chars().rev() {
                acc += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                if acc > max {
                    break;
                }
                s.insert(0, ch);
            }
            s
        }
    };
    buf.set_string(inner.x + 1, inner.y, &visible, Style::default());
    Some(Cursor {
        x: inner.x + 1 + visible.width() as u16,
        y: inner.y,
        shape: crate::ansi::CursorShape::Bar,
        blinking: true,
    })
}

fn draw_confirm(message: &str, full: Rect, buf: &mut Buffer) {
    let w = (message.width() as u16 + 6).clamp(30, full.width.saturating_sub(4).max(10));
    let area = centered(full, w, 4);
    Clear.render(area, buf);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(th().overlay_border())
        .border_style(Style::default().fg(Color::Yellow))
        .title(" confirm ");
    let inner = block.inner(area);
    block.render(area, buf);
    buf.set_stringn(inner.x + 1, inner.y, message, inner.width.saturating_sub(2) as usize, Style::default());
    buf.set_line(
        inner.x + 1,
        inner.y + 1,
        &Line::from(vec![
            Span::styled("y", Style::default().fg(th().accent).add_modifier(Modifier::BOLD)),
            Span::raw(" yes   "),
            Span::styled("any other key", Style::default().fg(Color::Gray).add_modifier(Modifier::BOLD)),
            Span::raw(" no"),
        ]),
        inner.width.saturating_sub(2),
    );
}
