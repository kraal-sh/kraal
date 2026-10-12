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

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::{Dimensions, Grid, Row, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::{Term, TermMode};
use alacritty_terminal::vte::ansi::{
    Attr, CharsetIndex, ClearMode, Color, CursorShape, CursorStyle, Handler, Hyperlink, KeyboardModes,
    KeyboardModesApplyBehavior, LineClearMode, Mode, ModifyOtherKeys, NamedColor, PrivateMode, Rgb, ScpCharPath,
    ScpUpdateMode, StandardCharset, TabulationClearMode, cursor_icon::CursorIcon,
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
/// Output that would move more rows than the scrollback holds within this is a flood.
const FLOOD_WINDOW: Duration = Duration::from_secs(1);
/// A pause in the output this long ends a flood.
const CALM: Duration = Duration::from_millis(100);
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
    /// Output floods the pane: rows stay in the emulator, as many as the scrollback holds, until
    /// it stops. And when the rows moved recently started being counted, and how many.
    flood: bool,
    window: (Instant, usize),
    /// When output was last parsed.
    last: Instant,
    /// A flood's rows moved here, freeing the emulator's.
    freed: bool,
    extents: Extents,
}

/// For each row of the emulator's primary screen and history, how far its cells may differ from
/// its last one: past that, it holds the blank cells it was cleared with, and a row moving out
/// is read up to there only. Followed for what plain output does (text, line feeds, tabs, erasing
/// in a line); a row changed any other way is read whole.
#[derive(Default)]
struct Extents {
    screen: Vec<u16>,
    /// The history's rows, oldest first; read whole when this does not have as many.
    history: VecDeque<u16>,
    /// A scroll region may be set: lines then move in ways not followed.
    region: bool,
    /// Lines moved without this knowing how (the history was full): every row is read whole
    /// until the history's rows are counted again.
    lost: bool,
}

/// Read the row whole.
const WHOLE: u16 = u16::MAX;

impl Extents {
    fn reset(&mut self, screen: usize, history: usize) {
        self.screen = vec![WHOLE; screen];
        self.history = std::iter::repeat_n(WHOLE, history).collect();
        self.lost = false;
    }

    /// The `rows` at the top of the screen went into the history; the screen's lines moved up
    /// as many, unless they may have moved some other way.
    fn scrolled(&mut self, rows: usize, moved_up: bool) {
        let n = rows.min(self.screen.len());
        self.history.extend(&self.screen[..n]);
        self.history.extend(std::iter::repeat_n(WHOLE, rows - n));
        if moved_up && !self.region {
            self.screen.rotate_left(n);
            // New rows, cleared with one cell.
            let len = self.screen.len();
            self.screen[len - n..].fill(0);
        } else {
            self.screen.fill(WHOLE);
        }
    }

    fn set(&mut self, line: i32, end: u16) {
        if let Some(e) = usize::try_from(line).ok().and_then(|l| self.screen.get_mut(l)) {
            *e = end;
        }
    }

    /// Cells of `line` up to `end` (not included) were written.
    fn wrote(&mut self, line: i32, end: usize) {
        if let Some(e) = usize::try_from(line).ok().and_then(|l| self.screen.get_mut(l))
            && *e != WHOLE
        {
            *e = (*e).max(end.min(usize::from(WHOLE - 1)) as u16);
        }
    }
}

impl Archive {
    pub(super) fn new(limit: usize, screen: usize) -> Archive {
        let extents = Extents { screen: vec![0; screen], ..Extents::default() };
        let mut archive = Archive {
            lines: History::default(),
            limit,
            high: 0,
            keep: 0,
            due: 0,
            flood: false,
            window: (Instant::now(), 0),
            last: Instant::now(),
            freed: false,
            extents,
        };
        archive.fit(screen);
        archive
    }

    /// The grid changed in ways not followed (a resize, images): read every row whole.
    pub(super) fn forget_extents(&mut self, screen: usize, history: usize) {
        self.extents.reset(screen, history);
    }

    /// The screen has `screen` rows now.
    pub(super) fn fit(&mut self, screen: usize) {
        // The emulator keeps a couple of screens of history, to scroll back to without the
        // archive (and all a resize reflows), and up to a batch more before the oldest move out.
        // Output adds up to a screen of rows before the next look at the history's length: with
        // it, the history stays within the rows the emulator allocates first.
        self.high = EMULATOR_ROWS.saturating_sub(screen).max(BATCH).min(2 * screen.max(32) + BATCH);
        self.keep = self.high.saturating_sub(BATCH).max(self.high / 2);
        self.due = self.high;
    }

    /// The output about to be parsed may push up to a screen of rows into the history: move the
    /// oldest out if the emulator holds too many.
    fn make_room<T: EventListener>(&mut self, term: &mut Term<T>, seen: &mut i64) {
        let history = term.grid().history_size();
        if self.flood {
            // The emulator keeps the history as it does alone; what is older goes first.
            self.due = history + BATCH;
            self.lines.trim(self.limit.saturating_sub(self.due + term.screen_lines()), false);
            return;
        }
        let moved = self.compact(term, seen, self.keep);
        let now = Instant::now();
        if now.duration_since(self.window.0) > FLOOD_WINDOW {
            self.window = (now, 0);
        }
        self.window.1 += moved;
        if self.window.1 > self.limit {
            // More than the scrollback holds within the window: most of it would be kept only
            // to be dropped. Until the output stops (see `rest`).
            self.flood = true;
            self.due = history + BATCH;
            return;
        }
        let history = term.grid().history_size();
        if history > self.high {
            // Rows in use stay: look again after a batch, and keep both parts within the limit.
            self.due = history + BATCH;
            self.lines.trim(self.limit.saturating_sub(self.due + term.screen_lines()), false);
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
        if self.extents.lost || self.extents.history.len() != history {
            self.extents.lost = false;
            self.extents.history = std::iter::repeat_n(WHOLE, history).collect();
        }
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
            let extent = self.extents.history[(end - top) as usize];
            self.lines.push(&rows, wraps(end), usize::from(extent));
            line = end + 1;
        }
        let moved = (line - top) as usize;
        if moved > 0 {
            let grid = term.grid_mut();
            grid.update_history(history - moved);
            grid.update_history(self.limit);
            self.extents.history.drain(..moved);
            *seen -= moved as i64;
        }
        // Both parts within the scrollback while the emulator's fills up again, as tmux keeps
        // between nine tenths of its limit and all of it.
        self.lines.trim(self.limit.saturating_sub(self.high), false);
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
        self.lines.set_width(width);
        // The oldest lines that no longer fit in the scrollback at this width go first.
        self.lines.trim(self.limit.saturating_sub(grid.history_size()), true);
        let room = self.limit.saturating_sub(grid.history_size());
        // The newest lines, with the rows each takes.
        let mut lines = Vec::new();
        let mut count = 0;
        for line in self.lines.newest_first() {
            let rows = line.rows_at(width);
            if count >= wanted || count + rows > room {
                break;
            }
            count += rows;
            lines.push((line, rows));
        }
        if count == 0 {
            return 0;
        }
        let blank = grid.cursor.template.bg == Color::Named(NamedColor::Background);
        prepend(term.grid_mut(), count, |mut rows| {
            for (line, n) in lines.iter().rev() {
                let (these, rest) = std::mem::take(&mut rows).split_at_mut(*n);
                line.decode_into(width, these, blank);
                rows = rest;
            }
        });
        for _ in 0..lines.len() {
            self.lines.drop_newest();
        }
        for _ in 0..count {
            self.extents.history.push_front(WHOLE);
        }
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

/// Put `added` rows, written by `fill`, above the grid's history, keeping every line where it is
/// (lines are counted from the top of the screen) and the view on the text it shows.
fn prepend(grid: &mut Grid<Cell>, added: usize, fill: impl FnOnce(&mut [Row<Cell>])) {
    let screen = grid.screen_lines() as i32;
    let history = grid.history_size();
    let offset = grid.display_offset();
    // The grid grows by `added` rows, blank (cleared with the cursor's colours), at its bottom:
    // what was there moves up. Put them above the rest, and write them.
    grid.scroll_up::<Color>(&(Line(0)..Line(screen)), added);
    let top = -((history + added) as i32);
    let mut rows: Vec<Row<Cell>> = (top..screen).map(|l| std::mem::take(&mut grid[Line(l)])).collect();
    rows.rotate_right(added);
    fill(&mut rows[..added]);
    for (l, row) in (top..).zip(rows) {
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
        let now = Instant::now();
        if now.duration_since(self.archive.last) > CALM {
            // Output that comes after a pause is no flood any more.
            self.calm();
        }
        self.archive.last = now;
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
        if self.term.grid().history_size() > self.archive.high && !self.archive.flood {
            let keep = self.archive.keep;
            self.archive.compact(&mut self.term, &mut self.commands.history_seen, keep);
            self.archive.due = self.archive.high;
        }
    }

    /// The pane's output paused: after a flood, the history moves to the archive now.
    fn calm(&mut self) {
        if self.archive.flood {
            self.archive.flood = false;
            self.archive.window = (Instant::now(), 0);
            self.settle();
            self.archive.freed = true;
        }
    }

    /// The pane's output stopped: as [`TermPane::calm`]. Whether memory was freed since last
    /// asked.
    pub(super) fn rest(&mut self) -> bool {
        self.calm();
        std::mem::take(&mut self.archive.freed)
    }

    /// The screen changed size: rows it gains come from the history, brought back first.
    pub(super) fn before_resize(&mut self, rows: usize) {
        let screen = self.term.screen_lines();
        if rows > screen {
            self.reveal(rows - screen);
        }
    }

    /// The archived lines are rewrapped only when they come back; their rows at the new width are
    /// counted then, and the oldest that no longer fit go (as the emulator's own do when it
    /// rewraps them).
    pub(super) fn after_resize(&mut self) {
        self.archive.fit(self.term.screen_lines());
        self.archive.lines.set_width(self.term.columns());
        // Rewrapped; and the emulator's scroll region is the whole screen again.
        self.archive.forget_extents(self.term.screen_lines(), self.term.grid().history_size());
        self.archive.extents.region = false;
        self.settle();
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

    /// Where the cursor is, and the history's length.
    #[inline]
    fn before(&self) -> (i32, usize) {
        let grid = self.term.grid();
        (grid.cursor.point.line.0, grid.history_size())
    }

    /// Text (or a tab) was written at the cursor, which was on `line`.
    #[inline]
    fn wrote(&mut self, (line, history): (i32, usize)) {
        if self.full(history.max(self.term.grid().history_size())) {
            return;
        }
        let grid = self.term.grid();
        let grown = grid.history_size().saturating_sub(history);
        let cursor = &grid.cursor;
        let now = cursor.point.line.0;
        // The cell under the cursor too: it waits there to wrap, or a character drawn over the
        // one before goes there at the start of a row.
        let end = cursor.point.column.0 + 1 + usize::from(cursor.input_needs_wrap);
        let insert = self.term.mode().contains(TermMode::INSERT);
        let extents = &mut self.archive.extents;
        if grown > 0 {
            extents.scrolled(grown, true);
        }
        if now != line || grown > 0 {
            // The row it wrapped from changed at its end.
            extents.set(line - grown as i32, WHOLE);
        }
        if insert {
            // What was on the right moved further right.
            extents.set(now, WHOLE);
        } else {
            extents.wrote(now, end);
        }
    }

    /// A line feed, which may push the top row into the history.
    #[inline]
    fn fed(&mut self, history: usize) {
        if self.full(history.max(self.term.grid().history_size())) {
            return;
        }
        let grown = self.term.grid().history_size().saturating_sub(history);
        let extents = &mut self.archive.extents;
        if grown > 0 {
            extents.scrolled(grown, true);
        } else if extents.region {
            // Lines may have moved inside the region.
            extents.screen.fill(WHOLE);
        }
    }

    /// The grid changed in a way not followed.
    fn changed(&mut self, history: usize) {
        if self.full(history.max(self.term.grid().history_size())) {
            return;
        }
        let grown = self.term.grid().history_size().saturating_sub(history);
        self.archive.extents.scrolled(grown, false);
    }

    /// Whether the history was full: the emulator then drops its oldest rows as others come,
    /// and its length does not tell that lines moved. Every row is read whole from then on, until
    /// its rows are counted again (see `compact`).
    #[inline]
    fn full(&mut self, history: usize) -> bool {
        let extents = &mut self.archive.extents;
        if history >= self.archive.limit && !extents.lost {
            extents.screen.fill(WHOLE);
            extents.history.clear();
            extents.lost = true;
        }
        extents.lost
    }
}

/// Handler methods passed on as they are: they change no cell.
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

/// Handler methods that change cells or move lines in ways not followed: every row is read
/// whole when it moves out.
macro_rules! changing {
    ($($name:ident($($arg:ident: $ty:ty),*);)*) => {
        $(
            fn $name(&mut self, $($arg: $ty),*) {
                let (_, history) = self.before();
                Handler::$name(&mut *self.term, $($arg),*);
                self.changed(history);
            }
        )*
    };
}

impl<T: EventListener> Handler for Feed<'_, T> {
    #[inline]
    fn input(&mut self, c: char) {
        self.room();
        if self.archive.extents.lost {
            self.term.input(c);
            return;
        }
        let before = self.before();
        self.term.input(c);
        self.wrote(before);
    }

    fn put_tab(&mut self, count: u16) {
        self.room();
        let before = self.before();
        self.term.put_tab(count);
        self.wrote(before);
    }

    #[inline]
    fn linefeed(&mut self) {
        self.room();
        if self.archive.extents.lost {
            self.term.linefeed();
            return;
        }
        let (_, history) = self.before();
        self.term.linefeed();
        self.fed(history);
    }

    #[inline]
    fn newline(&mut self) {
        self.room();
        let (_, history) = self.before();
        self.term.newline();
        self.fed(history);
    }

    fn clear_line(&mut self, mode: LineClearMode) {
        let point = self.term.grid().cursor.point;
        // The cells cleared are like those past the extent, or the whole row is.
        let end = match mode {
            LineClearMode::Right => Some(point.column.0),
            LineClearMode::Left => Some(point.column.0 + 1),
            LineClearMode::All => None,
        };
        self.term.clear_line(mode);
        match end {
            Some(end) => self.archive.extents.wrote(point.line.0, end),
            None => self.archive.extents.set(point.line.0, 0),
        }
    }

    fn erase_chars(&mut self, count: usize) {
        let point = self.term.grid().cursor.point;
        self.term.erase_chars(count);
        self.archive.extents.wrote(point.line.0, point.column.0.saturating_add(count));
    }

    fn insert_blank(&mut self, count: usize) {
        let point = self.term.grid().cursor.point;
        self.term.insert_blank(count);
        // Cells moved right by `count`, blanks where they were.
        let extents = &mut self.archive.extents;
        let at = usize::try_from(point.line.0).ok().and_then(|l| extents.screen.get(l).copied());
        if let Some(extent) = at.filter(|e| *e != WHOLE) {
            let end = usize::from(extent).max(point.column.0).saturating_add(count);
            extents.set(point.line.0, end.min(usize::from(WHOLE - 1)) as u16);
        }
    }

    fn scroll_up(&mut self, rows: usize) {
        self.room();
        let (_, history) = self.before();
        self.term.scroll_up(rows);
        self.changed(history);
    }

    fn delete_lines(&mut self, rows: usize) {
        self.room();
        let (_, history) = self.before();
        self.term.delete_lines(rows);
        self.changed(history);
    }

    fn clear_screen(&mut self, mode: ClearMode) {
        let saved = matches!(mode, ClearMode::Saved) && !self.term.mode().contains(TermMode::ALT_SCREEN);
        match mode {
            ClearMode::All => self.room(),
            _ if saved => self.archive.lines.clear(),
            _ => {}
        }
        let (_, history) = self.before();
        self.term.clear_screen(mode);
        self.changed(history);
        if saved {
            self.archive.extents.history.clear();
        }
    }

    fn reset_state(&mut self) {
        self.archive.lines.clear();
        self.term.reset_state();
        let screen = self.term.screen_lines();
        self.archive.extents = Extents { screen: vec![0; screen], ..Extents::default() };
    }

    fn set_scrolling_region(&mut self, top: usize, bottom: Option<usize>) {
        self.term.set_scrolling_region(top, bottom);
        // Only the whole screen is followed; the emulator goes back to it on its own too (a
        // resize), but this is told only of a region set.
        let screen = self.term.screen_lines();
        self.archive.extents.region = top > 1 || bottom.is_some_and(|b| b < screen);
    }

    changing! {
        scroll_down(rows: usize);
        insert_blank_lines(rows: usize);
        delete_chars(count: usize);
        reverse_index();
        set_private_mode(mode: PrivateMode);
        unset_private_mode(mode: PrivateMode);
        decaln();
    }

    forward! {
        set_title(title: Option<String>);
        set_cursor_style(style: Option<CursorStyle>);
        set_cursor_shape(shape: CursorShape);
        goto(line: i32, col: usize);
        goto_line(line: i32);
        goto_col(col: usize);
        move_up(rows: usize);
        move_down(rows: usize);
        identify_terminal(intermediate: Option<char>);
        device_status(arg: usize);
        move_forward(cols: usize);
        move_backward(cols: usize);
        move_down_and_cr(rows: usize);
        move_up_and_cr(rows: usize);
        backspace();
        carriage_return();
        bell();
        substitute();
        set_horizontal_tabstop();
        move_backward_tabs(count: u16);
        move_forward_tabs(count: u16);
        save_cursor_position();
        restore_cursor_position();
        clear_tabs(mode: TabulationClearMode);
        set_tabs(interval: u16);
        terminal_attribute(attr: Attr);
        set_mode(mode: Mode);
        unset_mode(mode: Mode);
        report_mode(mode: Mode);
        report_private_mode(mode: PrivateMode);
        set_keypad_application_mode();
        unset_keypad_application_mode();
        set_active_charset(index: CharsetIndex);
        configure_charset(index: CharsetIndex, charset: StandardCharset);
        set_color(index: usize, color: Rgb);
        dynamic_color_sequence(prefix: String, index: usize, terminator: &str);
        reset_color(index: usize);
        clipboard_store(clipboard: u8, data: &[u8]);
        clipboard_load(clipboard: u8, terminator: &str);
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

        /// Feed both; then the pane's output stops (a flood goes to the archive).
        fn feed(&mut self, bytes: &[u8]) {
            self.pane.feed(bytes);
            self.pane.rest();
            self.parser.advance(&mut self.reference, bytes);
        }

        fn term(&self) -> &TermPane {
            self.pane.term().unwrap()
        }

        /// Rows of history the pane holds, both parts, at its width now.
        fn history(&mut self) -> usize {
            let t = self.pane.term_mut().unwrap();
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
        let t = b.term();
        let bytes = t.archive.lines.bytes();
        assert!(bytes < t.archive.lines.len() * 60, "{bytes} bytes for {} lines", t.archive.lines.len());
    }

    #[test]
    fn a_flood_stays_in_the_emulator_until_it_stops() {
        let mut b = Both::new();
        b.feed(sample(1000).as_bytes());
        // More than the scrollback, at once: kept as the emulator alone keeps it.
        let flood = sample_of(6000, false);
        b.pane.feed(flood.as_bytes());
        b.parser.advance(&mut b.reference, flood.as_bytes());
        let t = b.term();
        assert!(t.archive.flood);
        assert_eq!(t.term.grid().history_size(), SCROLLBACK);
        assert!(t.archive.lines.is_empty(), "older than all the emulator holds");
        assert_eq!(b.pane.text(Some(SCROLLBACK + 20)), b.reference_text_all());
        // Typing meanwhile moves nothing.
        b.pane.write_input(Vec::new());
        assert_eq!(b.term().term.grid().history_size(), SCROLLBACK);
        // Once it stops, the history goes to the archive and reads the same.
        assert!(b.pane.rest());
        assert!(!b.pane.rest());
        let t = b.term();
        assert!(!t.archive.flood && t.term.grid().history_size() <= t.archive.high);
        let kept = b.history();
        assert_eq!(b.pane.text(Some(kept + ROWS as usize)), reference_text(&b.reference, kept));
        // Steady output is no flood.
        for _ in 0..20 {
            b.pane.feed(sample(100).as_bytes());
        }
        assert!(!b.term().archive.flood);
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
        // The same selection in the emulator that kept everything reads the same.
        let range = b.term().term.selection.as_ref().unwrap().to_range(&b.term().term).unwrap();
        assert_eq!(range.start.line, Line(-(kept as i32)));
        assert!(range.end.line >= Line(-(kept as i32) + 2000));
        b.reference.selection = b.term().term.selection.clone();
        let expected = b.reference.selection_to_string().unwrap();
        let differ = selected.lines().zip(expected.lines()).position(|(a, b)| a != b);
        assert_eq!(differ, None, "{:?}", differ.map(|i| (selected.lines().nth(i), expected.lines().nth(i))));
        assert_eq!(selected, expected);
        assert!(selected.lines().count() > 1500);
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
            // The emulator alone may have kept the end of a line whose start it dropped; the
            // archive drops whole lines.
            let n = ours.len().min(reference.len()) - 1;
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

    /// Output of every kind, at random: text in colours, wide and combining characters, tabs,
    /// erasing, inserting and deleting characters and lines, cursor moves, insert mode, scroll
    /// regions, the alternate screen. The history reads as the emulator's alone, cell by cell:
    /// a row is never cut short on its way to the archive.
    #[test]
    fn rows_written_any_way_keep_all_their_cells() {
        let mut rng = 0x2545_f491_4f6c_dd1du64;
        let mut next = move |n: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % n
        };
        for round in 0..6 {
            let mut b = Both::new();
            let mut out = String::new();
            for _ in 0..12_000 {
                let n = next(100);
                match n {
                    0..40 => {
                        let len = next(40) as usize;
                        out.push_str(&format!("\x1b[3{}m{}", next(8), "abcdefghijk ".repeat(4).get(..len).unwrap()));
                    }
                    40..55 => out.push_str("\r\n"),
                    55..58 => out.push_str("\x1b[44m"),
                    58..62 => out.push_str("\x1b[0m"),
                    62..64 => out.push_str(["\x1b[K", "\x1b[1K", "\x1b[2K"][next(3) as usize]),
                    64..66 => out.push_str(&format!("\x1b[{}X", next(50))),
                    66..67 => out.push_str(&format!("\x1b[{}@", next(5))),
                    67..68 => out.push_str(&format!("\x1b[{}P", next(5))),
                    68..70 => out.push('\t'),
                    70..72 => out.push_str("日本\u{301}x"),
                    72..74 => out.push_str(&format!("\x1b[{};{}H", next(12) + 1, next(35) + 1)),
                    74..76 => out.push_str(["\x1b[A", "\x1b[B", "\x1b[5C", "\x1b[3D", "\r"][next(5) as usize]),
                    76..77 => out.push_str(["\x1b[4h", "\x1b[4l"][next(2) as usize]),
                    77..78 => out.push_str(["\x1b[2;8r", "\x1b[1;5r", "\x1b[r"][next(3) as usize]),
                    78..79 => {
                        out.push_str(["\x1b[2S", "\x1b[T", "\x1b[2L", "\x1b[M", "\x1bM", "\x1bD"][next(6) as usize])
                    }
                    79..80 => out.push_str(["\x1b[J", "\x1b[1J", "\x1b[2J"][next(3) as usize]),
                    80..81 if next(10) == 0 => out.push_str(["\x1b[?1049h", "\x1b[?1049l"][next(2) as usize]),
                    81..82 if next(50) == 0 => out.push_str("\x1b#8"),
                    _ => out.push_str(&format!("line {n} \x1b[1mbold\x1b[0m\r\n")),
                }
            }
            out.push_str("\x1b[?1049l\x1b[r\x1b[4l\x1b[0m");
            for chunk in out.as_bytes().chunks(997) {
                b.feed(chunk);
            }
            let kept = b.history();
            assert!(!b.term().archive.lines.is_empty(), "round {round}");
            eprintln!(
                "kept {kept} emulator {} lines {} reference {}",
                b.term().term.grid().history_size(),
                b.term().archive.lines.len(),
                b.reference.grid().history_size()
            );
            // Every row, through views scrolled over the whole history.
            b.pane.scroll_to_top();
            eprintln!("after {} lines {}", b.term().term.grid().history_size(), b.term().archive.lines.len());
            let top = b.pane.display_offset();
            assert_eq!(top, kept);
            for offset in (0..=top).rev().step_by(ROWS as usize - 1) {
                b.pane.scroll_to_bottom();
                b.pane.scroll(offset as i32);
                b.reference.scroll_display(Scroll::Bottom);
                b.reference.scroll_display(Scroll::Delta(offset as i32));
                assert_eq!(view(&b.term().term), view(&b.reference), "round {round}, {offset} rows up");
            }
        }
    }

    /// What the archive costs on bulk output: `cargo test --release -- --ignored --nocapture
    /// archive_cost`.
    #[test]
    #[ignore]
    fn archive_cost() {
        let size = Size { cols: 169, rows: 47 };
        let config = TermConfig { scrolling_history: 10_000, ..TermConfig::default() };
        let seq: Vec<u8> = (1..=3_000_000).flat_map(|i| format!("{i}\r\n").into_bytes()).collect();
        let log: Vec<u8> = (0..300_000)
            .flat_map(|i| {
                format!("\x1b[2m00:00:{:02}.{:03}\x1b[0m \x1b[32mINFO\x1b[0m p00 req={i:06} ok\r\n", i % 60, i % 1000)
                    .into_bytes()
            })
            .collect();
        for (name, input) in [("seq 3M", &seq), ("log 300k", &log)] {
            for archived in [false, true, false, true] {
                let mut term = Term::new(config.clone(), &size, Listener::default());
                let mut archive = Archive::new(10_000, 47);
                let mut seen = 0;
                let mut parser: Processor = Processor::new();
                let start = std::time::Instant::now();
                for chunk in input.chunks(64 << 10) {
                    if archived {
                        parser.advance(&mut Feed { term: &mut term, archive: &mut archive, seen: &mut seen }, chunk);
                    } else {
                        parser.advance(&mut term, chunk);
                    }
                }
                let took = start.elapsed();
                eprintln!(
                    "{name}: {} {:?}, emulator history {}, archive {} lines",
                    if archived { "archive" } else { "plain  " },
                    took,
                    term.grid().history_size(),
                    archive.lines.len()
                );
            }
        }
    }
}
