//! What a terminal pane shows: scrolling, selection, copy mode, search, its text, and drawing
//! the emulator's cells into a buffer.

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::search::{Match, RegexSearch};
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vi_mode::ViMotion;
use alacritty_terminal::vte::ansi::{Color as AColor, CursorShape as ACursorShape, NamedColor, Rgb};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};

use super::*;
use crate::ansi::{Cursor, CursorShape};

impl Pane {
    pub fn scroll(&mut self, lines: i32) {
        match &mut self.content {
            Content::Term(t) => {
                if lines > 0 {
                    t.reveal(t.term.grid().display_offset() + lines as usize);
                }
                t.term.scroll_display(Scroll::Delta(lines));
                if t.term.grid().display_offset() == 0 {
                    t.settle();
                }
            }
            Content::Preview(p) => p.scroll(-lines),
            Content::Files(f) => f.scroll_active(-lines),
        }
    }

    pub fn scroll_page(&mut self, up: bool) {
        match &mut self.content {
            Content::Term(t) => {
                if up {
                    t.reveal(t.term.grid().display_offset() + t.term.screen_lines());
                }
                t.term.scroll_display(if up { Scroll::PageUp } else { Scroll::PageDown });
            }
            Content::Preview(p) => {
                if up {
                    p.page_up()
                } else {
                    p.page_down()
                }
            }
            Content::Files(f) => {
                f.key(if up { crate::files::FileKey::PageUp } else { crate::files::FileKey::PageDown });
            }
        }
    }

    pub fn scroll_to_bottom(&mut self) {
        match &mut self.content {
            Content::Term(t) => {
                t.term.scroll_display(Scroll::Bottom);
                t.settle();
            }
            Content::Preview(p) => p.bottom(),
            Content::Files(f) => {
                f.key(crate::files::FileKey::End);
            }
        }
    }

    pub fn scroll_to_top(&mut self) {
        match &mut self.content {
            Content::Term(t) => {
                t.reveal_all();
                t.term.scroll_display(Scroll::Top);
            }
            Content::Preview(p) => p.top(),
            Content::Files(f) => {
                f.key(crate::files::FileKey::Home);
            }
        }
    }

    pub fn display_offset(&self) -> usize {
        self.term().map(|t| t.term.grid().display_offset()).unwrap_or(0)
    }

    /// Start a mouse selection at pane-relative cell (col, row).
    pub fn selection_start(&mut self, col: u16, row: u16, kind: SelectionType) {
        if let Some(t) = self.term_mut() {
            let p = t.viewport_point(col, row);
            t.term.selection = Some(Selection::new(kind, p, Side::Left));
            if kind != SelectionType::Simple
                && let Some(sel) = &mut t.term.selection
            {
                sel.update(p, Side::Right);
            }
        }
    }

    pub fn selection_update(&mut self, col: u16, row: u16) {
        if let Some(t) = self.term_mut() {
            let p = t.viewport_point(col, row);
            if let Some(sel) = &mut t.term.selection {
                sel.update(p, Side::Right);
            }
        }
    }

    pub fn selection_text(&self) -> Option<String> {
        self.term()?.term.selection_to_string().filter(|s| !s.is_empty())
    }

