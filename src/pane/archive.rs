//! A pane's history in two parts. The emulator keeps the newest rows, as many as fit in the rows
//! it allocates for its history anyway, at full width; [`History`] keeps the older lines,
//! compact. Output moves rows from the first to the second as it pushes them up. They come back,
//! laid out at the pane's width then, when the view, the copy-mode cursor, a search or a
//! command's mark goes above what the emulator holds. Resizing reflows only what the emulator
//! holds.
//!
//! Rows move between the parts only above everything in use: the view, the selection and the
//! copy-mode cursor, with a screen to spare. Both parts together hold no more rows than the
//! pane's scrollback, as the emulator alone did, and at least all but a batch of them.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::{Dimensions, Grid, Row, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::{
    Attr, CharsetIndex, ClearMode, Color, CursorShape, CursorStyle, Handler, Hyperlink, KeyboardModes,
    KeyboardModesApplyBehavior, LineClearMode, Mode, ModifyOtherKeys, PrivateMode, Rgb, ScpCharPath, ScpUpdateMode,
    StandardCharset, TabulationClearMode, cursor_icon::CursorIcon,
};

use super::history::History;
use super::pty::TermPane;

/// The emulator allocates the rows of its history a thousand at a time: the history stays
/// within the first thousand.
const EMULATOR_ROWS: usize = 1000;
/// Rows of output between two moves while what is in use keeps rows in the emulator.
const BATCH: usize = 256;
/// Rows brought back at least at a time, so that scrolling line by line does not do it each line.
const PAGE: usize = 256;
/// A longer line moves in pieces of this many rows (as when it is saved).
const MAX_LINE_ROWS: usize = 1000;

pub(super) struct Archive {
    pub(super) lines: History,
    /// The most rows of history, both parts together: the pane's scrollback.
    limit: usize,
    /// Rows of history the emulator holds before the oldest move out, and how many stay then.
    high: usize,
    keep: usize,
    /// Above how many rows of history the next move is due: higher than `high` while rows in
    /// use stay in the emulator.
    due: usize,
}

impl Archive {
    pub(super) fn new(limit: usize, screen: usize) -> Archive {
        let mut archive = Archive { lines: History::default(), limit, high: 0, keep: 0, due: 0 };
        archive.fit(screen);
        archive
    }

    /// The screen has `screen` rows now.
    pub(super) fn fit(&mut self, screen: usize) {
        // Output adds up to a screen of rows before the next look at the history's length.
        self.high = EMULATOR_ROWS.saturating_sub(screen).max(screen).max(2 * BATCH);
        self.keep = self.high - BATCH;
        self.due = self.high;
    }

    /// The output about to be parsed may push up to a screen of rows into the history: move the
    /// oldest out if the emulator holds too many.
    fn make_room<T: EventListener>(&mut self, term: &mut Term<T>, seen: &mut i64) {
        self.compact(term, seen, self.keep);
        let history = term.grid().history_size();
        if history > self.high {
            // Rows in use stay: look again after a batch, and keep both parts within the limit.
            self.due = history + BATCH;
            self.lines.trim(self.limit.saturating_sub(self.due + term.screen_lines()));
        } else {
            self.due = self.high;
        }
    }

    /// Move the oldest lines of the emulator's history here, as long as the rows of each are all
    /// above those in use and `keep` rows stay. Returns the rows moved; `seen` (the history's
    /// length as last counted for the marks, see `commands.rs`) goes down by as many.
    pub(super) fn compact<T: EventListener>(&mut self, term: &mut Term<T>, seen: &mut i64, keep: usize) -> usize {
        if self.limit == 0 || term.mode().contains(TermMode::ALT_SCREEN) {
            return 0;
        }
        let grid = term.grid();
        let history = grid.history_size();
        let top = -(history as i32);
        let bound = first_in_use(term).min(-(keep as i32));
        if bound <= top {
            return 0;
        }
        let last = grid.columns() - 1;
        let screen = grid.screen_lines() as i32;
        let wraps = |l: i32| grid[Line(l)][Column(last)].flags.contains(Flags::WRAPLINE);
        let mut rows: Vec<&[Cell]> = Vec::new();
        let mut line = top;
        while line < bound {
            let mut end = line;
            while wraps(end) && end + 1 < screen && end + 1 - line < MAX_LINE_ROWS as i32 {
                end += 1;
            }
            if end >= bound {
                break;
            }
            rows.clear();
            rows.extend((line..=end).map(|l| &grid[Line(l)][..]));
            self.lines.push(&rows, wraps(end));
            line = end + 1;
        }
        let moved = (line - top) as usize;
        if moved > 0 {
            let grid = term.grid_mut();
            grid.update_history(history - moved);
            grid.update_history(self.limit);
            *seen -= moved as i64;
        }
        // Both parts within the scrollback while the emulator's fills up again, as tmux keeps
        // between nine tenths of its limit and all of it.
        self.lines.trim(self.limit.saturating_sub(self.high));
        moved
    }

