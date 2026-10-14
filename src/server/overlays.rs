//! What kraal draws over the panes: the leader menu, pickers, prompts, confirmations, help and
//! messages. They share one look: a framed box with a short title, one column of padding, and a
//! footer of keys (the key in the accent colour, what it does dimmed). Each goes in the screen it
//! is given: the client's, less the bottom bar's row ([`super::Server::screen_above_bar`]).

use ratatui::buffer::Buffer;
use ratatui::layout::{Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Widget};
use unicode_width::UnicodeWidthStr;

use super::ui::{Picker, Prompt, Question};
use crate::ansi::{Cursor, CursorShape};
use crate::input::KeySpec;
use crate::keymap::{Action, Command, MenuEntry};
use crate::text::{columns, fold, truncate, wrap};
use crate::theme::current as th;

fn key_style() -> Style {
    Style::default().fg(th().accent).add_modifier(Modifier::BOLD)
}

fn dim() -> Style {
    Style::default().fg(th().dim)
}

/// A box centred in `screen`.
pub(crate) fn centered(screen: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(screen.width);
    let h = h.min(screen.height);
    Rect::new(screen.x + (screen.width - w) / 2, screen.y + (screen.height - h) / 2, w, h)
}

/// Clear `area` and frame it with `title`; returns the inside.
pub(crate) fn frame(area: Rect, title: &str, color: Color, buf: &mut Buffer) -> Rect {
    Clear.render(area, buf);
    let title = truncate(title, area.width.saturating_sub(4) as usize);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(th().overlay_border())
        .border_style(Style::default().fg(color))
        .title(Span::styled(format!(" {title} "), Style::default().add_modifier(Modifier::BOLD)));
    let inner = block.inner(area);
    block.render(area, buf);
    inner
}

/// `key what · key what`, dropping what does not fit from the end; `right` (a count, a hint)
/// goes against the right edge when there is room.
/// A key as the theme can show it: the ASCII theme spells symbols out.
pub(super) fn shown_key(k: &str) -> String {
    if !th().ascii {
        return k.to_string();
    }
    k.replace('⏎', "enter")
        .replace("↑↓", "up/down")
        .replace("←→", "left/right")
        .replace("←↓↑→", "arrows")
        .replace('⌫', "bksp")
}

pub(crate) fn key_line(keys: &[(&str, &str)], right: &str, width: u16) -> Line<'static> {
    let width = width as usize;
    let owned: Vec<(String, &str)> = keys.iter().map(|(k, w)| (shown_key(k), *w)).collect();
    let keys: Vec<(&str, &str)> = owned.iter().map(|(k, w)| (k.as_str(), *w)).collect();
    let keys = keys.as_slice();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    let room_right = if right.is_empty() { 0 } else { columns(right) + 2 };
    let cost = |(k, what): &(&str, &str)| 3 + columns(k) + 1 + columns(what);
    // The first key (what Enter does) and the last (how to leave) come first, then the right
    // text (a count), then the other keys, from the start, while they fit.
    let mut shown: Vec<usize> = Vec::new();
    let mut budget = width + 3;
    let ends: Vec<usize> = if keys.len() > 1 { vec![0, keys.len() - 1] } else { (0..keys.len()).collect() };
    for i in ends {
        if cost(&keys[i]) <= budget {
            budget -= cost(&keys[i]);
            shown.push(i);
        }
    }
    let right_fits = room_right <= budget;
    if right_fits {
        budget -= room_right;
    }
    let middle = keys.len().saturating_sub(1);
    for (i, key) in keys.iter().enumerate().take(middle).skip(1) {
        if cost(key) > budget {
            break;
        }
        budget -= cost(key);
        shown.push(i);
    }
    shown.sort_unstable();
    for (n, i) in shown.into_iter().enumerate() {
        let (k, what) = keys[i];
        if n > 0 {
            spans.push(Span::styled(" · ", dim()));
            used += 3;
        }
        spans.push(Span::styled(k.to_string(), key_style()));
        spans.push(Span::styled(format!(" {what}"), dim()));
        used += columns(k) + 1 + columns(what);
    }
    if !right.is_empty() && right_fits && used + room_right <= width {
        spans.push(Span::raw(" ".repeat(width - used - columns(right))));
        spans.push(Span::styled(right.to_string(), dim()));
    }
    Line::from(spans)
}

// ---- leader menu ----------------------------------------------------------------------------

/// One entry of the leader menu, as shown.
pub(super) struct MenuItem {
    /// The key; the arrows that move the focus and the digits that pick a tab share one entry.
    pub(super) key: String,
    pub(super) label: String,
    /// It opens a further menu.
    pub(super) group: bool,
    /// The key a click on the entry presses (none for an entry standing for several keys).
    pub(super) press: Option<KeySpec>,
    /// Main actions (0), other keys (1) and menus (2) go in columns of their own when they fit.
    pub(super) section: u8,
}

/// Commands the first menu's footer names instead of a line of their own; in a menu further in
/// (quit in the Workspaces menu), they are lines like the others.
fn in_footer(e: &MenuEntry, root: bool) -> bool {
    root && matches!(
        e.command,
        Some(Command::Action(Action::Help | Action::Detach | Action::Quit | Action::SendPrefix))
    )
}

/// The keys of the menu's footer, besides how to leave: help, detach and quit in the first menu
/// (`root`).
pub(super) fn menu_footer(entries: &[MenuEntry], root: bool) -> Vec<(String, String)> {
    let ascii = th().ascii;
    entries
        .iter()
        .filter(|e| in_footer(e, root) && !matches!(e.command, Some(Command::Action(Action::SendPrefix))))
        .map(|e| (crate::keymap::pretty(&e.key, ascii), e.label.to_lowercase()))
        .collect()
}

/// The entries of a menu, in the keymap's order; `root` for the first one.
pub(super) fn menu_items(entries: &[MenuEntry], root: bool) -> Vec<MenuItem> {
    let ascii = th().ascii;
    let focus = |e: &MenuEntry| {
        matches!(
            e.command,
            Some(Command::Action(Action::FocusLeft | Action::FocusRight | Action::FocusUp | Action::FocusDown))
        )
    };
    let tab = |e: &MenuEntry| matches!(e.command, Some(Command::Action(Action::Tab(_))));
    let mut items: Vec<MenuItem> = Vec::new();
    let (mut focus_done, mut tab_done) = (false, false);
    for e in entries.iter().filter(|e| !in_footer(e, root)) {
        if focus(e) {
            if !std::mem::replace(&mut focus_done, true) {
                // In the order of hjkl.
                let mut found: Vec<&MenuEntry> = entries.iter().filter(|x| focus(x)).collect();
                found.sort_by_key(|x| match x.command {
                    Some(Command::Action(Action::FocusLeft)) => 0,
                    Some(Command::Action(Action::FocusDown)) => 1,
                    Some(Command::Action(Action::FocusUp)) => 2,
                    _ => 3,
                });
                // `←↓↑→` (`arrows` in ASCII) or `hjkl` when each is one character.
                let keys: Vec<String> = found.iter().map(|x| crate::keymap::pretty(&x.key, false)).collect();
                let joined = keys.concat();
                let key = if keys.iter().all(|k| columns(k) == 1) && (!ascii || joined.is_ascii() || joined == "←↓↑→")
                {
                    shown_key(&joined)
                } else {
                    found.iter().map(|x| crate::keymap::pretty(&x.key, ascii)).collect::<Vec<_>>().join("/")
                };
                items.push(MenuItem { key, label: "Focus".into(), group: false, press: None, section: 1 });
            }
            continue;
        }
        if tab(e) {
            if !std::mem::replace(&mut tab_done, true) {
                let n = entries.iter().filter(|x| tab(x)).count();
                let key = format!("1-{n}");
                items.push(MenuItem { key, label: "Go to tab".into(), group: false, press: None, section: 1 });
            }
            continue;
        }
        let key = crate::keymap::pretty(&e.key, ascii);
        let section = if e.group {
            2
        } else if e.main {
            0
        } else {
            1
        };
        items.push(MenuItem { key, label: e.label.clone(), group: e.group, press: Some(e.key.clone()), section });
    }
    items
}

/// Where the leader menu goes: a box against the right edge, above the status line. Entries go
/// down its columns, each column as wide as its entries (like `ls`); its keys are at the bottom.
pub(super) struct MenuLayout {
    pub(super) area: Rect,
    pub(super) cells: Vec<MenuCell>,
}

pub(super) struct MenuCell {
    /// Index in the items.
    pub(super) item: usize,
    /// Where it is drawn and clicked.
    pub(super) area: Rect,
    /// Width of its column's keys (they are right-aligned).
    key_w: usize,
}