    /// The rows on screen (as scrolled), with the column of each character, for hints.
    pub fn screen_rows(&self) -> Vec<crate::hints::Row> {
        let Some(t) = self.term() else { return Vec::new() };
        let grid = t.term.grid();
        let offset = grid.display_offset() as i32;
        (0..grid.screen_lines() as i32)
            .map(|r| {
                let row = &grid[Line(r - offset)];
                let mut out = crate::hints::Row::default();
                for col in 0..grid.columns() {
                    let cell = &row[Column(col)];
                    if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                        continue;
                    }
                    let index = out.cols.len();
                    if let Some(link) = cell.hyperlink() {
                        let uri = link.uri().to_string();
                        match out.links.last_mut() {
                            Some((_, last, u)) if *u == uri && *last + 1 == index => *last = index,
                            _ => out.links.push((index, index, uri)),
                        }
                    }
                    out.text.push(cell.c);
                    out.cols.push(col as u16);
                }
                out
            })
            .collect()
    }

    /// The link under a cell: an OSC 8 hyperlink, or a URL written in the text.
    pub fn url_at(&self, col: u16, row: u16) -> Option<String> {
        let t = self.term()?;
        let p = t.viewport_point(col, row);
        let grid = t.term.grid();
        if let Some(h) = grid[p.line][p.column].hyperlink() {
            return Some(h.uri().to_string());
        }
        let (line, index) = row_chars(&t.term, p.line);
        url_in_text(&line, *index.get(p.column.0)?)
    }

    pub fn has_selection(&self) -> bool {
        self.term().is_some_and(|t| t.term.selection.is_some())
    }

    pub fn clear_selection(&mut self) {
        if let Some(t) = self.term_mut() {
            t.term.selection = None;
        }
    }

    /// Keyboard copy mode: a vi-style cursor that moves over the screen and scrollback.
    pub fn copy_mode(&mut self, on: bool) {
        if let Some(t) = self.term_mut()
            && t.term.mode().contains(TermMode::VI) != on
        {
            t.term.toggle_vi_mode();
            if !on {
                t.term.selection = None;
                t.term.scroll_display(Scroll::Bottom);
                t.settle();
            }
        }
    }

    pub fn in_copy_mode(&self) -> bool {
        self.term().is_some_and(|t| t.term.mode().contains(TermMode::VI))
    }

    /// Move the copy-mode cursor, keeping it in view and extending the selection.
    pub fn vi_motion(&mut self, motion: ViMotion) {
        if let Some(t) = self.term_mut() {
            // A screen of history above the cursor, for the motion to go to.
            let mut more = t.term.screen_lines();
            t.reveal((-t.term.vi_mode_cursor.point.line.0).max(0) as usize + more);
            let (cursor, selection) = (t.term.vi_mode_cursor, t.term.selection.clone());
            loop {
                t.term.vi_motion(motion);
                // Stopped at the top of what the emulator holds: the history goes on above.
                if t.term.vi_mode_cursor.point.line > t.term.topmost_line()
                    || !t.reveal(t.term.grid().history_size() + more)
                {
                    break;
                }
                t.term.vi_mode_cursor = cursor;
                t.term.selection = selection.clone();
                more *= 4;
            }
            let point = t.term.vi_mode_cursor.point;
            t.term.scroll_to_point(point);
        }
    }

    /// Move the copy-mode cursor by whole screens or half screens (negative = up).
    pub fn vi_scroll(&mut self, lines: i32) {
        if let Some(t) = self.term_mut() {
            if lines > 0 {
                t.reveal(t.term.grid().display_offset() + lines as usize);
            }
            t.term.scroll_display(Scroll::Delta(lines));
            let screen = t.term.screen_lines() as i32;
            let offset = t.term.grid().display_offset() as i32;
            let mut p = t.term.vi_mode_cursor.point;
            p.line = Line((p.line.0 - lines).clamp(-offset, screen - 1 - offset));
            t.term.vi_goto_point(p);
        }
    }

    /// Top or bottom of the scrollback.
    pub fn vi_edge(&mut self, top: bool) {
        if let Some(t) = self.term_mut() {
            if top {
                t.reveal_all();
            }
            let line = if top { t.term.topmost_line() } else { t.term.bottommost_line() };
            let p = Point::new(line, Column(0));
            t.term.vi_goto_point(p);
            t.term.scroll_to_point(p);
        }
    }

    /// Start, change or drop the copy-mode selection at the cursor.
    pub fn vi_select(&mut self, kind: SelectionType) {
        if let Some(t) = self.term_mut() {
            let p = t.term.vi_mode_cursor.point;
            match &t.term.selection {
                Some(sel) if sel.ty == kind => t.term.selection = None,
                Some(sel) => {
                    let mut sel = sel.clone();
                    sel.ty = kind;
                    t.term.selection = Some(sel);
                }
                None => {
                    let mut sel = Selection::new(kind, p, Side::Left);
                    sel.update(p, Side::Right);
                    t.term.selection = Some(sel);
                }
            }
        }
    }

    /// Search the scrollback for `query` (smart case: case-insensitive unless it has capitals),
    /// starting from the bottom. Returns whether something matched.
    pub fn search(&mut self, query: &str) -> bool {
        self.search = None;
        self.clear_selection();
        if query.is_empty() || self.term().is_none() {
            return false;
        }
        let pattern = regex_escape(query);
        let Ok(regex) = RegexSearch::new(&pattern) else { return false };
        // The archived history is searched as the emulator searches its own: smart case.
        let archived =
            regex::RegexBuilder::new(&pattern).case_insensitive(!query.chars().any(char::is_uppercase)).build();
        let Ok(archived) = archived else { return false };
        self.search = Some(SearchState { regex, archived, current: None });
        self.search_next(true)
    }

    /// Next match: `older` goes up the scrollback, otherwise down. Wraps around.
    pub fn search_next(&mut self, older: bool) -> bool {
        let Content::Term(t) = &mut self.content else { return false };
        let Some(state) = &mut self.search else { return false };
        // A match the text has since scrolled away with is no place to start from.
        let term = &t.term;
        let on_grid = |p: &Point| p.line >= term.topmost_line() && p.line <= term.bottommost_line();
        if state.current.as_ref().is_some_and(|m| !on_grid(m.start()) || !on_grid(m.end())) {
            state.current = None;
        }
        // A match on the first line the emulator holds: the line before is in the archive.
        if older && state.current.as_ref().is_some_and(|m| m.start().line == t.term.topmost_line()) {
            t.reveal(t.term.grid().history_size() + 1);
        }
        let term = &mut t.term;
        let origin = match &state.current {
            Some(m) if older => *m.start(),
            Some(m) => *m.end(),
            None => {
                let bottom = Line(term.screen_lines() as i32 - 1);
                Point::new(bottom, Column(term.columns().saturating_sub(1)))
            }
        };
        let (dir, side) = if older { (Direction::Left, Side::Left) } else { (Direction::Right, Side::Right) };
        // Step off the current match so the same one is not found again.
        let stepped = match (&state.current, older) {
            (Some(_), true) => origin.sub(term, alacritty_terminal::index::Boundary::None, 1),
            (Some(_), false) => origin.add(term, alacritty_terminal::index::Boundary::None, 1),
            (None, _) => origin,
        };
        // Stepping off the last cell goes round to the first.
        let round = if older { stepped > origin } else { stepped < origin };
        let origin = stepped;
        let mut found = term.search_next(&mut state.regex, origin, dir, side, None);
        // Before going round to the other end, the lines in the archive, which come before the
        // emulator's.
        let wrapped =
            round || found.as_ref().is_none_or(|m| if older { *m.start() > origin } else { *m.end() < origin });
        if wrapped && let Some(lines) = archived_match(&t.archive.lines, &state.archived, older) {
            t.reveal_lines(lines);
            let term = &mut t.term;
            let from = if older { origin } else { Point::new(term.topmost_line(), Column(0)) };
            found = term.search_next(&mut state.regex, from, dir, side, None).or(found);
        }
        let term = &mut t.term;
        let Some(m) = found else {
            state.current = None;
            return false;
        };
        let mut sel = Selection::new(SelectionType::Simple, *m.start(), Side::Left);
        sel.update(*m.end(), Side::Right);
        term.selection = Some(sel);
        if term.mode().contains(TermMode::VI) {
            term.vi_goto_point(*m.start());
        }
        term.scroll_to_point(*m.start());
        // Keep the match near the middle of the view rather than on its edge.
        let screen = term.screen_lines() as i32;
        let offset = term.grid().display_offset() as i32;
        let row = m.start().line.0 + offset;
        let delta = screen / 2 - row;
        if row < screen / 3 || row > screen * 2 / 3 {
            term.scroll_display(Scroll::Delta(delta));
        }
        state.current = Some(m);
        true
    }

    pub fn end_search(&mut self) {
        self.search = None;
    }

    /// Plain text of the screen (or the last `lines` lines including scrollback).
    pub fn text(&self, lines: Option<usize>) -> String {
        match &self.content {
            Content::Preview(p) => p.visible_text(),
            Content::Files(f) => f.visible_text(),
            Content::Term(t) => {
                let grid = t.term.grid();
                let screen = grid.screen_lines() as i32;
                let history = grid.history_size() as i32;
                let (start, end, archived) = match lines {
                    None => (0, screen, 0),
                    Some(n) => {
                        let bottom = last_nonblank_line(&t.term).unwrap_or(0) + 1;
                        let n = n.min(i32::MAX as usize) as i32;
                        // Rows older than the emulator's come from the archive.
                        ((bottom - n).max(-history), bottom, (n - bottom - history).max(0) as usize)
                    }
                };
                let rows = (start..end).map(|l| row_text(&t.term, Line(l)));
                join_rows(t.archived_rows(archived).into_iter().chain(rows))
            }
        }
    }

    /// Render the pane content into `area` (which matches the pane size).
    pub fn render(&mut self, area: Rect, buf: &mut Buffer, links: &mut crate::ansi::Links) {
        match &mut self.content {
            Content::Preview(p) => p.render(area, buf),
            Content::Files(f) => f.render(area, buf),
            Content::Term(t) => render_term(&t.term, area, buf, Some(links)),
        }
    }

    /// Cursor position in `area` coordinates if visible.
    pub fn cursor(&self, area: Rect) -> Option<Cursor> {
        let t = self.term()?;
        let content = t.term.renderable_content();
        if content.mode.contains(TermMode::VI) {
            // Copy mode: the vi cursor, wherever the view is scrolled.
            let p = content.cursor.point;
            let row = p.line.0 + content.display_offset as i32;
            let (x, y) = (p.column.0 as u16, row);
            if y < 0 || x >= area.width || y as u16 >= area.height {
                return None;
            }
            return Some(Cursor { x: area.x + x, y: area.y + y as u16, shape: CursorShape::Block, blinking: false });
        }
        if content.display_offset != 0 {
            return None;
        }
        let shape = match content.cursor.shape {
            ACursorShape::Hidden => return None,
            ACursorShape::Block | ACursorShape::HollowBlock => CursorShape::Block,
            ACursorShape::Underline => CursorShape::Underline,
            ACursorShape::Beam => CursorShape::Bar,
        };
        let p = content.cursor.point;
        let (x, y) = (p.column.0 as u16, p.line.0 as u16);
        if x >= area.width || y >= area.height {
            return None;
        }
        Some(Cursor { x: area.x + x, y: area.y + y, shape, blinking: t.term.cursor_style().blinking })
    }
}