    /// Bring back at least `wanted` rows (whole lines, the newest first) above the emulator's
    /// history, laid out at its width. Returns the rows brought back; `seen` goes up by as many.
    pub(super) fn page_in<T: EventListener>(&mut self, term: &mut Term<T>, seen: &mut i64, wanted: usize) -> usize {
        if self.lines.is_empty() || wanted == 0 || term.mode().contains(TermMode::ALT_SCREEN) {
            return 0;
        }
        let grid = term.grid();
        let width = grid.columns();
        let room = self.limit.saturating_sub(grid.history_size());
        let mut lines: Vec<Vec<Row<Cell>>> = Vec::new();
        let mut count = 0;
        while count < wanted
            && let Some(line) = self.lines.newest()
        {
            let mut rows = Vec::new();
            line.decode(width, &mut rows);
            if count + rows.len() > room {
                break;
            }
            count += rows.len();
            lines.push(rows);
            self.lines.drop_newest();
        }
        if count == 0 {
            return 0;
        }
        prepend(term.grid_mut(), lines.into_iter().rev().flatten().collect());
        *seen += count as i64;
        // They stay while output runs, until a batch of it.
        self.due = self.due.max(term.grid().history_size() + BATCH);
        count
    }
}

/// The topmost line in use: the view, the selection, the copy-mode cursor, and a screen above.
fn first_in_use<T>(term: &Term<T>) -> i32 {
    let grid = term.grid();
    let mut line = -(grid.display_offset() as i32);
    if let Some(range) = term.selection.as_ref().and_then(|s| s.to_range(term)) {
        line = line.min(range.start.line.0);
    }
    if term.mode().contains(TermMode::VI) {
        line = line.min(term.vi_mode_cursor.point.line.0);
    }
    line - grid.screen_lines() as i32
}

/// Put `rows` above the grid's history, keeping every line where it is (lines are counted from
/// the top of the screen) and the view on the text it shows.
fn prepend(grid: &mut Grid<Cell>, rows: Vec<Row<Cell>>) {
    let added = rows.len();
    let screen = grid.screen_lines() as i32;
    let history = grid.history_size();
    let offset = grid.display_offset();
    // The grid grows by as many rows, blank, at its bottom: what was there moves up.
    grid.scroll_up::<Color>(&(Line(0)..Line(screen)), added);
    let top = -((history + added) as i32);
    let old: Vec<Row<Cell>> = (top..screen - added as i32).map(|l| std::mem::take(&mut grid[Line(l)])).collect();
    for (l, row) in (top..).zip(rows.into_iter().chain(old)) {
        grid[Line(l)] = row;
    }
    let now = grid.display_offset();
    if now != offset {
        grid.scroll_display(Scroll::Delta(offset as i32 - now as i32));
    }
}

impl TermPane {
    /// Feed the emulator (see [`Feed`]).
    pub(super) fn parse(&mut self, bytes: &[u8]) {
        let mut feed = Feed { term: &mut self.term, archive: &mut self.archive, seen: &mut self.commands.history_seen };
        self.parser.advance(&mut feed, bytes);
    }

    /// Give the emulator the output a synchronized update holds.
    pub(super) fn stop_sync(&mut self) {
        let mut feed = Feed { term: &mut self.term, archive: &mut self.archive, seen: &mut self.commands.history_seen };
        self.parser.stop_sync(&mut feed);
    }

    /// Make the emulator's history hold at least `rows` rows, bringing rows back from the archive
    /// (a page at least). Whether any came back.
    pub(super) fn reveal(&mut self, rows: usize) -> bool {
        let history = self.term.grid().history_size();
        if rows <= history {
            return false;
        }
        let wanted = (rows - history).max(PAGE);
        self.archive.page_in(&mut self.term, &mut self.commands.history_seen, wanted) > 0
    }

    /// Bring back every archived line that fits in the scrollback.
    pub(super) fn reveal_all(&mut self) {
        self.archive.page_in(&mut self.term, &mut self.commands.history_seen, usize::MAX);
    }