impl MenuLayout {
    /// The entry under a click, if any.
    pub(super) fn item_at(&self, col: u16, row: u16) -> Option<usize> {
        self.cells.iter().find(|c| c.area.contains(Position::new(col, row))).map(|c| c.item)
    }
}

/// Space between two columns of the menu.
const MENU_GAP: usize = 3;
/// Shortest a label gets when the screen is too narrow for the menu.
const MENU_LABEL_MIN: usize = 6;

/// Widths of the key and of the whole of each column (indices into `items`), labels shortened to
/// `cap` (menu names stay whole: they are short).
fn column_widths(items: &[MenuItem], cols: &[Vec<usize>], cap: usize) -> Vec<(usize, usize)> {
    cols.iter()
        .map(|col| {
            let key_w = col.iter().map(|&i| columns(&items[i].key)).max().unwrap_or(0);
            let width = col
                .iter()
                .map(|&i| {
                    let it = &items[i];
                    let label = if it.group { columns(&it.label) } else { columns(&it.label).min(cap) };
                    key_w + 2 + label + usize::from(it.group)
                })
                .max()
                .unwrap_or(0);
            (key_w, width)
        })
        .collect()
}

/// Entries going down `lines` rows before the next column, like `ls`.
fn flow(n: usize, lines: usize) -> Vec<Vec<usize>> {
    (0..n).collect::<Vec<_>>().chunks(lines.max(1)).map(<[usize]>::to_vec).collect()
}

/// One column per section (main actions, other keys, menus), when each fits the height.
fn by_section(items: &[MenuItem], max_lines: usize) -> Option<Vec<Vec<usize>>> {
    let mut cols: Vec<Vec<usize>> = Vec::new();
    let mut last = None;
    for (i, it) in items.iter().enumerate() {
        if last != Some(it.section) {
            cols.push(Vec::new());
            last = Some(it.section);
        }
        cols.last_mut()?.push(i);
    }
    (cols.len() > 1 && cols.iter().all(|c| c.len() <= max_lines)).then_some(cols)
}

pub(super) fn menu_layout(items: &[MenuItem], screen: Rect) -> MenuLayout {
    let mut layout = MenuLayout { area: Rect::default(), cells: Vec::new() };
    // One column between the box and the right edge, down to the bottom of the screen (above the
    // bottom bar); the top bar stays free.
    let max_w = screen.width.saturating_sub(2) as usize;
    let max_lines = (screen.height as usize).saturating_sub(6).max(1);
    if items.is_empty() || max_w < 12 || screen.height < 6 {
        return layout;
    }
    // Frame and padding: two columns on each side.
    let total = |cols: &[Vec<usize>], cap: usize| {
        let w = column_widths(items, cols, cap);
        w.iter().map(|c| c.1).sum::<usize>() + MENU_GAP * (w.len() - 1) + 4
    };
    // Labels get shorter only when even that is too wide.
    let longest = items.iter().map(|i| columns(&i.label)).max().unwrap_or(0);
    let fit =
        |cols: &[Vec<usize>]| (MENU_LABEL_MIN..=longest.max(MENU_LABEL_MIN)).rev().find(|&c| total(cols, c) <= max_w);
    // The sections side by side when they fit; else the entries flow down up to three columns
    // while the width allows, more when the height needs them.
    let (cols, cap) = match by_section(items, max_lines).and_then(|cols| fit(&cols).map(|cap| (cols, cap))) {
        Some(found) => found,
        None => {
            let fewest = items.len().div_ceil(max_lines);
            let most = fewest.max(3).min(items.len());
            let lines = (fewest..=most)
                .rev()
                .map(|c| items.len().div_ceil(c))
                .find(|&l| total(&flow(items.len(), l), usize::MAX) <= max_w)
                .unwrap_or(items.len().div_ceil(fewest));
            let cols = flow(items.len(), lines);
            let cap = fit(&cols).unwrap_or(0);
            (cols, cap)
        }
    };
    let lines = cols.iter().map(Vec::len).max().unwrap_or(1);
    let widths = column_widths(items, &cols, cap);
    let w = total(&cols, cap).min(max_w) as u16;
    let h = (lines as u16 + 4).min(screen.height - 1);
    layout.area = Rect::new(screen.x + screen.width - w - 1, screen.y + screen.height - h, w, h);
    let right = layout.area.right() - 2;
    let mut x = layout.area.x + 2;
    for (c, (key_w, width)) in widths.iter().enumerate() {
        for (line, &n) in cols[c].iter().enumerate() {
            // The bottom border, a blank line and the keys of the box come after the entries.
            if line as u16 + 4 >= h || x >= right {
                continue;
            }
            let area = Rect::new(x, layout.area.y + 1 + line as u16, (*width as u16).min(right - x), 1);
            layout.cells.push(MenuCell { item: n, area, key_w: *key_w });
        }
        x = x.saturating_add((width + MENU_GAP) as u16);
    }
    layout
}

/// The leader menu: `title` says where it is; its last line, the `footer` keys and how to leave,
/// or why a key did nothing (`miss`).
pub(super) fn draw_menu(
    items: &[MenuItem],
    layout: &MenuLayout,
    (title, footer): (&str, &[(String, String)]),
    back: bool,
    miss: Option<&str>,
    buf: &mut Buffer,
) {
    if layout.area.width < 12 || layout.area.height < 4 {
        return;
    }
    let t = th();
    let inner = frame(layout.area, title, t.accent, buf);
    for cell in &layout.cells {
        let item = &items[cell.item];
        let label_style = if item.group {
            Style::default().fg(t.text).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(t.text)
        };
        let width = cell.area.width as usize;
        // A key wider than a squeezed column is shortened rather than pushed out of it.
        let key_w = cell.key_w.min(width.saturating_sub(3));
        let room = width.saturating_sub(key_w + 2 + usize::from(item.group));
        let mut spans = vec![
            Span::raw(" ".repeat(key_w.saturating_sub(columns(&item.key)))),
            Span::styled(truncate(&item.key, key_w), key_style()),
            Span::raw("  "),
            Span::styled(truncate(&item.label, room), label_style),
        ];
        if item.group {
            spans.push(Span::styled(if t.ascii { "+" } else { "…" }, dim()));
        }
        buf.set_line(cell.area.x, cell.area.y, &Line::from(spans), cell.area.width);
    }
    let footer_y = inner.y + inner.height - 1;
    let mut keys: Vec<(&str, &str)> = footer.iter().map(|(k, w)| (k.as_str(), w.as_str())).collect();
    if back {
        keys.push(("⌫", "back"));
    }
    keys.push(("esc", "close"));
    let line = match miss {
        Some(m) => Line::from(Span::styled(m.to_string(), Style::default().fg(t.blocked))),
        None => key_line(&keys, "", inner.width.saturating_sub(2)),
    };
    buf.set_line(inner.x + 1, footer_y, &line, inner.width.saturating_sub(2));
}

// ---- pickers --------------------------------------------------------------------------------

/// Where a picker's parts are; the same for drawing and for mouse clicks.
pub(super) struct PickerLayout {
    pub(super) area: Rect,
    /// The rows of the list.
    pub(super) list: Rect,
    /// Index (in the filtered list) of the first row shown.
    pub(super) start: usize,
    pub(super) row_height: u16,
    pinned: Option<(Rect, usize)>,
}

impl PickerLayout {
    /// The item under a click, if any.
    pub(super) fn row_at(&self, col: u16, row: u16) -> Option<usize> {
        if let Some((area, index)) = self.pinned
            && area.contains(Position::new(col, row))
        {
            return Some(index);
        }
        self.list
            .contains(Position::new(col, row))
            .then(|| self.start + ((row - self.list.y) / self.row_height) as usize)
            .filter(|&index| self.pinned.is_none_or(|(_, pinned)| index != pinned))
    }
}

/// A picker's geometry. Its height follows the number of items, not the matches, so it does not
/// jump while filtering.
pub(super) fn picker_layout(p: &Picker, screen: Rect) -> PickerLayout {
    let w = screen.width.saturating_sub(4).min(92);
    let row_height = if p.two_line && p.actions.is_none() { 2 } else { 1 };
    let rows = (p
        .items
        .len()
        .max(p.min_items)
        .max(p.action_keys().len())
        .max(if p.kind == super::ui::PickerKind::Workspace { 2 } else { 1 }) as u16
        * row_height)
        .clamp(1, screen.height.saturating_sub(9).max(1));
    let rows = rows / row_height * row_height;
    let area = centered(screen, w, rows + 5);
    let inner = Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(2), area.height.saturating_sub(2));
    let mut list = Rect::new(inner.x, inner.y + 2, inner.width, inner.height.saturating_sub(3));
    let pinned = if p.kind == super::ui::PickerKind::Workspace && p.actions.is_none() {
        p.pinned.and_then(|pinned| p.filtered.iter().position(|&i| i == pinned)).map(|index| {
            list.height = list.height.saturating_sub(row_height);
            (Rect::new(list.x, list.bottom(), list.width, row_height), index)
        })
    } else {
        None
    };
    let shown = (list.height / row_height).max(1) as usize;
    let selected = if pinned.is_some_and(|(_, i)| i == p.selected) { p.selected.saturating_sub(1) } else { p.selected };
    let start = p.drag.as_ref().map_or_else(|| selected.saturating_sub(shown.saturating_sub(1)), |d| d.list_start);
    PickerLayout { area, list, start, row_height, pinned }
}