pub(super) struct SearchState {
    regex: RegexSearch,
    /// The same, for the archived lines.
    archived: regex::Regex,
    current: Option<Match>,
}

/// How many of the newest archived lines to bring back for the nearest match there: the newest
/// line that matches when going to older lines, the oldest otherwise (going round to the top).
fn archived_match(lines: &super::history::History, regex: &regex::Regex, older: bool) -> Option<usize> {
    let hit = |line: super::history::Line| regex.is_match(&line.searched());
    if older {
        lines.newest_first().position(hit).map(|i| i + 1)
    } else {
        lines.oldest_first().position(hit).map(|i| lines.len() - i)
    }
}

/// Escape regex metacharacters so a plain query matches literally.
pub(super) fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.+*?()|[]{}^$#&-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A URL around character column `col` of `line`, if any.
pub fn url_in_text(line: &str, col: usize) -> Option<String> {
    let chars: Vec<char> = line.chars().collect();
    if col >= chars.len() {
        return None;
    }
    let stop = |c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '`' | '|');
    if stop(chars[col]) {
        return None;
    }
    let mut start = col;
    while start > 0 && !stop(chars[start - 1]) {
        start -= 1;
    }
    let mut end = col;
    while end + 1 < chars.len() && !stop(chars[end + 1]) {
        end += 1;
    }
    let word: String = chars[start..=end].iter().collect();
    let begin = ["https://", "http://", "file://", "ftp://"].iter().filter_map(|s| word.find(s)).min()?;
    let url = word[begin..].trim_end_matches(['.', ',', ';', ':', ')', ']', '}', '!', '?']);
    (url.len() > 8).then(|| url.to_string())
}