    /// Bring back the newest `lines` archived lines.
    pub(super) fn reveal_lines(&mut self, lines: usize) {
        let width = self.term.columns();
        let rows: usize = self.archive.lines.newest_first().take(lines).map(|l| l.rows_at(width)).sum();
        self.archive.page_in(&mut self.term, &mut self.commands.history_seen, rows);
    }

    /// Move what nothing uses any more to the archive: rows brought back, once the view left
    /// them.
    pub(super) fn settle(&mut self) {
        if self.term.grid().history_size() > self.archive.high {
            let keep = self.archive.keep;
            self.archive.compact(&mut self.term, &mut self.commands.history_seen, keep);
            self.archive.due = self.archive.high;
        }
    }

    /// The screen changed size: rows it gains come from the history, brought back first.
    pub(super) fn before_resize(&mut self, rows: usize) {
        let screen = self.term.screen_lines();
        if rows > screen {
            self.reveal(rows - screen);
        }
    }

    /// The archived lines take as many rows at the new width as they will when they come back:
    /// the oldest go when they no longer fit, as the emulator's own do when it rewraps them.
    pub(super) fn after_resize(&mut self) {
        self.archive.fit(self.term.screen_lines());
        self.archive.lines.set_width(self.term.columns());
        self.settle();
        let history = self.term.grid().history_size();
        self.archive.lines.trim(self.archive.limit.saturating_sub(history.max(self.archive.high)));
    }

    /// The text of the archive's last `rows` rows at the pane's width, as grid rows read.
    pub(super) fn archived_rows(&self, rows: usize) -> Vec<String> {
        let width = self.term.columns();
        let mut lines = Vec::new();
        let mut count = 0;
        for line in self.archive.lines.newest_first() {
            if count >= rows {
                break;
            }
            let mut texts = Vec::new();
            line.row_texts(width, &mut texts);
            count += texts.len();
            lines.push(texts);
        }
        let mut texts: Vec<String> = lines.into_iter().rev().flatten().collect();
        texts.drain(..texts.len().saturating_sub(rows));
        texts
    }
}

/// The emulator as the parser drives it, with room made in its history before output pushes rows
/// into it, and the archive cleared with it.
pub(super) struct Feed<'a, T> {
    pub(super) term: &'a mut Term<T>,
    pub(super) archive: &'a mut Archive,
    pub(super) seen: &'a mut i64,
}

impl<T: EventListener> Feed<'_, T> {
    #[inline]
    fn room(&mut self) {
        if self.term.grid().history_size() > self.archive.due {
            self.archive.make_room(self.term, self.seen);
        }
    }
}

/// Handler methods passed on as they are.
macro_rules! forward {
    ($($name:ident($($arg:ident: $ty:ty),*);)*) => {
        $(
            #[inline]
            fn $name(&mut self, $($arg: $ty),*) {
                Handler::$name(&mut *self.term, $($arg),*)
            }
        )*
    };
}