pub(super) fn draw_picker(p: &Picker, screen: Rect, buf: &mut Buffer) -> Option<Cursor> {
    let t = th();
    let layout = picker_layout(p, screen);
    let inner = frame(layout.area, &p.title, t.accent, buf);
    if inner.height < 3 || inner.width < 6 {
        return None;
    }
    // The search line, or an instruction for fixed choices.
    let review = p.kind == super::ui::PickerKind::AccountReview;
    let conflict = p.kind == super::ui::PickerKind::EditConflict;
    let workspace = matches!(p.kind, super::ui::PickerKind::Workspace | super::ui::PickerKind::DeletedWorkspaces);
    let prompt = if review {
        Line::from("Which account is this?")
    } else if conflict {
        Line::from("Choose which version to keep.")
    } else {
        Line::from(vec![Span::styled("Search  ", Style::default().fg(t.accent)), Span::raw(p.query.clone())])
    };
    buf.set_line(inner.x + 1, inner.y, &prompt, inner.width.saturating_sub(2));
    for x in inner.x..inner.x + inner.width {
        buf[(x, inner.y + 1)].set_symbol(t.hline()).set_fg(t.dim);
    }
    if review
        && let Some(super::ui::PickAction::UseExisting { existing, .. }) = p.items.first().map(|i| &i.action)
        && let Some(identity) = &existing.public_identity
    {
        buf.set_stringn(inner.x + 1, inner.y + 1, identity, inner.width.saturating_sub(2) as usize, dim());
    }
    let list = layout.list;
    let row_style =
        |selected: bool| if selected { Style::default().fg(Color::White).bg(t.selection) } else { Style::default() };
    let pointer = |selected: bool| if selected { format!("{} ", t.pointer()) } else { "  ".into() };
    if let Some(at) = p.actions {
        // What can be done with the selected item.
        let keys = p.action_keys();
        let key_w = keys.iter().map(|(k, _)| columns(k)).max().unwrap_or(5);
        let start = at.saturating_sub((list.height as usize).saturating_sub(1));
        for (row, (i, (k, what))) in keys.iter().enumerate().skip(start).take(list.height as usize).enumerate() {
            let selected = i == at;
            let y = list.y + row as u16;
            let line = Line::from(vec![
                Span::raw(pointer(selected)),
                Span::styled(
                    format!("{:<key_w$}", k.split('+').map(upper_first).collect::<Vec<_>>().join("+")),
                    key_style(),
                ),
                Span::raw("  "),
                Span::raw(what.to_string()),
            ])
            .patch_style(row_style(selected));
            fill(buf, Rect::new(list.x, y, list.width, 1), row_style(selected));
            buf.set_line(list.x, y, &line, list.width);
        }
        let item = p.current().map(|i| i.label.as_str()).unwrap_or("");
        let verb = keys.get(at).map_or("Choose", |(_, label)| *label);
        let footer = key_line(&[("Enter", verb), ("Esc", "Back")], item, inner.width.saturating_sub(2));
        buf.set_line(inner.x + 1, inner.y + inner.height - 1, &footer, inner.width.saturating_sub(2));
        return None;
    }
    let visible = p
        .filtered
        .iter()
        .enumerate()
        .skip(layout.start)
        .take((list.height / layout.row_height) as usize)
        .filter(|(i, _)| layout.pinned.is_none_or(|(_, pinned)| *i != pinned))
        .enumerate()
        .map(|(row, (i, &idx))| (i, idx, list.y + row as u16 * layout.row_height));
    let pinned = layout.pinned.map(|(area, i)| (i, p.filtered[i], area.y));
    for (i, idx, y) in visible.chain(pinned) {
        let item = &p.items[idx];
        let selected =
            p.drag.as_ref().filter(|d| d.moved && p.query.is_empty()).map_or(i == p.selected, |d| d.hover == Some(i));
        let mut spans = vec![Span::raw(pointer(selected))];
        if let Some(s) = item.state {
            spans.push(Span::styled(format!("{} ", t.icon(s)), Style::default().fg(t.state_color(s))));
        }
        let label_style =
            if item.dim { Style::default().fg(t.dim) } else { Style::default().add_modifier(Modifier::BOLD) };
        spans.push(Span::styled(item.label.clone(), label_style));
        if !item.detail.is_empty() && layout.row_height == 1 {
            if p.kind == super::ui::PickerKind::Commands {
                // Keys against the right edge, in a column.
                let used: usize = spans.iter().map(|s| columns(&s.content)).sum();
                let room = (list.width as usize).saturating_sub(used + 2);
                let keys = truncate(&item.detail, room);
                spans.push(Span::raw(" ".repeat(room.saturating_sub(columns(&keys)) + 1)));
                spans.push(Span::styled(keys, key_style()));
            } else {
                spans.push(Span::styled(format!("  {}", item.detail), dim()));
            }
        }
        fill(buf, Rect::new(list.x, y, list.width, 1), row_style(selected));
        buf.set_line(list.x, y, &Line::from(spans).patch_style(row_style(selected)), list.width);
        if layout.row_height == 2 {
            fill(buf, Rect::new(list.x, y + 1, list.width, 1), row_style(selected));
            buf.set_stringn(
                list.x + 2,
                y + 1,
                if matches!(item.action, super::ui::PickAction::OpenDefinition(_)) {
                    crate::text::truncate_start(&item.detail, list.width.saturating_sub(2) as usize)
                } else {
                    item.detail.clone()
                },
                list.width.saturating_sub(2) as usize,
                row_style(selected),
            );
        }
    }
    if workspace && p.filtered.iter().all(|&i| Some(i) == p.pinned) && list.height > 0 {
        let message = if p.query.is_empty() { "No workspaces yet." } else { "No matching workspaces." };
        buf.set_stringn(list.x + 2, list.y, message, list.width.saturating_sub(2) as usize, dim());
    }
    if p.filtered.is_empty() {
        use super::ui::PickerKind as K;
        let msg = match p.kind {
            _ if p.loading.is_some() => if th().ascii { "Loading..." } else { "Loading…" }.into(),
            _ if !p.query.is_empty() => format!("Nothing matches \"{}\".", p.query),
            K::DeletedWorkspaces => "No deleted workspaces.".into(),
            K::Runbook => "No runbooks yet: `kraal runbook new <name>` creates one.".into(),
            K::Tool => "No tools: add some under [tools] in config.toml.".into(),
            K::Agent => "No accounts ready. Open Accounts from the Agents menu to sign in.".into(),
            K::Buffers => "Nothing copied yet: what you copy (copy mode, a mouse selection) shows here.".into(),
            _ => "Nothing here yet.".into(),
        };
        buf.set_stringn(list.x + 2, list.y, msg, list.width.saturating_sub(2) as usize, dim());
    }
    let keys = p.action_keys();
    let enter = match p.current().map(|i| &i.action) {
        Some(super::ui::PickAction::AgentLaunch { .. }) => Some("Start".to_string()),
        Some(super::ui::PickAction::AccountDetails(_)) => Some("Details".to_string()),
        _ => keys.first().map(|(_, what)| what.to_string()),
    };
    let mut footer: Vec<(&str, &str)> = Vec::new();
    if let Some(enter) = &enter {
        footer.push(("Enter", enter));
    }
    if keys.len() > 1 {
        footer.push(("Tab", "Actions"));
    }
    if p.undo.is_some() {
        footer.push(("Ctrl+Z", "Undo"));
    }
    footer.push(("Esc", if review || p.kind == super::ui::PickerKind::DeletedWorkspaces { "Back" } else { "Close" }));
    let count = if let Some(message) = &p.message {
        message.clone()
    } else if p.query.is_empty() {
        p.filtered.iter().filter(|&&i| Some(i) != p.pinned).count().to_string()
    } else {
        let matching = p.filtered.iter().filter(|&&i| Some(i) != p.pinned).count();
        let total = p.items.iter().enumerate().filter(|(i, _)| Some(*i) != p.pinned).count();
        format!("{matching}/{total}")
    };
    let mut line = key_line(&footer, &count, inner.width.saturating_sub(2));
    if p.message.is_some()
        && let Some(span) = line.spans.last_mut().filter(|span| span.content == count)
    {
        span.style = Style::default().fg(p.message_kind.colour(th()));
    }
    buf.set_line(inner.x + 1, inner.y + inner.height - 1, &line, inner.width.saturating_sub(2));
    if review || conflict {
        return None;
    }
    let before: String = p.query.chars().take(p.cursor).collect();
    let x = inner.x + 1 + columns("Search  ") as u16 + before.width() as u16;
    (x < inner.x + inner.width).then_some(Cursor { x, y: inner.y, shape: CursorShape::Bar, blinking: true })
}

