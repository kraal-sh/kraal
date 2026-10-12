//! Drawing the file manager: panels, rows, header and the footer of key hints.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use unicode_width::UnicodeWidthChar;

use super::{Entry, FileKey, FilesPane, Panel, Row, Side};
use crate::text::{
    columns as text_width, human_size, local_minutes, pad_start, sanitize, sanitize_lines, truncate, truncate_start,
    wrap,
};
use crate::theme::current as theme;

/// Narrower areas show only the active panel.
pub(super) const MIN_SPLIT_WIDTH: u16 = 24;
pub(super) const SIZE_WIDTH: usize = 7;
pub(super) const DATE_WIDTH: usize = 16;
/// Narrowest name column kept before the date, then the size, are dropped.
pub(super) const MIN_NAME_WIDTH: usize = 8;

// The file manager's styles, from the interface theme.
pub(super) fn dim() -> Style {
    Style::new().fg(theme().dim)
}
pub(super) fn note() -> Style {
    dim().add_modifier(Modifier::ITALIC)
}
pub(super) fn error_style() -> Style {
    Style::new().fg(theme().production)
}
pub(super) fn dir_name() -> Style {
    Style::new().fg(Color::LightBlue).add_modifier(Modifier::BOLD)
}
pub(super) fn link_name() -> Style {
    Style::new().fg(Color::LightCyan)
}
pub(super) fn marked_style() -> Style {
    Style::new().fg(theme().blocked).add_modifier(Modifier::BOLD)
}
pub(super) fn filter_style() -> Style {
    Style::new().fg(theme().blocked)
}
pub(super) fn header_active() -> Style {
    Style::new().fg(Color::Black).bg(theme().accent).add_modifier(Modifier::BOLD)
}
pub(super) fn header_inactive() -> Style {
    Style::new().fg(theme().text).bg(Color::Indexed(236))
}
pub(super) fn cursor_active() -> Style {
    Style::new().bg(theme().selection).add_modifier(Modifier::BOLD)
}
pub(super) fn cursor_inactive() -> Style {
    Style::new().bg(Color::Indexed(235))
}
pub(super) fn hint_key() -> Style {
    Style::new().fg(theme().accent).add_modifier(Modifier::BOLD)
}
pub(super) fn hint_text() -> Style {
    dim()
}

impl Panel {
    /// Lines shown after the rows: loading, error or empty notes.
    pub(super) fn messages(&self, width: usize) -> Vec<(String, Style)> {
        if let Some(error) = &self.error {
            return wrap(&sanitize_lines(error), width).into_iter().map(|line| (line, error_style())).collect();
        }
        if !self.shown.is_empty() {
            return Vec::new();
        }
        let text = if self.loading {
            "loading…"
        } else if self.entries.is_empty() {
            "(empty)"
        } else if !self.filter.is_empty() {
            "(no match)"
        } else {
            "(only hidden files, press .)"
        };
        vec![(text.to_owned(), note())]
    }

    /// The panel's status line.
    pub(super) fn status(&self, show_hidden: bool) -> (String, Style) {
        if self.editing {
            return (format!("/{}▏", sanitize(&self.filter)), filter_style());
        }
        if self.error.is_some() || (self.loading && self.entries.is_empty()) {
            return (String::new(), dim());
        }
        let mut parts = Vec::new();
        let marked = self.marked_entries();
        let style = if marked.is_empty() {
            let files = self.shown_entries().filter(|e| !e.is_dir);
            let size: u64 = files.map(|e| e.size).fold(0, u64::saturating_add);
            let count = self.shown.len();
            parts.push(format!("{count} item{}, {}", if count == 1 { "" } else { "s" }, human_size(size)));
            dim()
        } else {
            // Directory sizes are unknown: the size counts the marked files only.
            let files: Vec<&&Entry> = marked.iter().filter(|e| !e.is_dir).collect();
            let size: u64 = files.iter().map(|e| e.size).fold(0, u64::saturating_add);
            if files.is_empty() {
                parts.push(format!("{} marked", marked.len()));
            } else {
                parts.push(format!("{} marked, {}", marked.len(), human_size(size)));
            }
            marked_style()
        };
        if !self.filter.is_empty() {
            parts.push(format!("filter: {}", sanitize(&self.filter)));
        }
        let hidden = self.entries.iter().filter(|e| e.name.starts_with('.')).count();
        if !show_hidden && hidden > 0 {
            parts.push(format!("{hidden} hidden"));
        }
        (parts.join(" · "), style)
    }
}