impl<T: EventListener> Handler for Feed<'_, T> {
    #[inline]
    fn input(&mut self, c: char) {
        self.room();
        self.term.input(c);
    }

    #[inline]
    fn linefeed(&mut self) {
        self.room();
        self.term.linefeed();
    }

    #[inline]
    fn newline(&mut self) {
        self.room();
        self.term.newline();
    }

    fn scroll_up(&mut self, rows: usize) {
        self.room();
        self.term.scroll_up(rows);
    }

    fn delete_lines(&mut self, rows: usize) {
        self.room();
        self.term.delete_lines(rows);
    }

    fn clear_screen(&mut self, mode: ClearMode) {
        match mode {
            ClearMode::All => self.room(),
            ClearMode::Saved if !self.term.mode().contains(TermMode::ALT_SCREEN) => self.archive.lines.clear(),
            _ => {}
        }
        self.term.clear_screen(mode);
    }

    fn reset_state(&mut self) {
        self.archive.lines.clear();
        self.term.reset_state();
    }

    forward! {
        set_title(title: Option<String>);
        set_cursor_style(style: Option<CursorStyle>);
        set_cursor_shape(shape: CursorShape);
        goto(line: i32, col: usize);
        goto_line(line: i32);
        goto_col(col: usize);
        insert_blank(count: usize);
        move_up(rows: usize);
        move_down(rows: usize);
        identify_terminal(intermediate: Option<char>);
        device_status(arg: usize);
        move_forward(cols: usize);
        move_backward(cols: usize);
        move_down_and_cr(rows: usize);
        move_up_and_cr(rows: usize);
        put_tab(count: u16);
        backspace();
        carriage_return();
        bell();
        substitute();
        set_horizontal_tabstop();
        scroll_down(rows: usize);
        insert_blank_lines(rows: usize);
        erase_chars(count: usize);
        delete_chars(count: usize);
        move_backward_tabs(count: u16);
        move_forward_tabs(count: u16);
        save_cursor_position();
        restore_cursor_position();
        clear_line(mode: LineClearMode);
        clear_tabs(mode: TabulationClearMode);
        set_tabs(interval: u16);
        reverse_index();
        terminal_attribute(attr: Attr);
        set_mode(mode: Mode);
        unset_mode(mode: Mode);
        report_mode(mode: Mode);
        set_private_mode(mode: PrivateMode);
        unset_private_mode(mode: PrivateMode);
        report_private_mode(mode: PrivateMode);
        set_scrolling_region(top: usize, bottom: Option<usize>);
        set_keypad_application_mode();
        unset_keypad_application_mode();
        set_active_charset(index: CharsetIndex);
        configure_charset(index: CharsetIndex, charset: StandardCharset);
        set_color(index: usize, color: Rgb);
        dynamic_color_sequence(prefix: String, index: usize, terminator: &str);
        reset_color(index: usize);
        clipboard_store(clipboard: u8, data: &[u8]);
        clipboard_load(clipboard: u8, terminator: &str);
        decaln();
        push_title();
        pop_title();
        text_area_size_pixels();
        text_area_size_chars();
        set_hyperlink(link: Option<Hyperlink>);
        set_mouse_cursor_icon(icon: CursorIcon);
        report_keyboard_mode();
        push_keyboard_mode(mode: KeyboardModes);
        pop_keyboard_modes(count: u16);
        set_keyboard_mode(mode: KeyboardModes, behavior: KeyboardModesApplyBehavior);
        set_modify_other_keys(mode: ModifyOtherKeys);
        report_modify_other_keys();
        set_scp(char_path: ScpCharPath, update_mode: ScpUpdateMode);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use alacritty_terminal::index::Point;
    use alacritty_terminal::term::Config as TermConfig;
    use alacritty_terminal::vi_mode::ViMotion;
    use alacritty_terminal::vte::ansi::Processor;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    use super::super::pty::{Listener, Size};
    use super::super::screen::{lines_text, render_term};
    use super::super::{Pane, PaneEnv, PaneSpec};
    use super::*;

    const COLS: u16 = 30;
    const ROWS: u16 = 10;
    const SCROLLBACK: usize = 3000;

    /// A pane whose program prints nothing (the test feeds it), and an emulator that keeps its
    /// whole history, as a pane did before the archive, fed the same.
    struct Both {
        pane: Pane,
        reference: Term<Listener>,
        parser: Processor,
    }

    impl Both {
        fn new() -> Both {
            let (tx, _rx) = crossbeam_channel::bounded(64);
            let env = PaneEnv {
                socket: "/nonexistent".into(),
                session: "t".into(),
                shell: "sh".into(),
                scrollback: SCROLLBACK,
                graphics: Default::default(),
            };
            let spec = PaneSpec { command: vec!["sleep".into(), "30".into()], ..Default::default() };
            let pane = Pane::spawn(1, None, spec, COLS, ROWS, &env, &tx).unwrap();
            let config = TermConfig { scrolling_history: SCROLLBACK, kitty_keyboard: true, ..TermConfig::default() };
            let reference = Term::new(config, &Size { cols: COLS, rows: ROWS }, Listener::default());
            Both { pane, reference, parser: Processor::new() }
        }

        fn feed(&mut self, bytes: &[u8]) {
            self.pane.feed(bytes);
            self.parser.advance(&mut self.reference, bytes);
        }

        fn term(&self) -> &TermPane {
            self.pane.term().unwrap()
        }

        /// Rows of history the pane holds, both parts, at its width now.
        fn history(&self) -> usize {
            let t = self.term();
            let width = t.term.columns();
            let archived: usize = t.archive.lines.oldest_first().map(|l| l.rows_at(width)).sum();
            assert_eq!(archived, t.archive.lines.rows());
            archived + t.term.grid().history_size()
        }

        fn resize(&mut self, cols: u16, rows: u16) {
            self.pane.resize(cols, rows);
            self.reference.resize(Size { cols, rows });
        }
    }

    impl Drop for Both {
        fn drop(&mut self) {
            self.pane.kill();
        }
    }

    /// Lines of every kind: colours, wide characters, lines that wrap, links, coloured ends.
    fn sample(lines: usize) -> String {
        sample_of(lines, true)
    }

    /// The same, `fills` or not: the emulator rewraps a line whose end is coloured into more
    /// rows, where the archive colours the end of its new last row.
    fn sample_of(lines: usize, fills: bool) -> String {
        let mut s = String::new();
        for i in 0..lines {
            match i % 6 {
                0 => s.push_str(&format!("\x1b[3{}mcoloured {i}\x1b[0m plain\r\n", i % 8)),
                1 => s.push_str(&format!("日本語の行 {i} wide\r\n")),
                2 => s.push_str(&format!("{} long {i}\r\n", "x".repeat(70))),
                3 => s.push_str(&format!("\x1b]8;;https://example.org/{i}\x1b\\link {i}\x1b]8;;\x1b\\ after\r\n")),
                4 if fills => s.push_str(&format!("\x1b[44mbar {i}\x1b[K\x1b[0m\r\n")),
                _ => s.push_str(&format!("line {i}\r\n")),
            }
        }
        s
    }

    fn view(term: &Term<Listener>) -> Buffer {
        let area = Rect::new(0, 0, term.columns() as u16, term.screen_lines() as u16);
        let mut buf = Buffer::empty(area);
        let mut links = crate::ansi::Links::new();
        render_term(term, area, &mut buf, Some(&mut links));
        buf
    }

    /// The reference's last `rows` rows of history and its screen, as text.
    fn reference_text(r: &Term<Listener>, rows: usize) -> String {
        let bottom = super::super::screen::last_nonblank_line(r).unwrap_or(0) + 1;
        lines_text(r, -(rows as i32), bottom)
    }

    #[test]
    fn the_emulator_keeps_a_short_history_and_the_rest_reads_as_before() {
        let mut b = Both::new();
        b.feed(sample(4000).as_bytes());
        let t = b.term();
        assert!(t.term.grid().history_size() <= t.archive.high, "{}", t.term.grid().history_size());
        assert!(!t.archive.lines.is_empty());
        let kept = b.history();
        assert!((SCROLLBACK - BATCH..=SCROLLBACK).contains(&kept), "{kept}");
        // `kraal read --lines`: the same text, through the archive.
        let all = b.pane.text(Some(kept + ROWS as usize));
        assert_eq!(all, reference_text(&b.reference, kept));
        let bottom = super::super::screen::last_nonblank_line(&b.reference).unwrap_or(0) + 1;
        assert_eq!(b.pane.text(Some(5)), lines_text(&b.reference, bottom - 5, bottom));
        // Lines kept in a few bytes each, against a row of 24-byte cells each.
        let bytes = t.archive.lines.bytes();
        assert!(bytes < t.archive.lines.len() * 60, "{bytes} bytes for {} lines", t.archive.lines.len());
    }

    #[test]
    fn scrolling_back_shows_what_the_emulator_showed() {
        let mut b = Both::new();
        b.feed(sample(4000).as_bytes());
        let kept = b.history();
        // Line by line near the bottom, then page by page to the top, and back down.
        let mut offset = 0;
        for step in [1, 1, 3, 10, 200, 333, 1000, 5000] {
            b.pane.scroll(step);
            offset = (offset + step as usize).min(kept);
            b.reference.scroll_display(Scroll::Bottom);
            b.reference.scroll_display(Scroll::Delta(offset as i32));
            assert_eq!(b.pane.display_offset(), offset);
            assert_eq!(view(&b.term().term), view(&b.reference), "{offset} rows up");
        }
        // Back at the bottom, the rows that came back go again (as many as the scrollback holds
        // with the emulator's rows: a batch of the oldest may go).
        b.pane.scroll_to_bottom();
        assert!(b.term().term.grid().history_size() <= b.term().archive.high);
        let kept = b.history();
        assert!(kept >= SCROLLBACK - BATCH);
        b.pane.scroll_to_top();
        b.reference.scroll_display(Scroll::Bottom);
        b.reference.scroll_display(Scroll::Delta(kept as i32));
        assert_eq!(view(&b.term().term), view(&b.reference));
        for _ in 0..5 {
            b.pane.scroll_page(false);
            b.reference.scroll_display(Scroll::PageDown);
            assert_eq!(view(&b.term().term), view(&b.reference));
        }
        // Output while the view is up keeps it on its text, and the history within its bounds.
        b.feed(sample(50).as_bytes());
        assert_eq!(view(&b.term().term), view(&b.reference));
        b.pane.scroll_to_bottom();
        b.feed(b"x\r\n");
        assert!(b.term().term.grid().history_size() <= b.term().archive.high);
    }

    #[test]
    fn search_goes_through_the_archive_and_round() {
        let mut b = Both::new();
        b.feed(b"needle first\r\n");
        b.feed(sample(900).as_bytes());
        b.feed(b"needle second\r\n");
        b.feed(sample(900).as_bytes());
        b.feed(b"Needle third\r\n");
        b.feed(sample(100).as_bytes());
        assert!(b.pane.search("needle"));
        assert_eq!(b.pane.selection_text().as_deref(), Some("Needle"), "smart case: lower case matches both");
        let line = |b: &Both| {
            let t = b.term();
            let sel = t.term.selection.as_ref().unwrap().to_range(&t.term).unwrap();
            super::super::screen::row_text(&t.term, sel.start.line)
        };
        assert_eq!(line(&b), "Needle third");
        assert!(b.pane.search_next(true));
        assert_eq!(line(&b), "needle second");
        assert!(b.pane.search_next(true));
        assert_eq!(line(&b), "needle first");
        // Round to the newest.
        assert!(b.pane.search_next(true));
        assert_eq!(line(&b), "Needle third");
        // And the other way, round to the oldest.
        assert!(b.pane.search_next(false));
        assert_eq!(line(&b), "needle first");
        assert!(b.pane.search_next(false));
        assert_eq!(line(&b), "needle second");
        assert!(b.pane.search("Needle"));
        assert!(b.pane.search_next(true));
        assert_eq!(line(&b), "Needle third", "capitals match exactly");
        assert!(!b.pane.search("haystack"));
    }

    #[test]
    fn copy_mode_selects_across_the_archive() {
        let mut b = Both::new();
        b.feed(b"the very first line\r\n");
        b.feed(sample(3500).as_bytes());
        b.pane.copy_mode(true);
        b.pane.vi_edge(true);
        let t = b.term();
        let first = super::super::screen::row_text(&t.term, t.term.vi_mode_cursor.point.line);
        // The first line went out of the scrollback; the oldest kept is the reference's.
        let kept = b.history();
        let top = super::super::screen::row_text(&b.reference, Line(-(kept as i32)));
        assert_eq!(first, top);
        // From there, down past what the emulator held before, selected line by line.
        b.pane.vi_select(alacritty_terminal::selection::SelectionType::Lines);
        for _ in 0..2000 {
            b.pane.vi_motion(ViMotion::Down);
        }
        let selected = b.pane.selection_text().unwrap();
        let start = Point::new(Line(-(kept as i32)), Column(0));
        let end = Point::new(Line(-(kept as i32) + 2000), Column(COLS as usize - 1));
        let expected = b.reference.bounds_to_string(start, end);
        let differ = selected.lines().zip(expected.lines()).position(|(a, b)| a != b);
        assert_eq!(differ, None, "{:?}", differ.map(|i| (selected.lines().nth(i), expected.lines().nth(i))));
        // A selection of whole lines ends with the end of its last one.
        assert_eq!(selected, expected + "\n");
        // Up again, a line at a time, beyond the top of the emulator's part.
        b.pane.vi_select(alacritty_terminal::selection::SelectionType::Lines);
        b.pane.copy_mode(false);
        b.pane.copy_mode(true);
        for _ in 0..1500 {
            b.pane.vi_motion(ViMotion::Up);
        }
        let t = b.term();
        let at = t.term.vi_mode_cursor.point.line;
        assert_eq!(super::super::screen::row_text(&t.term, at), super::super::screen::row_text(&b.reference, at));
        b.pane.copy_mode(false);
        assert!(b.term().term.grid().history_size() <= b.term().archive.high, "given back when done");
    }

    /// A whole command as zsh with kraal's integration prints it.
    fn command(line: &str, output: &str, status: i32) -> String {
        format!("\x1b]133;A\x07$ \x1b]133;B\x07{line}\r\n\x1b]133;C\x07{output}\x1b]133;D;{status}\x07")
    }

    #[test]
    fn commands_and_their_output_are_found_in_the_archive() {
        let mut b = Both::new();
        b.feed(command("first", "one\r\n", 0).as_bytes());
        let long = sample(2000);
        b.feed(command("long", &long, 0).as_bytes());
        b.feed(command("last", "z\r\n", 0).as_bytes());
        b.feed(b"\x1b]133;A\x07$ ");
        b.pane.copy_mode(true);
        let cursor = |b: &Both| {
            let t = b.term();
            super::super::screen::row_text(&t.term, t.term.vi_mode_cursor.point.line)
        };
        assert!(b.pane.jump_to_prompt(true));
        assert_eq!(cursor(&b), "$ last");
        assert!(b.pane.jump_to_prompt(true));
        assert_eq!(cursor(&b), "$ long");
        assert!(b.pane.jump_to_prompt(true));
        assert_eq!(cursor(&b), "$ first");
        assert!(!b.pane.jump_to_prompt(true));
        assert!(b.pane.jump_to_prompt(false));
        assert_eq!(cursor(&b), "$ long");
        // The long output, whole, though most of it left the emulator: what the same selection
        // reads from the emulator that kept everything.
        assert!(b.pane.select_command(false));
        let selected = b.pane.selection_text().unwrap();
        b.reference.selection = b.term().term.selection.clone();
        assert_eq!(Some(selected.clone()), b.reference.selection_to_string());
        assert_eq!(selected.lines().count(), 2000);
        assert_eq!(selected.lines().next(), Some("coloured 0 plain"));
        b.pane.copy_mode(false);
        b.feed(command("again", &sample(1500), 0).as_bytes());
        let (text, output) = b.pane.last_command_output().unwrap();
        assert_eq!(text.as_deref(), Some("again"));
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 1500, "wrapped rows read as their line");
        assert_eq!(lines[0], "coloured 0 plain");
        assert_eq!(lines[1499], "line 1499");
    }

    #[test]
    fn saved_text_holds_the_archive_and_comes_back() {
        let mut b = Both::new();
        b.feed(sample(4000).as_bytes());
        let saved = b.pane.scrollback_ansi().unwrap();
        let kept = b.history();
        let reference = super::super::scrollback::encode(&b.reference, &History::default(), usize::MAX);
        // The same lines as the emulator alone would save, the oldest it kept first.
        let reference_lines: Vec<&[u8]> = reference.split(|c| *c == b'\n').collect();
        let saved_lines: Vec<&[u8]> = saved.split(|c| *c == b'\n').collect();
        assert!(saved_lines.len() >= kept / 2, "{} lines", saved_lines.len());
        assert_eq!(saved_lines[..], reference_lines[reference_lines.len() - saved_lines.len()..]);
        // A restart gives it back, through the archive.
        let mut fresh = Both::new();
        assert!(fresh.pane.restore_scrollback(&saved, None, SCROLLBACK));
        let text = fresh.pane.text(Some(SCROLLBACK + 20));
        let before = b.pane.text(Some(SCROLLBACK + 20));
        let restored: Vec<&str> = text.lines().collect();
        assert_eq!(restored[restored.len() - 1], "kraal: restored");
        let old: Vec<&str> = before.lines().collect();
        let tail = &restored[..restored.len() - 1];
        assert_eq!(tail[tail.len() - 100..], old[old.len() - 100..]);
        assert!(!fresh.term().archive.lines.is_empty());
    }

    #[test]
    fn clearing_the_history_clears_the_archive() {
        let mut b = Both::new();
        b.feed(sample(2000).as_bytes());
        assert!(!b.term().archive.lines.is_empty());
        b.feed(b"\x1b[H\x1b[2J\x1b[3J");
        assert!(b.term().archive.lines.is_empty());
        assert_eq!(b.pane.text(Some(100)), b.reference_text_all());
        b.feed(sample(2000).as_bytes());
        b.feed(b"\x1bc");
        assert!(b.term().archive.lines.is_empty());
        // Not from a full-screen program: its screen has no history.
        b.feed(sample(2000).as_bytes());
        b.feed(b"\x1b[?1049h\x1b[3J\x1b[?1049l");
        assert!(!b.term().archive.lines.is_empty());
    }

    impl Both {
        fn reference_text_all(&self) -> String {
            let bottom = super::super::screen::last_nonblank_line(&self.reference).unwrap_or(0) + 1;
            lines_text(&self.reference, -(self.reference.grid().history_size() as i32), bottom)
        }
    }

    #[test]
    fn a_new_width_rewraps_the_archive_when_it_is_read() {
        let mut b = Both::new();
        b.feed(sample_of(4000, false).as_bytes());
        for (cols, rows) in [(17, 10), (55, 14), (30, 6), (30, 10)] {
            b.resize(cols, rows);
            let t = b.term();
            assert!(t.term.grid().history_size() <= t.archive.high);
            let kept = b.history();
            let ours = b.pane.text(Some(kept + rows as usize));
            let reference = b.reference_text_all();
            // The emulator alone dropped its oldest rows as narrow rows took more: the newest agree.
            let ours: Vec<&str> = ours.lines().collect();
            let reference: Vec<&str> = reference.lines().collect();
            let n = ours.len().min(reference.len());
            assert!(n > 1000);
            assert_eq!(ours[ours.len() - n..], reference[reference.len() - n..], "{cols}x{rows}");
        }
        // Scrolled to the top, the rows come back at the width now.
        b.pane.scroll_to_top();
        let t = b.term();
        let top = t.term.topmost_line();
        let first = super::super::screen::row_text(&t.term, top);
        assert!(!first.is_empty());
    }

    #[test]
    fn the_emulator_is_driven_as_the_parser_drives_it_alone() {
        // Every kind of sequence, so that none is lost on the way.
        let corpus: &[u8] = b"\x1b]0;title\x07\x1b]2;other\x1b\\plain \x1b[1;3;4;9;31;42mstyled\x1b[0m\r\n\
            \x1b[5;10Hat\x1b[2;3fthere\x1b[A\x1b[B\x1b[C\x1b[D\x1b[2E\x1b[F\x1b[5G\x1b[3d\
            tab\tstop\x1bH\x1b[2g\x1b[3g\x1b[I\x1b[Z\x08\r\
            \x1b[2@\x1b[3P\x1b[4X\x1b[K\x1b[1K\x1b[2K\x1b[J\x1b[1J\
            \x1b[2;8r\x1b[3L\x1b[2M\x1b[2S\x1b[3T\x1bD\x1bE\x1bM\x1b[r\
            \x1b7\x1b[10;10H\x1b8\x1b[s\x1b[u\x1b#8\
            \x1b[4hins\x1b[4l\x1b[20h\n\x1b[20l\x1b[?6h\x1b[?6l\x1b[?7l\x1b[?7h\x1b[?25l\x1b[?25h\
            \x1b[?1h\x1b=\x1b[?1l\x1b>\x1b[?2004h\x1b[?1000h\x1b[?1006h\x1b[?1004h\
            \x1b(0qqq\x1b(B\x1b)0\x0eqq\x0f\
            \x1b]4;1;rgb:12/34/56\x07\x1b]104;1\x07\x1b]8;id=x;https://a.b\x1b\\ln\x1b]8;;\x1b\\\
            \x1b]52;c;aGk=\x07\x1b[22;0t\x1b[23;0t\x1b[6n\x1b[c\x1b[>c\x1b[5n\x1b[18t\x1b[14t\
            \x1b[3 q\x1b[?1049hALT\x1b[?1049l\x1b[>1u\x1b[?u\x1b[=3;1u\x1b[<u\x1b[>4;2m\x1b[?4m\
            \x1b[?2026h\x1b[?2026l\x1b[?1$p\x1b[4$p\x1b]10;?\x07\x1b]11;?\x07\x1b]12;#ff0000\x07\
            end\x1b[8;30H\xe6\x97\xa5\xe6\x9c\xac\x1b[1J\x1bc";
        let events = Rc::new(RefCell::new(Vec::new()));
        let size = Size { cols: COLS, rows: ROWS };
        let config = TermConfig { scrolling_history: 100, kitty_keyboard: true, ..TermConfig::default() };
        let mut plain = Term::new(config.clone(), &size, Listener(events.clone()));
        let fed_events = Rc::new(RefCell::new(Vec::new()));
        let mut fed = Term::new(config, &size, Listener(fed_events.clone()));
        let mut archive = Archive::new(100, ROWS as usize);
        let mut seen = 0;
        // Byte by byte through the archive, in one piece alone, and compared after each piece.
        let (mut p1, mut p2): (Processor, Processor) = (Processor::new(), Processor::new());
        for piece in corpus.split_inclusive(|b| *b == b'\x07' || *b == b'\\' || *b == b'm') {
            p1.advance(&mut plain, piece);
            for byte in piece {
                p2.advance(&mut Feed { term: &mut fed, archive: &mut archive, seen: &mut seen }, &[*byte]);
            }
            for l in -(plain.grid().history_size() as i32)..ROWS as i32 {
                assert_eq!(
                    plain.grid()[Line(l)],
                    fed.grid()[Line(l)],
                    "line {l} after {:?}",
                    String::from_utf8_lossy(piece)
                );
            }
            assert_eq!(plain.mode(), fed.mode());
            assert_eq!(plain.grid().cursor.point, fed.grid().cursor.point);
            assert_eq!(format!("{:?}", plain.colors()[1]), format!("{:?}", fed.colors()[1]));
        }
        let describe = |e: &Rc<RefCell<Vec<alacritty_terminal::event::Event>>>| {
            e.borrow().iter().map(|e| format!("{e:?}")).collect::<Vec<_>>()
        };
        assert_eq!(describe(&events), describe(&fed_events));
        assert!(!events.borrow().is_empty());
    }
}