/// Capitalize a prompt action for its footer.
fn upper_first(s: &str) -> String {
    let mut s = s.to_string();
    if let Some(first) = s.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    s
}

fn fill(buf: &mut Buffer, area: Rect, style: Style) {
    let area = area.intersection(buf.area);
    for y in area.y..area.y + area.height {
        for x in area.x..area.x + area.width {
            buf[(x, y)].set_style(style);
        }
    }
}

// ---- prompts --------------------------------------------------------------------------------

pub(super) struct PromptLayout {
    area: Rect,
    lines: Vec<String>,
    pub(super) body: u16,
    pub(super) max_scroll: usize,
}

pub(super) fn prompt_layout(p: &Prompt, screen: Rect) -> PromptLayout {
    let w = screen.width.saturating_sub(4).min(76);
    // Diagnostics include paths: keep their spacing when a line needs more than one row.
    let lines: Vec<_> =
        p.error.as_deref().unwrap_or("").lines().flat_map(|l| fold(l, w.saturating_sub(4) as usize)).collect();
    let h = (lines.len() + 4).min(screen.height.saturating_sub(2).max(4) as usize) as u16;
    let area = centered(screen, w, h);
    let body = area.height.saturating_sub(4);
    PromptLayout { area, max_scroll: lines.len().saturating_sub(body as usize), lines, body }
}

pub(super) fn draw_prompt(p: &Prompt, screen: Rect, buf: &mut Buffer) -> Option<Cursor> {
    if p.tunnel_form.is_some() {
        return super::tunnel_ui::draw_form(p, screen, buf);
    }
    if p.server_form.is_some() {
        return super::server_ui::draw_form(p, screen, buf);
    }
    if p.workspace_form.is_some() {
        return super::workspace_ui::draw_form(p, screen, buf);
    }
    let layout = prompt_layout(p, screen);
    let inner = frame(layout.area, &p.title, th().accent, buf);
    if inner.height < 2 || inner.width < 4 {
        return None;
    }
    // The part of the value around the cursor that fits: back from the cursor, one pass.
    let room = inner.width.saturating_sub(2) as usize;
    // A secret shows as dots, one per character.
    let chars: Vec<char> = if p.masked { p.value.chars().map(|_| '•').collect() } else { p.value.chars().collect() };
    let cursor = p.cursor.min(chars.len());
    let (mut start, mut before) = (cursor, 0);
    while start > 0 {
        let w = unicode_width::UnicodeWidthChar::width(chars[start - 1]).unwrap_or(0);
        if before + w >= room {
            break;
        }
        before += w;
        start -= 1;
    }
    let shown: String = chars[start..].iter().collect();
    buf.set_stringn(inner.x + 1, inner.y, &shown, room, Style::default());
    let x = inner.x + 1 + before as u16;
    let scroll = p.scroll.min(layout.max_scroll);
    for (i, line) in layout.lines.iter().skip(scroll).take(layout.body as usize).enumerate() {
        buf.set_stringn(inner.x + 1, inner.y + 1 + i as u16, line, room, Style::default().fg(th().production));
    }
    let verb = upper_first(p.action.verb());
    let mut keys = vec![("Enter", verb.as_str()), ("Esc", "Cancel")];
    if layout.max_scroll > 0 {
        keys.insert(1, ("PgUp/PgDn", "Scroll"));
    }
    let line = key_line(&keys, "", inner.width.saturating_sub(2));
    buf.set_line(inner.x + 1, inner.y + inner.height - 1, &line, inner.width.saturating_sub(2));
    Some(Cursor { x, y: inner.y, shape: CursorShape::Bar, blinking: true })
}

// ---- confirmations --------------------------------------------------------------------------

/// Where a question's parts are; the same for drawing and for mouse clicks.
pub(super) struct ConfirmLayout {
    pub(super) area: Rect,
    /// The message, cut to the box.
    pub(super) lines: Vec<ConfirmRow>,
    /// Rows for the message.
    pub(super) body: u16,
    /// How far the message scrolls (0: it fits); the row saying there is more.
    pub(super) max_scroll: usize,
    pub(super) more: Option<u16>,
    /// The buttons, when there is room to draw them.
    pub(super) cancel: Option<Rect>,
    pub(super) ok: Option<Rect>,
}

impl ConfirmLayout {
    /// How far a page key scrolls: the rows of the message in view.
    pub(super) fn page(&self) -> usize {
        usize::from(self.body).max(1)
    }
}

/// A row of a question's message.
pub(super) struct ConfirmRow {
    pub(super) text: String,
    /// The question itself: the message's first line.
    pub(super) question: bool,
    /// The rest of a quoted line that the row above could not hold.
    pub(super) continued: bool,
}

pub(super) fn confirm_layout(q: &Question, screen: Rect) -> ConfirmLayout {
    let verb = q.action.verb();
    let max_w = screen.width.saturating_sub(2).max(1) as usize;
    let buttons_w = columns(&verb) + 15;
    let footer_w = columns(&verb).max(columns(q.action.cancel_label()))
        + columns("Enter  · Tab Choose · Esc ")
        + columns(q.action.cancel_label());
    let widest =
        q.message.lines().chain(q.title.as_deref()).map(columns).max().unwrap_or(0).max(buttons_w).max(footer_w);
    let w = (widest + 4).clamp(30.min(max_w), max_w.min(84)) as u16;
    let text_w = (w as usize).saturating_sub(4).max(1);
    let mut lines = Vec::new();
    let title = q.title.as_deref().unwrap_or_else(|| q.message.lines().next().unwrap_or(""));
    let long_title = columns(title) > text_w;
    if q.title.is_some() && long_title {
        // Keep a long question readable after the title is clipped, including after a resize.
        lines.extend(fold(title, text_w).into_iter().map(|text| ConfirmRow { text, question: true, continued: false }));
    }
    for (i, l) in q.message.lines().enumerate() {
        if i == 0 && l == title && !long_title {
            continue;
        }
        // What a question quotes (see [`Question::message`]) is cut as it is written; the rest
        // is prose, wrapped between words.
        let quoted = l.starts_with(' ');
        let rows = if quoted { fold(l, text_w) } else { wrap(l, text_w) };
        for (j, text) in rows.into_iter().enumerate() {
            lines.push(ConfirmRow { text, question: i == 0, continued: quoted && j > 0 });
        }
    }
    let max_h = screen.height.saturating_sub(2).max(3) as usize;
    let h = (lines.len().max(1) + 5).min(max_h) as u16;
    let area = centered(screen, w, h);
    let inner = Rect::new(area.x + 1, area.y + 1, area.width.saturating_sub(2), area.height.saturating_sub(2));
    let mut body = inner.height.saturating_sub(3);
    // A message longer than the box gives up a row to say so, and scrolls.
    let more = (lines.len() > body as usize && body > 1).then(|| {
        body -= 1;
        inner.y + body
    });
    let max_scroll = lines.len().saturating_sub(body as usize);
    let y = inner.y + inner.height.saturating_sub(2);
    let buttons = inner.height >= 3 && inner.width >= columns(&verb) as u16 + 15;
    let cancel_width = columns(q.action.cancel_label()) as u16 + 2;
    let cancel = buttons.then(|| Rect::new(inner.x + 1, y, cancel_width, 1));
    let ok = buttons.then(|| Rect::new(inner.x + 1 + cancel_width + 3, y, columns(&verb) as u16 + 2, 1));
    ConfirmLayout { area, lines, body, max_scroll, more, cancel, ok }
}

pub(crate) fn button_style(selected: bool, danger: bool) -> Style {
    let color = if danger { th().production } else { th().accent };
    if selected {
        Style::default().fg(Color::Black).bg(color).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(th().text).add_modifier(Modifier::UNDERLINED)
    }
}