impl FilesPane {
    pub(super) fn draw(&self, area: Rect, buf: &mut Buffer) {
        let area = area.intersection(buf.area);
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                buf[(x, y)].reset();
            }
        }
        if area.is_empty() {
            return;
        }
        let geo = geometry(area, self.active);
        for side in [Side::Left, Side::Right] {
            self.draw_panel(side, geo.panels[side.index()], &geo, buf);
        }
        if let Some(x) = geo.separator {
            let bottom = geo.footer.unwrap_or(area.bottom());
            for y in area.top()..bottom {
                put(buf, area, x, y, "│", 1, dim());
            }
        }
        if let Some(y) = geo.footer {
            let hints = self.hints();
            let width = usize::from(area.width);
            for (n, (start, end, index)) in footer_layout(&hints, area.width).into_iter().enumerate() {
                let Some(hint) = hints.get(index) else { continue };
                if n > 0 {
                    put(buf, area, area.x.saturating_add(start.saturating_sub(3)), y, " · ", width, hint_text());
                }
                let after_key = put(buf, area, area.x.saturating_add(start), y, hint.label, width, hint_key());
                let text_width = usize::from(end.saturating_sub(start)).saturating_sub(text_width(hint.label));
                put(buf, area, after_key, y, &format!(" {}", hint.text), text_width, hint_text());
            }
        }
    }

    pub(super) fn draw_panel(&self, side: Side, rect: Rect, geo: &Geometry, buf: &mut Buffer) {
        if rect.is_empty() {
            return;
        }
        let panel = self.panel(side);
        let active = side == self.active;
        let width = usize::from(rect.width);
        let inner = width.saturating_sub(1);
        if let Some(y) = geo.header {
            let style = if active { header_active() } else { header_inactive() };
            buf.set_style(Rect::new(rect.x, y, rect.width, 1).intersection(rect), style);
            let ascii = theme().ascii;
            let mark = match (active, ascii) {
                (true, true) => "> ",
                (true, false) => "▸ ",
                (false, _) => "  ",
            };
            let mut text = sanitize(&panel.location.short_display());
            if panel.loading && !panel.entries.is_empty() {
                text.push_str("  loading…");
            }
            // A server's name stays in view; a long path loses its start instead.
            let room = width.saturating_sub(2 + mark.len().min(2));
            let shown = match (&panel.location, text.split_once(':')) {
                (super::Location::Remote(_), Some((host, path))) if text_width(host) + 2 < room => {
                    format!("{host}:{}", truncate_start(path, room - text_width(host) - 1))
                }
                _ => truncate_start(&text, room),
            };
            put(buf, rect, rect.x, y, &format!("{mark}{shown}"), inner + 1, style);
        }
        let columns = Columns::new(width);
        if let Some(y) = geo.columns {
            put(buf, rect, rect.x.saturating_add(1), y, &truncate("Name", columns.name), columns.name, dim());
            if let Some(x) = columns.size_x() {
                put(buf, rect, rect.x.saturating_add(x), y, &pad_start("Size", SIZE_WIDTH), SIZE_WIDTH, dim());
            }
            if let Some(x) = columns.date_x() {
                put(buf, rect, rect.x.saturating_add(x), y, "Modified", DATE_WIDTH, dim());
            }
        }
        let (list_y, list_height) = geo.list;
        let offset = panel.scroll_offset(self.viewer, usize::from(list_height));
        let messages = panel.messages(width.saturating_sub(2).max(1));
        let rows = panel.rows_len();
        for (index, y) in (offset..).zip(list_y..list_y.saturating_add(list_height)) {
            match panel.row(index) {
                Some(row) => {
                    let cursor = index == panel.cursor;
                    self.draw_row(panel, row, RowPlace { rect, y, cursor, active, columns }, buf);
                }
                None => {
                    if let Some((text, style)) = index.checked_sub(rows).and_then(|i| messages.get(i)) {
                        put(buf, rect, rect.x.saturating_add(1), y, text, inner, *style);
                    }
                }
            }
        }
        if let Some(y) = geo.status {
            let (text, style) = panel.status(self.show_hidden);
            put(buf, rect, rect.x.saturating_add(1), y, &truncate(&text, inner), inner, style);
        }
    }

    pub(super) fn draw_row(&self, panel: &Panel, row: Row<'_>, place: RowPlace, buf: &mut Buffer) {
        let RowPlace { rect, y, cursor, active, columns } = place;
        let (name, size, date, mut style) = match row {
            Row::Parent => ("..".to_owned(), "UP--DIR".to_owned(), String::new(), dir_name()),
            Row::Entry(entry) => {
                let mut name = sanitize(&entry.name);
                if entry.is_dir {
                    name.push('/');
                } else if entry.is_link {
                    name.push('@');
                }
                let size = if entry.is_dir { "<DIR>".to_owned() } else { human_size(entry.size) };
                let mut style = if panel.marked.contains(&entry.name) {
                    marked_style()
                } else if entry.is_link {
                    link_name()
                } else if entry.is_dir {
                    dir_name()
                } else {
                    Style::new()
                };
                if entry.name.starts_with('.') {
                    style = style.add_modifier(Modifier::DIM);
                }
                (name, size, local_minutes(entry.mtime), style)
            }
        };
        let marked = matches!(row, Row::Entry(e) if panel.marked.contains(&e.name));
        let mut detail = if marked { marked_style() } else { Style::new() };
        if cursor {
            let highlight = if active { cursor_active() } else { cursor_inactive() };
            buf.set_style(Rect::new(rect.x, y, rect.width, 1).intersection(rect), highlight);
            style = style.patch(highlight);
            detail = detail.patch(highlight);
            if active && marked {
                style = style.fg(Color::Yellow);
                detail = detail.fg(Color::Yellow);
            }
        }
        // Selection and the cursor are marks of their own, not only colours.
        let ascii = theme().ascii;
        if marked {
            put(buf, rect, rect.x, y, if ascii { "*" } else { "✓" }, 1, style);
        } else if cursor && active {
            put(buf, rect, rect.x, y, if ascii { ">" } else { "›" }, 1, style);
        }
        put(buf, rect, rect.x.saturating_add(1), y, &truncate(&name, columns.name), columns.name, style);
        if let Some(x) = columns.size_x() {
            put(buf, rect, rect.x.saturating_add(x), y, &pad_start(&size, SIZE_WIDTH), SIZE_WIDTH, detail);
        }
        if let Some(x) = columns.date_x() {
            put(buf, rect, rect.x.saturating_add(x), y, &truncate(&date, DATE_WIDTH), DATE_WIDTH, detail);
        }
    }

    pub(super) fn hints(&self) -> Vec<Hint> {
        if self.panel(self.active).editing {
            return vec![
                Hint::new("type", "to filter", None),
                Hint::new("enter", "keep", Some(FileKey::Enter)),
                Hint::new("esc", "clear", Some(FileKey::Esc)),
            ];
        }
        let arrow = if self.active == Side::Left { "→" } else { "←" };
        vec![
            Hint::new("tab", "switch", Some(FileKey::Tab)),
            Hint::new("enter", "open", Some(FileKey::Enter)),
            Hint::new("⌫", "up", Some(FileKey::Backspace)),
            Hint::new("space", "mark", Some(FileKey::Char(' '))),
            Hint { label: "c", text: format!("copy {arrow}"), key: Some(FileKey::Char('c')) },
            Hint::new("m", "move", Some(FileKey::Char('m'))),
            Hint::new("d", "delete", Some(FileKey::Char('d'))),
            Hint::new("e", "edit", Some(FileKey::Char('e'))),
            Hint::new("s", "shell", Some(FileKey::Char('s'))),
            Hint::new("o", "server", Some(FileKey::Char('o'))),
            Hint::new("r", "refresh", Some(FileKey::Char('r'))),
            Hint::new(".", "hidden", Some(FileKey::Char('.'))),
            Hint::new("/", "filter", Some(FileKey::Char('/'))),
            Hint::new("q", "close", Some(FileKey::Char('q'))),
        ]
    }
}