/// The text of grid lines `start..end`, a line each, without the blank ones at the end.
pub(super) fn lines_text<T>(term: &Term<T>, start: i32, end: i32) -> String {
    join_rows((start..end).map(|l| row_text(term, Line(l))))
}

/// Rows' text, a line each, without the blank ones at the end.
fn join_rows(rows: impl Iterator<Item = String>) -> String {
    let mut out = String::new();
    for row in rows {
        out.push_str(&row);
        out.push('\n');
    }
    let trimmed = out.trim_end_matches('\n').len();
    out.truncate(trimmed);
    out.push('\n');
    out
}

pub(super) fn row_text<T>(term: &Term<T>, line: Line) -> String {
    row_chars(term, line).0.trim_end().to_string()
}

/// The text of a row, and for each column the index (in characters) of the character drawn
/// there: a wide character covers two columns, combining characters none.
pub(super) fn row_chars<T>(term: &Term<T>, line: Line) -> (String, Vec<usize>) {
    let grid = term.grid();
    let row = &grid[line];
    let mut s = String::new();
    let mut index = Vec::with_capacity(grid.columns());
    let (mut n, mut last) = (0, 0);
    for col in 0..grid.columns() {
        let cell = &row[Column(col)];
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            index.push(last);
            continue;
        }
        last = n;
        index.push(n);
        n += 1;
        // An image's cells read as blank.
        if cell.c == crate::graphics::PLACEHOLDER {
            s.push(' ');
            continue;
        }
        s.push(cell.c);
        if let Some(zw) = cell.zerowidth() {
            s.extend(zw);
            n += zw.len();
        }
    }
    (s, index)
}