/// A question with its two buttons. The message wraps; when it is longer than the box, it
/// scrolls (the buttons stay in view). A quoted line longer than the box goes on in the rows
/// below it, each marked in the margin, where no text of the message is ever drawn: a long line
/// cannot be taken for several, nor several for one.
pub(super) fn draw_confirm(q: &Question, screen: Rect, buf: &mut Buffer) {
    let danger = q.action.danger(&q.message);
    let layout = confirm_layout(q, screen);
    let color = if danger { th().production } else { th().accent };
    let title = q.title.as_deref().unwrap_or_else(|| q.message.lines().next().unwrap_or(""));
    let inner = frame(layout.area, title, color, buf);
    if inner.height == 0 || inner.width < 2 {
        return;
    }
    let width = inner.width.saturating_sub(2) as usize;
    let body = layout.body as usize;
    let max_scroll = layout.max_scroll;
    let scroll = q.scroll.min(max_scroll);
    for (row, line) in layout.lines.iter().skip(scroll).take(body).enumerate() {
        let y = inner.y + row as u16;
        let style =
            if line.question { Style::default().add_modifier(Modifier::BOLD) } else { Style::default().fg(th().text) };
        buf.set_stringn(inner.x + 1, y, &line.text, width, style);
        if line.continued {
            buf.set_stringn(inner.x, y, if th().ascii { ">" } else { "↪" }, 1, dim());
        }
    }
    if let Some(y) = layout.more {
        let more = match max_scroll - scroll {
            0 => "↑ top".to_string(),
            n => format!("↓ {n} more line{}", if n == 1 { "" } else { "s" }),
        };
        let more = if th().ascii { more.replace('↓', "v").replace('↑', "^") } else { more };
        buf.set_stringn(inner.x + 1, y, format!("{more:>width$}"), width, dim());
    }
    let button =
        |label: &str, selected: bool, danger: bool| Span::styled(format!(" {label} "), button_style(selected, danger));
    if let Some(cancel) = layout.cancel {
        let row = Line::from(vec![
            button(q.action.cancel_label(), !q.yes, false),
            Span::raw("   "),
            button(&q.action.verb(), q.yes, danger),
        ]);
        buf.set_line(inner.x + 1, cancel.y, &row, inner.width.saturating_sub(2));
    }
    // Leaving comes last, so it is the one footer key that never gives way.
    let verb = q.action.verb();
    let selected = if q.yes { verb.as_str() } else { q.action.cancel_label() };
    let mut keys = vec![("Enter", selected), ("Tab", "Choose")];
    if max_scroll > 0 {
        keys.push(("PgUp/PgDn", "Scroll"));
    }
    keys.push(("Esc", q.action.cancel_label()));
    let line = key_line(&keys, "", inner.width.saturating_sub(2));
    buf.set_line(inner.x + 1, inner.y + inner.height - 1, &line, inner.width.saturating_sub(2));
}

// ---- text viewers (help, messages) ----------------------------------------------------------

/// A scrolling text box with a title and a footer of keys.
pub(super) fn draw_viewer(
    title: &str,
    lines: &[Line<'static>],
    scroll: u16,
    keys: &[(&str, &str)],
    screen: Rect,
    buf: &mut Buffer,
) {
    let area = viewer_area(lines.len(), screen);
    let inner = frame(area, title, th().accent, buf);
    if inner.height < 2 {
        return;
    }
    let body = inner.height - 1;
    let max_scroll = viewer_max_scroll(lines.len(), screen);
    let scroll = scroll.min(max_scroll) as usize;
    for (i, line) in lines.iter().skip(scroll).take(body as usize).enumerate() {
        buf.set_line(inner.x + 1, inner.y + i as u16, line, inner.width.saturating_sub(2));
    }
    let more = if max_scroll > 0 { format!("{}%", (scroll * 100) / max_scroll.max(1) as usize) } else { String::new() };
    let line = key_line(keys, &more, inner.width.saturating_sub(2));
    buf.set_line(inner.x + 1, inner.y + inner.height - 1, &line, inner.width.saturating_sub(2));
}

fn viewer_area(lines: usize, screen: Rect) -> Rect {
    let w = screen.width.saturating_sub(4).min(84);
    let h = screen.height.saturating_sub(2).min(lines.min(u16::MAX as usize - 3) as u16 + 3);
    centered(screen, w, h)
}

/// How far a viewer of `lines` can scroll in `screen`: the last line at the bottom.
pub(super) fn viewer_max_scroll(lines: usize, screen: Rect) -> u16 {
    // Frame, then the body, then the line of keys.
    let body = viewer_area(lines, screen).height.saturating_sub(3);
    (lines.min(u16::MAX as usize) as u16).saturating_sub(body)
}

/// A heading line of a viewer.
pub(super) fn heading(text: &str) -> Line<'static> {
    Line::from(Span::styled(text.to_string(), Style::default().fg(th().accent).add_modifier(Modifier::BOLD)))
}

/// `keys  what` with the keys in a column.
pub(super) fn entry(keys: &str, what: &str, key_w: usize) -> Line<'static> {
    let keys = shown_key(keys);
    let keys = keys.as_str();
    let pad = key_w.saturating_sub(columns(keys));
    Line::from(vec![
        Span::raw("  "),
        Span::styled(keys.to_string(), key_style()),
        Span::raw(" ".repeat(pad + 2)),
        Span::raw(what.to_string()),
    ])
}

#[cfg(test)]
mod tests {
    use super::super::ui::ConfirmAction;
    use super::*;

    #[test]
    fn picker_result_notices_use_their_kind_without_recolouring_keys() {
        use super::super::ui::{Mode, PickerKind};
        use crate::theme::NoticeKind;

        for (cols, rows) in [(80, 24), (124, 36)] {
            for (kind, message, colour) in [
                (NoticeKind::Notice, "Saved", th().text),
                (NoticeKind::Warning, "Check", th().blocked),
                (NoticeKind::Error, "Failed", th().blocked),
            ] {
                let Mode::Picker(p) = Picker::new(PickerKind::Workspace, "Workspaces", vec![]).result(kind, message)
                else {
                    panic!()
                };
                let screen = Rect::new(0, 0, cols, rows);
                let mut buf = Buffer::empty(screen);
                draw_picker(&p, screen, &mut buf);
                let mut found = false;
                for y in 0..rows {
                    let line: String = (0..cols).map(|x| buf[(x, y)].symbol()).collect();
                    if let Some(at) = line.find(message) {
                        let x = line[..at].chars().count() as u16;
                        for x in x..x + message.len() as u16 {
                            assert_eq!(buf[(x, y)].fg, colour);
                        }
                        let at = line.find("Esc").unwrap();
                        assert_eq!(buf[(line[..at].chars().count() as u16, y)].fg, th().accent);
                        found = true;
                    }
                }
                assert!(found);
            }
        }
    }

    /// Every kind of overlay, drawn in `screen` of `buf`: questions, a picker, a prompt, the menu
    /// and a viewer.
    fn draw_every_overlay(screen: Rect, buf: &mut Buffer) {
        let message = "Remove the worktree /a/very/long/path/that/does/not/fit and its branch kraal/x?\n\
                       It has 3 uncommitted changes: they will be lost.\nCloses 2 panes: a, b\n  \
                       1. a quoted command, longer than most of these boxes are wide, is cut to their rows\n";
        let picker = Picker::new(
            super::super::ui::PickerKind::Ssh,
            "Connect to a server",
            (0..40)
                .map(|i| {
                    super::super::ui::PickItem::new(
                        format!("web-{i}"),
                        "deploy@web",
                        super::super::ui::PickAction::UnavailableDefinition("invalid.kdl".into()),
                    )
                })
                .collect(),
        );
        let mut prompt =
            Prompt::new("Rename tab", "a long name that goes on and on", super::super::ui::PromptAction::RenamePane(1));
        prompt.error = Some(message.repeat(5));
        prompt.scroll = 99;
        let (km, _) = crate::keymap::Keymap::build(&crate::config::Config::default().keys);
        let menu = menu_items(&km.menu(&[]), true);
        let lines: Vec<Line<'static>> = (0..50).map(|i| entry("ctrl+b p r", &format!("line {i}"), 12)).collect();
        for (action, yes) in [(ConfirmAction::ClosePane(1), true), (ConfirmAction::TrustRunbook, false)] {
            let q = Question { title: None, message: message.repeat(5), action, yes, scroll: 99, workspace_back: None };
            draw_confirm(&q, screen, buf);
        }
        draw_picker(&picker, screen, buf);
        draw_prompt(&prompt, screen, buf);
        let layout = menu_layout(&menu, screen);
        draw_menu(&menu, &layout, ("ctrl+b", &[]), false, Some("q does nothing here"), buf);
        draw_viewer("Help", &lines, 99, &[("esc", "close")], screen, buf);
    }

    const SIZES: [(u16, u16); 8] = [(1, 1), (5, 3), (12, 4), (20, 5), (33, 6), (40, 10), (80, 24), (200, 60)];