/// Where a list row is drawn.
#[derive(Clone, Copy)]
pub(super) struct RowPlace {
    pub(super) rect: Rect,
    pub(super) y: u16,
    pub(super) cursor: bool,
    pub(super) active: bool,
    pub(super) columns: Columns,
}

/// A key hint of the footer.
pub(super) struct Hint {
    pub(super) label: &'static str,
    pub(super) text: String,
    pub(super) key: Option<FileKey>,
}

impl Hint {
    pub(super) fn new(label: &'static str, text: &str, key: Option<FileKey>) -> Hint {
        Hint { label, text: text.to_owned(), key }
    }

    pub(super) fn width(&self) -> u16 {
        u16::try_from(text_width(self.label) + 1 + text_width(&self.text)).unwrap_or(u16::MAX)
    }
}

/// Column ranges (relative to the area) of the hints that fit in `width`, with their index.
pub(super) fn footer_layout(hints: &[Hint], width: u16) -> Vec<(u16, u16, usize)> {
    let mut placed = Vec::new();
    let mut x: u16 = 1;
    for (index, hint) in hints.iter().enumerate() {
        let end = x.saturating_add(hint.width());
        if end > width {
            break;
        }
        placed.push((x, end, index));
        x = end.saturating_add(3);
    }
    placed
}