pub(super) fn last_nonblank_line<T>(term: &Term<T>) -> Option<i32> {
    let screen = term.grid().screen_lines() as i32;
    (0..screen).rev().find(|l| !row_text(term, Line(*l)).is_empty())
}

/// Copy the visible grid of `term` into `buf` at `area`.
pub fn render_term<T: EventListener>(
    term: &Term<T>,
    area: Rect,
    buf: &mut Buffer,
    mut links: Option<&mut crate::ansi::Links>,
) {
    let mut uris: std::collections::HashMap<String, std::sync::Arc<str>> = Default::default();
    let content = term.renderable_content();
    let offset = content.display_offset as i32;
    let colors = content.colors;
    let selection = content.selection;
    for indexed in content.display_iter {
        let point = indexed.point;
        let x = point.column.0 as u16;
        let y = (point.line.0 + offset) as u16;
        if x >= area.width || y >= area.height {
            continue;
        }
        let cell = indexed.cell;
        if let Some(links) = links.as_deref_mut()
            && let Some(h) = cell.hyperlink()
        {
            let uri = h.uri();
            // Only well-formed, printable links are passed on.
            if uri.len() <= 2048 && !uri.chars().any(|c| c.is_control()) {
                let arc = uris.entry(uri.to_string()).or_insert_with(|| std::sync::Arc::from(uri)).clone();
                links.insert((area.x + x, area.y + y), arc);
            }
        }
        let Some(out) = buf.cell_mut((area.x + x, area.y + y)) else { continue };
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            out.set_symbol("");
            continue;
        }
        if cell.flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
            out.set_symbol(" ");
        } else if let Some(zw) = cell.zerowidth() {
            let mut s = String::with_capacity(8);
            s.push(cell.c);
            s.extend(zw);
            out.set_symbol(&s);
        } else if cell.c == '\0' || cell.c == '\t' {
            out.set_symbol(" ");
        } else {
            out.set_char(cell.c);
        }
        // A wide char at the last column cannot be drawn.
        if cell.flags.contains(Flags::WIDE_CHAR) && x + 1 >= area.width {
            out.set_symbol(" ");
        }
        out.fg = convert_color(cell.fg, colors);
        out.bg = convert_color(cell.bg, colors);
        out.underline_color = cell.underline_color().map(|c| convert_color(c, colors)).unwrap_or(Color::Reset);
        let mut m = Modifier::empty();
        let f = cell.flags;
        if f.contains(Flags::BOLD) {
            m |= Modifier::BOLD;
        }
        if f.contains(Flags::DIM) {
            m |= Modifier::DIM;
        }
        if f.contains(Flags::ITALIC) {
            m |= Modifier::ITALIC;
        }
        if f.intersects(Flags::ALL_UNDERLINES) {
            m |= Modifier::UNDERLINED;
        }
        if f.contains(Flags::STRIKEOUT) {
            m |= Modifier::CROSSED_OUT;
        }
        if f.contains(Flags::HIDDEN) {
            m |= Modifier::HIDDEN;
        }
        let mut inverse = f.contains(Flags::INVERSE);
        if let Some(sel) = &selection
            && sel.contains(point)
        {
            inverse = !inverse;
        }
        if inverse {
            m |= Modifier::REVERSED;
        }
        out.modifier = m;
    }
}