    #[test]
    fn overlays_fit_any_terminal() {
        for (w, h) in SIZES {
            let screen = Rect::new(0, 0, w, h);
            draw_every_overlay(screen, &mut Buffer::empty(screen));
        }
    }

    #[test]
    fn overlays_leave_the_bottom_bar_alone() {
        // The bar's row, right under the screen overlays get.
        for (w, h) in SIZES {
            let mut buf = Buffer::empty(Rect::new(0, 0, w, h + 1));
            for x in 0..w {
                buf[(x, h)].set_symbol("=");
            }
            draw_every_overlay(Rect::new(0, 0, w, h), &mut buf);
            assert!((0..w).all(|x| buf[(x, h)].symbol() == "="), "{w}×{h}");
        }
    }

    /// The text of `area` in `buf`, a line per row.
    fn screen(buf: &Buffer, area: Rect) -> String {
        (area.top()..area.bottom())
            .map(|y| (area.left()..area.right()).map(|x| buf[(x, y)].symbol()).collect::<String>() + "\n")
            .collect()
    }

    #[test]
    fn prompt_errors_wrap_and_keep_input_and_actions_visible_at_both_sizes() {
        use super::super::ui::PromptAction;
        let path = format!("/{}a  b.kdl", "long-日-folder/".repeat(18));
        let error = format!("Couldn't write {path}.\nPermission denied. Fix the destination and retry.");
        for (w, h) in [(80, 24), (124, 36)] {
            let area = Rect::new(0, 0, w, h);
            for action in [PromptAction::OpenPreview] {
                let mut p = Prompt::new("Workspace", " café 日 ", action);
                p.cursor = 3;
                let mut buf = Buffer::empty(area);
                draw_prompt(&p, area, &mut buf);
                assert!(screen(&buf, area).contains(&upper_first(p.action.verb())));
                p.error = Some(error.clone());
                let layout = prompt_layout(&p, area);
                assert_eq!(layout.max_scroll, 0);
                assert_eq!(layout.area.width, 76);
                let cursor = draw_prompt(&p, area, &mut buf).unwrap();
                assert_eq!(cursor.x, layout.area.x + 5);
                assert_eq!(cursor.y, layout.area.y + 1);
                let shown = screen(&buf, area);
                assert!(shown.contains(" café 日 "), "{shown}");
                assert!(shown.contains("Permission denied. Fix the destination and retry."), "{shown}");
                assert!(shown.contains("Enter Open") && shown.contains("Esc Cancel"), "{shown}");
                assert!(!shown.contains("scroll"), "{shown}");
                for (i, line) in layout.lines.iter().enumerate() {
                    let mut x = layout.area.x + 2;
                    let y = layout.area.y + 2 + i as u16;
                    for c in line.chars() {
                        assert_eq!(buf[(x, y)].symbol(), c.to_string(), "missing {line:?}: {shown}");
                        x += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0) as u16;
                    }
                }
                assert_eq!(layout.lines.join(""), error.replace('\n', ""));
            }
        }
    }

    #[test]
    fn a_prompt_diagnostic_taller_than_the_terminal_is_readable_in_pages() {
        for (w, h) in [(80, 24), (124, 36)] {
            let area = Rect::new(0, 0, w, h);
            let mut p = Prompt::new("Preview file", "scratch", super::super::ui::PromptAction::OpenPreview);
            p.error = Some((0..100).map(|i| format!("Diagnostic line {i}\n")).collect());
            let layout = prompt_layout(&p, area);
            assert!(layout.max_scroll > 0);
            for start in (0..layout.lines.len()).step_by(layout.body as usize) {
                p.scroll = start;
                let mut buf = Buffer::empty(area);
                draw_prompt(&p, area, &mut buf);
                let shown = screen(&buf, area);
                assert!(shown.contains("scratch"), "{shown}");
                assert!(shown.contains("Enter Open · PgUp/PgDn Scroll · Esc Cancel"), "{shown}");
                for line in layout.lines.iter().skip(start.min(layout.max_scroll)).take(layout.body as usize) {
                    assert!(shown.contains(line), "missing {line:?}: {shown}");
                }
            }
        }
    }

    #[test]
    fn screen_list_counts_exclude_add_rows() {
        use super::super::ui::{PickAction, PickItem, PickerKind};
        for (w, h) in [(80, 24), (124, 36)] {
            for kind in [PickerKind::Workspace, PickerKind::Accounts] {
                for total in [0, 2] {
                    let workspace = kind == PickerKind::Workspace;
                    let mut items: Vec<_> = (0..total)
                        .map(|i| {
                            PickItem::new(
                                if i == 0 { "alpha" } else { "beta" },
                                "",
                                if workspace {
                                    PickAction::SwitchWorkspace(i)
                                } else {
                                    PickAction::AccountDetails(i.to_string())
                                },
                            )
                        })
                        .collect();
                    items.push(PickItem::new(
                        if workspace { "+ Add workspace" } else { "+ Add account" },
                        "",
                        if workspace { PickAction::AddWorkspace } else { PickAction::AddAccount },
                    ));
                    let mut p = Picker::new(kind, "List", items);
                    for query in ["", "alpha", "zzz"] {
                        p.query = query.into();
                        p.refilter();
                        let area = Rect::new(0, 0, w, h);
                        let mut buf = Buffer::empty(area);
                        draw_picker(&p, area, &mut buf);
                        let layout = picker_layout(&p, area);
                        let footer = screen(
                            &buf,
                            Rect::new(layout.area.x + 1, layout.area.bottom() - 2, layout.area.width - 2, 1),
                        );
                        let expected = if query.is_empty() {
                            total.to_string()
                        } else {
                            format!("{}/{total}", u32::from(query == "alpha" && total > 0))
                        };
                        assert_eq!(footer.split_whitespace().last(), Some(expected.as_str()), "{footer}");
                        assert!(screen(&buf, area).contains(if workspace {
                            "+ Add workspace"
                        } else {
                            "+ Add account"
                        }));
                    }
                }
            }
        }
    }

    #[test]
    fn edit_conflicts_show_an_instruction_without_an_input_cursor() {
        use super::super::edits::Answer;
        use super::super::ui::{PickAction, PickItem, PickerKind};
        for (w, h) in [(80, 24), (124, 36)] {
            let area = Rect::new(0, 0, w, h);
            let items = [
                ("Keep your version as a local file", Answer::KeepLocal),
                ("Keep the version of box", Answer::KeepServer),
                ("Replace it with your version", Answer::Overwrite),
            ]
            .into_iter()
            .map(|(label, answer)| PickItem::new(label, "", PickAction::EditAnswer { edit: 1, answer }))
            .collect();
            let mut p = Picker::new(PickerKind::EditConflict, "File changed on box", items);
            for selected in 0..p.items.len() {
                p.selected = selected;
                let mut buf = Buffer::empty(area);
                assert!(draw_picker(&p, area, &mut buf).is_none());
                let shown = screen(&buf, area);
                assert!(shown.contains("Choose which version to keep."), "{shown}");
                assert!(!shown.contains("Search"), "{shown}");
                for item in &p.items {
                    assert!(shown.contains(&item.label), "{shown}");
                }
                assert!(shown.contains("Enter Choose") && shown.contains("Esc Close"), "{shown}");
            }
        }
    }

    #[test]
    fn shared_picker_search_and_actions_use_explicit_key_names() {
        use super::super::ui::{PickAction, PickItem, PickerKind};
        for (w, h) in [(80, 24), (124, 36)] {
            let area = Rect::new(0, 0, w, h);
            for (kind, action, verb) in [
                (PickerKind::Accounts, PickAction::AddAccount, "Add account"),
                (PickerKind::Accounts, PickAction::AccountDetails("work".into()), "Details"),
                (
                    PickerKind::Agent,
                    PickAction::AgentLaunch { agent: "claude".into(), account: "work".into() },
                    "Start",
                ),
                (PickerKind::Tunnels, PickAction::Tunnel { id: 1, retry: true }, "Details"),
                (PickerKind::Ssh, PickAction::Ssh("web".into()), "Connect"),
                (PickerKind::Workspace, PickAction::SwitchWorkspace(1), "Open"),
                (PickerKind::Tunnels, PickAction::AddTunnel, "Add"),
            ] {
                let mut p = Picker::new(kind, "List", vec![PickItem::new("item", "", action)]);
                p.query = "it".into();
                p.cursor = 2;
                p.refilter();
                let mut buf = Buffer::empty(area);
                let cursor = draw_picker(&p, area, &mut buf).unwrap();
                let layout = picker_layout(&p, area);
                let shown = screen(&buf, area);
                assert!(shown.contains("Search  it"), "{shown}");
                assert_eq!(cursor.x, layout.area.x + 2 + 8 + 2);
                assert!(shown.contains(&format!("Enter {verb}")) && shown.contains("Esc Close"), "{shown}");
                if p.action_keys().len() > 1 {
                    assert!(shown.contains("Tab Actions"), "{shown}");
                    p.actions = Some(1);
                    buf = Buffer::empty(area);
                    draw_picker(&p, area, &mut buf);
                    let layout = picker_layout(&p, area);
                    let footer = screen(&buf, Rect::new(layout.area.x, layout.area.bottom() - 2, layout.area.width, 1));
                    assert!(footer.contains(&format!("Enter {}", p.action_keys()[1].1)), "{footer}");
                    assert!(footer.contains("Esc Back"), "{footer}");
                }
            }
        }
    }

    #[test]
    fn questions_use_the_question_as_title_and_name_the_selected_action() {
        use super::super::ui::Mode;
        for (w, h) in [(80, 24), (124, 36)] {
            let area = Rect::new(0, 0, w, h);
            for message in [
                "Delete web?\nThe layout can be restored.",
                "Close pane?",
                "Trust this runbook?\nRead its commands first.",
            ] {
                let Mode::Confirm(mut q) = Mode::confirm(message, ConfirmAction::ClosePane(1)) else { panic!() };
                for yes in [false, true] {
                    q.yes = yes;
                    let mut buf = Buffer::empty(area);
                    draw_confirm(&q, area, &mut buf);
                    let layout = confirm_layout(&q, area);
                    assert!(layout.body > 0 && layout.cancel.is_some() && layout.ok.is_some());
                    let title = screen(&buf, Rect::new(layout.area.x, layout.area.y, layout.area.width, 1));
                    assert!(title.contains(message.lines().next().unwrap()), "{title}");
                    let shown = screen(&buf, area);
                    assert_eq!(shown.matches(message.lines().next().unwrap()).count(), 1, "{shown}");
                    let verb = if yes { q.action.verb() } else { q.action.cancel_label().into() };
                    assert!(shown.contains(&format!("Enter {verb} · Tab Choose · Esc Cancel")), "{shown}");
                }
            }
        }
    }

    #[test]
    fn workspace_actions_keep_management_before_close_and_delete() {
        use super::super::ui::{PickAction, PickItem, PickerKind};
        for (w, h) in [(80, 24), (124, 36)] {
            for open in [false, true] {
                let action = if open { PickAction::SwitchWorkspace(1) } else { PickAction::ReopenWorkspace(1) };
                let mut p = Picker::new(
                    PickerKind::Workspace,
                    "Workspaces",
                    vec![PickItem::new("web", "", action), PickItem::new("docs", "", PickAction::ReopenWorkspace(2))],
                );
                p.source_workspaces.push(1);
                p.review_workspaces.push(1);
                p.layout_undos.push(1);
                p.actions = Some(0);
                let area = Rect::new(0, 0, w, h);
                let mut buf = Buffer::empty(area);
                draw_picker(&p, area, &mut buf);
                let layout = picker_layout(&p, area);
                let shown = screen(&buf, layout.list);
                let mut expected = vec![
                    "Open",
                    "Rename",
                    "Duplicate",
                    "Move down",
                    "Export layout",
                    "Load from file",
                    "Review",
                    "Undo load",
                ];
                if open {
                    expected.push("Close");
                }
                expected.push("Delete");
                let positions: Vec<_> = expected
                    .iter()
                    .map(|label| shown.find(label).unwrap_or_else(|| panic!("{label}: {shown}")))
                    .collect();
                assert!(positions.windows(2).all(|p| p[0] < p[1]), "{shown}");
            }
        }
    }

    #[test]
    fn picker_footers_describe_the_selected_line_at_both_sizes() {
        use super::super::ui::{PickAction, PickItem, PickerKind};
        for (w, h) in [(124, 36), (80, 24)] {
            let area = Rect::new(0, 0, w, h);
            let cases = [
                (PickerKind::Accounts, PickAction::AddAccount, "Add account", false),
                (
                    PickerKind::Agent,
                    PickAction::AgentSwitch { pane: 1, agent: "codex".into(), account: "work".into(), current: false },
                    "Switch account",
                    false,
                ),
                (
                    PickerKind::Agent,
                    PickAction::AgentSwitch { pane: 1, agent: "codex".into(), account: "work".into(), current: true },
                    "Keep current account",
                    false,
                ),
                (PickerKind::Ssh, PickAction::SshGroup("web".into()), "Open group", true),
                (PickerKind::Workspace, PickAction::SwitchWorkspace(1), "Open", true),
                (PickerKind::Workspace, PickAction::OpenDefinition("w.kdl".into()), "Open", true),
                (PickerKind::Workspace, PickAction::AddWorkspace, "Add", false),
                (PickerKind::RunbookSteps, PickAction::RunbookList, "Back to runbooks", false),
            ];
            for (kind, action, enter, more) in cases {
                let create = matches!(action, PickAction::AddWorkspace);
                let mut p = Picker::new(kind, "List", vec![PickItem::new("item", "", action)]);
                if create {
                    p.query = "new".into();
                    p.refilter();
                }
                let mut buf = Buffer::empty(area);
                draw_picker(&p, area, &mut buf);
                let layout = picker_layout(&p, area);
                let footer = screen(&buf, Rect::new(layout.area.x, layout.area.bottom() - 2, layout.area.width, 1));
                assert!(footer.contains(enter), "{w}×{h}: {footer}");
                assert_eq!(footer.contains("Tab Actions"), more, "{footer}");
                if let Some((add, index)) = layout.pinned {
                    assert_eq!(layout.row_at(layout.list.x + 2, layout.list.y), None);
                    assert_eq!(layout.row_at(add.x + 2, add.y), Some(index));
                } else {
                    assert_eq!(layout.row_at(layout.list.x + 2, layout.list.y), Some(0));
                }
            }
            let p = Picker::new(PickerKind::Agent, "Agents", vec![]);
            let mut buf = Buffer::empty(area);
            draw_picker(&p, area, &mut buf);
            let text = screen(&buf, area);
            assert!(text.contains("Esc Close"));
            assert!(!text.contains("⏎") && !text.contains("more actions"), "{text}");
            let p = Picker::new(
                PickerKind::Workspace,
                "Workspaces",
                vec![PickItem::new("invalid", "error", PickAction::UnavailableDefinition("invalid.kdl".into()))],
            );
            draw_picker(&p, area, &mut buf);
            let layout = picker_layout(&p, area);
            let footer = screen(&buf, Rect::new(layout.area.x, layout.area.bottom() - 2, layout.area.width, 1));
            assert!(footer.contains("Esc Close"));
            assert!(!footer.contains("⏎") && !footer.contains("more actions"), "{footer}");
        }
    }

    #[test]
    fn workspace_add_stays_visible_and_names_are_not_dimmed_while_scrolling_and_searching() {
        use super::super::ui::{PickAction, PickItem, PickerKind};
        for (w, h) in [(80, 24), (124, 36)] {
            let area = Rect::new(0, 0, w, h);
            let mut items: Vec<_> = (0..40)
                .map(|i| {
                    PickItem::new(format!("workspace-{i}"), "Closed · 1 tab · 2 panes", PickAction::ReopenWorkspace(i))
                })
                .collect();
            items.push(PickItem::new("+ Add workspace", "", PickAction::AddWorkspace));
            let mut p = Picker::new(PickerKind::Workspace, "Workspaces", items);
            for selected in [0, 20, 39, 40] {
                p.selected = selected;
                let layout = picker_layout(&p, area);
                let mut buf = Buffer::empty(area);
                draw_picker(&p, area, &mut buf);
                let shown = screen(&buf, area);
                assert!(shown.contains("+ Add workspace"), "{shown}");
                assert!(shown.contains(&p.current().unwrap().label), "{shown}");
                let (add, index) = layout.pinned.unwrap();
                assert_eq!(index, 40);
                assert_eq!(layout.row_at(add.x + 2, add.y), Some(40));
                assert_eq!(layout.row_at(add.x + 2, add.y + 1), Some(40));
                assert!(buf[(layout.list.x + 2, layout.list.y)].modifier.contains(Modifier::BOLD));
                assert_ne!(buf[(layout.list.x + 2, layout.list.y)].fg, th().dim);
            }
            p.query = "does-not-exist".into();
            p.refilter();
            let mut buf = Buffer::empty(area);
            draw_picker(&p, area, &mut buf);
            let shown = screen(&buf, area);
            assert!(shown.contains("No matching workspaces."));
            assert!(shown.contains("Enter Add"));
            assert!(!shown.contains("Tab Actions"));
            assert_eq!(p.current().unwrap().label, "+ Add workspace");
        }
    }

    #[test]
    fn the_menu_box_shows_every_entry() {
        for (preset, size) in [("kraal", (80, 24)), ("kraal", (200, 50)), ("classic", (80, 24)), ("classic", (120, 40))]
        {
            let cfg = crate::config::Config::from_user_toml(&format!("[keys]\npreset = \"{preset}\"\n")).unwrap();
            let (km, _) = crate::keymap::Keymap::build(&cfg.keys);
            let items = menu_items(&km.menu(&[]), true);
            let full = Rect::new(0, 0, size.0, size.1);
            let layout = menu_layout(&items, full);
            // Against the right edge and the bottom, below the top bar.
            assert_eq!(layout.area.right(), full.right() - 1, "{preset} {size:?}");
            assert_eq!(layout.area.bottom(), full.bottom(), "{preset} {size:?}");
            assert!(layout.area.y >= 1, "{preset} {size:?}: {:?}", layout.area);
            // Every entry is there, inside the box, and a click finds it.
            let inside = Rect::new(layout.area.x + 1, layout.area.y + 1, layout.area.width - 2, layout.area.height - 2);
            let mut seen: Vec<usize> = layout.cells.iter().map(|c| c.item).collect();
            seen.sort();
            assert_eq!(seen, (0..items.len()).collect::<Vec<_>>(), "{preset} {size:?}");
            for cell in &layout.cells {
                assert_eq!(layout.item_at(cell.area.x, cell.area.y), Some(cell.item));
                assert!(inside.contains(Position::new(cell.area.right() - 1, cell.area.y)), "{preset} {size:?}");
            }
            let mut buf = Buffer::empty(full);
            let footer = menu_footer(&km.menu(&[]), true);
            draw_menu(&items, &layout, ("ctrl+b", &footer), false, None, &mut buf);
            let text = screen(&buf, layout.area);
            for cell in &layout.cells {
                let key = &items[cell.item].key;
                assert!(text.contains(key.as_str()), "{preset} {size:?}: {key}\n{text}");
            }
            assert!(text.contains("help") && text.contains("esc close"), "{preset} {size:?}\n{text}");
        }
    }

    #[test]
    fn the_kraal_menu_reads_as_three_columns() {
        let (km, _) = crate::keymap::Keymap::build(&crate::config::Config::default().keys);
        let items = menu_items(&km.menu(&[]), true);
        let full = Rect::new(0, 0, 100, 30);
        let layout = menu_layout(&items, full);
        let column = |section: u8| -> Vec<u16> {
            let mut xs: Vec<u16> =
                layout.cells.iter().filter(|c| items[c.item].section == section).map(|c| c.area.x).collect();
            xs.dedup();
            xs
        };
        // Main actions, other keys and menus each in one column, in that order.
        let (main, keys, menus) = (column(0), column(1), column(2));
        assert!(main.len() == 1 && keys.len() == 1 && menus.len() == 1, "{main:?} {keys:?} {menus:?}");
        assert!(main[0] < keys[0] && keys[0] < menus[0]);
        let mut buf = Buffer::empty(full);
        let footer = menu_footer(&km.menu(&[]), true);
        draw_menu(&items, &layout, ("ctrl+b", &footer), false, None, &mut buf);
        let text = screen(&buf, layout.area);
        assert!(text.contains("s  Connect to a server") && text.contains("S  Servers…"), "{text}");
        assert!(text.contains("h help · d detach · q quit · esc close"), "{text}");
    }

    #[test]
    fn quit_is_in_the_footer_of_the_first_menu_and_a_line_further_in() {
        let full = Rect::new(0, 0, 120, 40);
        for (preset, keys) in [("kraal", "d detach · q quit"), ("classic", "d detach · Q quit")] {
            let cfg = crate::config::Config::from_user_toml(&format!("[keys]\npreset = \"{preset}\"\n")).unwrap();
            let (km, _) = crate::keymap::Keymap::build(&cfg.keys);
            let root = km.menu(&[]);
            let items = menu_items(&root, true);
            assert!(items.iter().all(|i| i.label != "Quit"), "{preset}");
            let layout = menu_layout(&items, full);
            let mut buf = Buffer::empty(full);
            draw_menu(&items, &layout, ("ctrl+b", &menu_footer(&root, true)), false, None, &mut buf);
            let text = screen(&buf, layout.area);
            assert!(text.contains(keys), "{preset}\n{text}");
        }
        let (km, _) = crate::keymap::Keymap::build(&crate::config::Config::default().keys);
        let w = crate::input::Key { code: crate::input::KeyCode::Char('W'), mods: Default::default(), raw: Vec::new() };
        let workspaces = km.menu(&[w]);
        assert!(menu_footer(&workspaces, false).is_empty());
        assert!(menu_items(&workspaces, false).iter().any(|i| i.key == "q" && i.label == "Quit"));
    }

    #[test]
    fn a_viewer_scrolls_to_its_last_line_and_no_further() {
        let full = Rect::new(0, 0, 80, 24);
        let lines: Vec<Line<'static>> = (0..100).map(|i| Line::raw(format!("line {i}"))).collect();
        let max = viewer_max_scroll(lines.len(), full);
        let draw = |scroll: u16| {
            let mut buf = Buffer::empty(full);
            draw_viewer("Help", &lines, scroll, &[("esc", "close")], full, &mut buf);
            screen(&buf, full)
        };
        assert!(draw(max).contains("line 99"));
        assert!(!draw(max - 1).contains("line 99"), "the last line shows only at the end");
        assert_eq!(viewer_max_scroll(3, full), 0);
    }

    #[test]
    fn every_line_of_a_long_question_can_be_read() {
        let message: String = (1..=30).map(|i| format!("line number {i}\n")).collect();
        let area = Rect::new(0, 0, 60, 12);
        let mut q = Question {
            title: None,
            message,
            action: ConfirmAction::ClosePane(1),
            yes: false,
            scroll: 0,
            workspace_back: None,
        };
        let layout = confirm_layout(&q, area);
        assert!(layout.max_scroll > 0 && layout.more.is_some() && layout.cancel.is_some());
        q.scroll = layout.max_scroll;
        let mut buf = Buffer::empty(area);
        draw_confirm(&q, area, &mut buf);
        let screen: String = (0..area.height)
            .map(|y| (0..area.width).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>() + "\n")
            .collect();
        assert!(screen.contains("line number 30") && screen.contains("↑ top"), "{screen}");
    }

    /// What a question quotes keeps its spacing, and a line of it longer than the box goes on
    /// in rows marked as its continuation; the prose around it wraps between words.
    #[test]
    fn a_question_quotes_lines_as_they_are_written() {
        let message = format!(
            "Trust the runbook deploy?\none two three four five six seven eight nine ten eleven twelve\n  \
             1. if true; then\n         echo  two   spaces\n     \n     fi\n  2. echo {}",
            "x".repeat(60)
        );
        let area = Rect::new(0, 0, 50, 20);
        let q = Question {
            title: None,
            message,
            action: ConfirmAction::TrustRunbook,
            yes: false,
            scroll: 0,
            workspace_back: None,
        };
        let layout = confirm_layout(&q, area);
        let rows: Vec<(&str, bool)> = layout.lines.iter().map(|row| (row.text.as_str(), row.continued)).collect();
        let (cut, rest) = (format!("  2. echo {}", "x".repeat(34)), format!("  {}", "x".repeat(26)));
        let expected = [
            ("one two three four five six seven eight nine", false),
            ("ten eleven twelve", false),
            ("  1. if true; then", false),
            ("         echo  two   spaces", false),
            ("     ", false),
            ("     fi", false),
            (cut.as_str(), false),
            (rest.as_str(), true),
        ];
        assert_eq!(rows, expected);
        // The mark is in the margin, which the message never reaches.
        let mut buf = Buffer::empty(area);
        draw_confirm(&q, area, &mut buf);
        let margin = |row: u16| buf[(layout.area.x + 1, layout.area.y + 1 + row)].symbol().to_string();
        assert_eq!((margin(6), margin(7)), (" ".to_string(), "↪".to_string()), "{}", screen(&buf, area));
        assert!(screen(&buf, area).contains("         echo  two   spaces"));
    }

    #[test]
    fn key_lines_drop_what_does_not_fit() {
        let text = |l: &Line| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>();
        let keys = [("Enter", "Connect"), ("Tab", "Actions"), ("Esc", "Close")];
        assert_eq!(text(&key_line(&keys, "", 80)), "Enter Connect · Tab Actions · Esc Close");
        assert_eq!(text(&key_line(&keys, "", 25)), "Enter Connect · Esc Close", "leaving stays possible");
        let with_count = text(&key_line(&keys, "3/40", 60));
        assert!(with_count.ends_with("3/40") && columns(&with_count) == 60, "{with_count:?}");
    }
}