/// Widths of a panel's columns: ` name size date`.
#[derive(Clone, Copy)]
pub(super) struct Columns {
    pub(super) name: usize,
    pub(super) size: bool,
    pub(super) date: bool,
}

impl Columns {
    pub(super) fn new(width: usize) -> Columns {
        let inner = width.saturating_sub(1);
        let with_date = 1 + SIZE_WIDTH + 1 + DATE_WIDTH + 1;
        let with_size = 1 + SIZE_WIDTH + 1;
        if inner >= MIN_NAME_WIDTH + with_date {
            Columns { name: inner - with_date, size: true, date: true }
        } else if inner >= MIN_NAME_WIDTH + with_size {
            Columns { name: inner - with_size, size: true, date: false }
        } else {
            Columns { name: inner, size: false, date: false }
        }
    }

    /// Column of the size, relative to the panel.
    pub(super) fn size_x(&self) -> Option<u16> {
        self.size.then(|| u16::try_from(1 + self.name + 1).unwrap_or(u16::MAX))
    }

    pub(super) fn date_x(&self) -> Option<u16> {
        self.date.then(|| u16::try_from(1 + self.name + 1 + SIZE_WIDTH + 1).unwrap_or(u16::MAX))
    }
}

/// Rows and panel rectangles of the pane for one area.
pub(super) struct Geometry {
    pub(super) area: Rect,
    pub(super) panels: [Rect; 2],
    pub(super) separator: Option<u16>,
    pub(super) header: Option<u16>,
    pub(super) columns: Option<u16>,
    /// First row and height of the listings.
    pub(super) list: (u16, u16),
    pub(super) status: Option<u16>,
    pub(super) footer: Option<u16>,
}

impl Geometry {
    pub(super) fn side_at(&self, col: u16) -> Option<Side> {
        let inside = |rect: Rect| col >= rect.left() && col < rect.right();
        if inside(self.panels[0]) {
            Some(Side::Left)
        } else if inside(self.panels[1]) {
            Some(Side::Right)
        } else {
            None
        }
    }
}

/// Lays out `area`: rows are dropped from the least useful as it shrinks, and
/// narrow areas show only the `active` panel.
pub(super) fn geometry(area: Rect, active: Side) -> Geometry {
    let height = area.height;
    let mut top = area.y;
    let mut bottom = area.bottom();
    let header = (height >= 1).then(|| {
        top += 1;
        area.y
    });
    let footer = (height >= 3).then(|| {
        bottom -= 1;
        bottom
    });
    let status = (height >= 5).then(|| {
        bottom -= 1;
        bottom
    });
    let columns = (height >= 7).then(|| {
        top += 1;
        top - 1
    });
    let list = (top, bottom.saturating_sub(top));
    let panel_height = footer.map_or(height, |y| y - area.y);
    let (panels, separator) = if area.width >= MIN_SPLIT_WIDTH {
        let left = (area.width - 1) / 2;
        let right = area.width - 1 - left;
        let separator = area.x + left;
        (
            [Rect::new(area.x, area.y, left, panel_height), Rect::new(separator + 1, area.y, right, panel_height)],
            Some(separator),
        )
    } else {
        let full = Rect::new(area.x, area.y, area.width, panel_height);
        let none = Rect::new(area.x, area.y, 0, 0);
        (if active == Side::Left { [full, none] } else { [none, full] }, None)
    };
    Geometry { area, panels, separator, header, columns, list, status, footer }
}

/// Writes `text` at (`x`, `y`) within `clip`, at most `max_width` columns;
/// returns the column after the text.
pub(super) fn put(buf: &mut Buffer, clip: Rect, x: u16, y: u16, text: &str, max_width: usize, style: Style) -> u16 {
    let clip = clip.intersection(buf.area);
    if y < clip.top() || y >= clip.bottom() || x < clip.left() || x >= clip.right() {
        return x;
    }
    let max_width = max_width.min(usize::from(clip.right() - x));
    buf.set_stringn(x, y, text, max_width, style).0
}

/// Rows of `area` as text, skipping the cells hidden by wide characters.
pub(super) fn buffer_rows(buf: &Buffer, area: Rect) -> Vec<String> {
    let area = area.intersection(buf.area);
    (area.top()..area.bottom())
        .map(|y| {
            let mut row = String::new();
            let mut x = area.left();
            while x < area.right() {
                let symbol = buf[(x, y)].symbol();
                row.push_str(symbol);
                let width = symbol.chars().map(|c| c.width().unwrap_or(0)).sum::<usize>();
                x = x.saturating_add(u16::try_from(width.max(1)).unwrap_or(1));
            }
            row.trim_end().to_owned()
        })
        .collect()
}