pub(super) fn convert_color(c: AColor, colors: &alacritty_terminal::term::color::Colors) -> Color {
    match c {
        AColor::Spec(Rgb { r, g, b }) => Color::Rgb(r, g, b),
        AColor::Indexed(i) => match colors[i as usize] {
            Some(Rgb { r, g, b }) => Color::Rgb(r, g, b),
            None => Color::Indexed(i),
        },
        AColor::Named(n) => {
            let idx = n as usize;
            if let Some(Rgb { r, g, b }) = colors[idx] {
                return Color::Rgb(r, g, b);
            }
            match n {
                NamedColor::Black | NamedColor::DimBlack => Color::Black,
                NamedColor::Red | NamedColor::DimRed => Color::Red,
                NamedColor::Green | NamedColor::DimGreen => Color::Green,
                NamedColor::Yellow | NamedColor::DimYellow => Color::Yellow,
                NamedColor::Blue | NamedColor::DimBlue => Color::Blue,
                NamedColor::Magenta | NamedColor::DimMagenta => Color::Magenta,
                NamedColor::Cyan | NamedColor::DimCyan => Color::Cyan,
                NamedColor::White | NamedColor::DimWhite => Color::Gray,
                NamedColor::BrightBlack => Color::DarkGray,
                NamedColor::BrightRed => Color::LightRed,
                NamedColor::BrightGreen => Color::LightGreen,
                NamedColor::BrightYellow => Color::LightYellow,
                NamedColor::BrightBlue => Color::LightBlue,
                NamedColor::BrightMagenta => Color::LightMagenta,
                NamedColor::BrightCyan => Color::LightCyan,
                NamedColor::BrightWhite => Color::White,
                _ => Color::Reset,
            }
        }
    }
}

pub(super) static OUTER_COLORS: std::sync::Mutex<Option<(Rgb, Rgb)>> = std::sync::Mutex::new(None);

/// Record the outer terminal's foreground/background (`rgb:rrrr/gggg/bbbb` replies to OSC 10/11),
/// so programs querying a pane's colours get the real ones.
pub fn set_outer_colors(fg: &str, bg: &str) {
    let (Some(fg), Some(bg)) = (parse_xcolor(fg), parse_xcolor(bg)) else { return };
    if let Ok(mut c) = OUTER_COLORS.lock() {
        *c = Some((fg, bg));
    }
}

pub(super) fn parse_xcolor(s: &str) -> Option<Rgb> {
    let rest = s.strip_prefix("rgb:")?;
    let mut it = rest.split('/').map(|c| {
        let v = u32::from_str_radix(c, 16).ok()?;
        // Scale 1–4 hex digits to 8 bits.
        let max = (1u32 << (4 * c.len() as u32)) - 1;
        Some((v * 255 / max.max(1)) as u8)
    });
    Some(Rgb { r: it.next()??, g: it.next()??, b: it.next()?? })
}

/// Colour reported to programs that query the palette (OSC 4/10/11) when they never set it.
pub(super) fn default_color(index: usize) -> Rgb {
    if (index == 256 || index == 257)
        && let Some((fg, bg)) = OUTER_COLORS.lock().ok().and_then(|c| *c)
    {
        return if index == 256 { fg } else { bg };
    }
    const BASE: [(u8, u8, u8); 16] = [
        (0x1d, 0x1f, 0x21),
        (0xcc, 0x66, 0x66),
        (0xb5, 0xbd, 0x68),
        (0xf0, 0xc6, 0x74),
        (0x81, 0xa2, 0xbe),
        (0xb2, 0x94, 0xbb),
        (0x8a, 0xbe, 0xb7),
        (0xc5, 0xc8, 0xc6),
        (0x66, 0x66, 0x66),
        (0xd5, 0x4e, 0x53),
        (0xb9, 0xca, 0x4a),
        (0xe7, 0xc5, 0x47),
        (0x7a, 0xa6, 0xda),
        (0xc3, 0x97, 0xd8),
        (0x70, 0xc0, 0xb1),
        (0xea, 0xea, 0xea),
    ];
    let (r, g, b) = match index {
        0..16 => BASE[index],
        16..232 => {
            let i = index - 16;
            let v = |c: usize| if c == 0 { 0 } else { (c * 40 + 55) as u8 };
            (v(i / 36), v((i / 6) % 6), v(i % 6))
        }
        232..=255 => {
            let v = (8 + (index - 232) * 10) as u8;
            (v, v, v)
        }
        257 => BASE[0],
        _ => (0xd8, 0xd8, 0xd8),
    };
    Rgb { r, g, b }
}
