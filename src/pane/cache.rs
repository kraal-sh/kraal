//! A terminal pane's screen as last drawn, kept between frames: only the rows the emulator says
//! changed are drawn from its grid again, and each row has a version that changes with its cells,
//! so that a client can tell which rows differ from what it was sent.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::selection::SelectionRange;
use alacritty_terminal::term::{Term, TermDamage};
use ratatui::buffer::{Buffer, Cell};
use ratatui::layout::Rect;

use super::screen::{draw_cell, link_of};

/// Versions are unique across panes and restarts: a row of a new screen never passes for one a
/// client was sent.
static VERSIONS: AtomicU64 = AtomicU64::new(1);

fn new_version() -> u64 {
    VERSIONS.fetch_add(1, Ordering::Relaxed)
}

#[derive(Default)]
pub struct ScreenCache {
    cols: usize,
    rows: usize,
    /// What the rows were drawn with besides the grid: when either changes, all of them are
    /// drawn again.
    offset: usize,
    selection: Option<SelectionRange>,
    /// The count of the grid's changes the rows were drawn after, and of the archive's moves.
    changes: Option<u64>,
    moved: u64,
    cells: Vec<Cell>,
    versions: Vec<u64>,
    /// The links of each row: column and target.
    links: Vec<Vec<(u16, Arc<str>)>>,
    /// Rows with image placeholders: what they show depends on the images too.
    images: Vec<bool>,
    /// A row being drawn.
    row: Vec<Cell>,
    row_links: Vec<(u16, Arc<str>)>,
}

impl ScreenCache {
    /// Bring the rows up to date with `term`: those its damage covers, or all of them when the
    /// view moved, the selection changed or the size did. `changes` counts the changes made to
    /// the grid: nothing is looked at while it, the view and the selection stay the same. `moved`
    /// counts the moves of rows the emulator's damage does not tell: every row is looked at again.
    pub fn refresh<T: EventListener>(&mut self, term: &mut Term<T>, (changes, moved): (u64, u64)) {
        let (cols, rows) = (term.columns(), term.screen_lines());
        let offset = term.grid().display_offset();
        let selection = term.selection.as_ref().and_then(|s| s.to_range(term));
        let resized = (cols, rows) != (self.cols, self.rows) || self.cells.len() != cols * rows;
        let mut all = resized || offset != self.offset || selection != self.selection || moved != self.moved;
        if !all && self.changes == Some(changes) {
            return;
        }
        self.changes = Some(changes);
        self.moved = moved;
        let mut lines = Vec::new();
        match term.damage() {
            TermDamage::Full => all = true,
            TermDamage::Partial(damaged) => {
                if !all {
                    lines.extend(damaged.map(|d| d.line));
                }
            }
        }
        term.reset_damage();
        self.offset = offset;
        self.selection = selection;
        if resized {
            self.cols = cols;
            self.rows = rows;
            self.cells.clear();
            self.cells.resize(cols * rows, Cell::EMPTY);
            self.versions.clear();
            self.versions.resize(rows, 0);
            self.links.clear();
            self.links.resize_with(rows, Vec::new);
            self.images.clear();
            self.images.resize(rows, false);
        }
        if all {
            lines.clear();
            lines.extend(0..rows);
        }
        for y in lines {
            if y < rows {
                self.draw_row(term, y, resized);
            }
        }
    }

    /// Draw row `y` from the grid; its version changes when its cells did (always when `fresh`).
    fn draw_row<T>(&mut self, term: &Term<T>, y: usize, fresh: bool) {
        let cols = self.cols;
        let line = Line(y as i32 - self.offset as i32);
        let grid = term.grid();
        let colors = term.colors();
        let selection = self.selection.as_ref();
        self.row.clear();
        self.row.resize(cols, Cell::EMPTY);
        self.row_links.clear();
        let mut images = false;
        let mut uris: Vec<Arc<str>> = Vec::new();
        for (x, out) in self.row.iter_mut().enumerate() {
            let cell = &grid[line][Column(x)];
            if let Some(h) = link_of(cell) {
                let uri = match uris.iter().find(|u| ***u == *h.uri()) {
                    Some(u) => u.clone(),
                    None => {
                        let u: Arc<str> = Arc::from(h.uri());
                        uris.push(u.clone());
                        u
                    }
                };
                self.row_links.push((x as u16, uri));
            }
            draw_cell(cell, Point::new(line, Column(x)), x + 1 >= cols, colors, selection, out);
            images |= out.symbol().starts_with(crate::graphics::PLACEHOLDER);
        }
        let cells = &mut self.cells[y * cols..(y + 1) * cols];
        if fresh || cells != self.row.as_slice() {
            cells.swap_with_slice(&mut self.row);
            self.versions[y] = new_version();
        }
        std::mem::swap(&mut self.links[y], &mut self.row_links);
        self.images[y] = images;
    }

    /// The size the rows are drawn at.
    pub fn size(&self) -> (u16, u16) {
        (self.cols as u16, self.rows as u16)
    }

    pub fn versions(&self) -> &[u64] {
        &self.versions
    }

    /// Whether what row `y` shows depends on images.
    pub fn has_images(&self, y: usize) -> bool {
        self.images.get(y).copied().unwrap_or(false)
    }

    /// Copy the rows `rows` says to into `buf` at `area` (the size of the rows), and add every
    /// row's links to `links`.
    pub fn draw(&self, area: Rect, buf: &mut Buffer, rows: impl Fn(usize) -> bool, links: &mut crate::ansi::Links) {
        let (bx, by, bw) = (usize::from(buf.area.x), usize::from(buf.area.y), usize::from(buf.area.width));
        let (x, y) = (usize::from(area.x), usize::from(area.y));
        if area.width as usize != self.cols || area.height as usize != self.rows || buf.area.union(area) != buf.area {
            return;
        }
        let cols = self.cols;
        for r in 0..self.rows {
            if rows(r) {
                let at = (y + r - by) * bw + (x - bx);
                if let Some(dst) = buf.content.get_mut(at..at + cols) {
                    dst.clone_from_slice(&self.cells[r * cols..(r + 1) * cols]);
                }
            }
            for (cx, uri) in &self.links[r] {
                links.insert((area.x + cx, area.y + r as u16), uri.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::grid::Scroll;
    use alacritty_terminal::index::Side;
    use alacritty_terminal::selection::{Selection, SelectionType};
    use alacritty_terminal::term::Config;
    use alacritty_terminal::vte::ansi::Processor;

    use super::*;
    use crate::pane::pty::Size;
    use crate::pane::screen::render_term;

    fn term(cols: u16, rows: u16, history: usize) -> Term<VoidListener> {
        Term::new(Config { scrolling_history: history, ..Config::default() }, &Size { cols, rows }, VoidListener)
    }

    type Drawn = (Buffer, crate::ansi::Links);

    /// What the cache draws (all rows) and what a full render draws, at the same place.
    fn both(cache: &ScreenCache, term: &Term<VoidListener>) -> (Drawn, Drawn) {
        let area = Rect::new(1, 2, cache.cols as u16, cache.rows as u16);
        let screen = Rect::new(0, 0, area.width + 2, area.height + 3);
        let (mut cached, mut full) = (Buffer::empty(screen), Buffer::empty(screen));
        let (mut cached_links, mut full_links) = (crate::ansi::Links::new(), crate::ansi::Links::new());
        cache.draw(area, &mut cached, |_| true, &mut cached_links);
        render_term(term, area, &mut full, Some(&mut full_links));
        ((cached, cached_links), (full, full_links))
    }

    fn row(buf: &Buffer, y: u16) -> Vec<Cell> {
        (0..buf.area.width).map(|x| buf[(x, y)].clone()).collect()
    }

    #[test]
    fn only_rows_that_change_get_a_new_version() {
        let mut t = term(10, 4, 0);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut t, b"hello\r\nworld");
        let mut cache = ScreenCache::default();
        cache.refresh(&mut t, (1, 0));
        let before = cache.versions().to_vec();
        assert!(before.iter().all(|v| *v > 0));
        parser.advance(&mut t, b"!");
        cache.refresh(&mut t, (2, 0));
        let after = cache.versions().to_vec();
        assert_eq!((after[0], after[2], after[3]), (before[0], before[2], before[3]));
        assert_ne!(after[1], before[1]);
        // The cursor moving changes no cell.
        parser.advance(&mut t, b"\x1b[1;1H\x1b[3;5H");
        cache.refresh(&mut t, (3, 0));
        assert_eq!(cache.versions(), after.as_slice());
        let (cached, full) = both(&cache, &t);
        assert_eq!(cached, full);
    }

    #[test]
    fn the_grid_is_looked_at_only_when_it_changed() {
        let mut t = term(6, 2, 0);
        let mut parser: Processor = Processor::new();
        let mut cache = ScreenCache::default();
        cache.refresh(&mut t, (1, 0));
        let before = cache.versions().to_vec();
        // Changed behind the count's back: not seen.
        parser.advance(&mut t, b"abc");
        cache.refresh(&mut t, (1, 0));
        assert_eq!(cache.versions(), before.as_slice());
        // Counted: seen.
        cache.refresh(&mut t, (2, 0));
        assert_ne!(cache.versions()[0], before[0]);
        let (cached, full) = both(&cache, &t);
        assert_eq!(cached, full);
        // Rows moved without damage (the archive's): every row is looked at.
        t.grid_mut()[Line(1)][Column(0)].c = 'z';
        cache.refresh(&mut t, (2, 0));
        assert_ne!(both(&cache, &t).0, both(&cache, &t).1);
        cache.refresh(&mut t, (2, 1));
        let (cached, full) = both(&cache, &t);
        assert_eq!(cached, full);
    }

    #[test]
    fn a_new_screen_never_reuses_versions() {
        let mut a = ScreenCache::default();
        a.refresh(&mut term(5, 2, 0), (0, 0));
        let mut b = ScreenCache::default();
        b.refresh(&mut term(5, 2, 0), (0, 0));
        assert!(a.versions().iter().all(|v| !b.versions().contains(v)));
    }

    #[test]
    fn rows_with_image_placeholders_are_told() {
        let mut t = term(6, 3, 0);
        let mut parser: Processor = Processor::new();
        parser.advance(&mut t, "ab\r\n\u{10EEEE}".as_bytes());
        let mut cache = ScreenCache::default();
        cache.refresh(&mut t, (1, 0));
        assert_eq!((cache.has_images(0), cache.has_images(1), cache.has_images(2)), (false, true, false));
    }

    /// Random output, the view scrolled back and selections: after each step, the rows the cache
    /// has are what a full render draws, and a row's version changes exactly when what it shows
    /// does.
    #[test]
    fn rows_follow_whatever_the_terminal_does() {
        let mut rng = 0x2545_f491_4f6c_dd1du64;
        let mut next = move |n: u64| {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng % n
        };
        let pieces: &[&str] = &[
            "abc",
            "0123456789",
            "日本",
            "\u{2764}\u{fe0f}",
            "e\u{301}",
            "\r",
            "\n",
            "\t",
            "\x08",
            "\x1b[A",
            "\x1b[2B",
            "\x1b[3C",
            "\x1b[D",
            "\x1b[2;3H",
            "\x1b[H",
            "\x1b[J",
            "\x1b[1J",
            "\x1b[2J",
            "\x1b[3J",
            "\x1b[K",
            "\x1b[1K",
            "\x1b[2K",
            "\x1b[2@",
            "\x1b[P",
            "\x1b[3X",
            "\x1b[L",
            "\x1b[M",
            "\x1b[S",
            "\x1b[T",
            "\x1b[2;4r",
            "\x1b[r",
            "\x1b[31m",
            "\x1b[42m",
            "\x1b[38;5;200m",
            "\x1b[48;2;1;2;3m",
            "\x1b[1;4;7m",
            "\x1b[0m",
            "\x1b[4h",
            "\x1b[4l",
            "\x1b[?7l",
            "\x1b[?7h",
            "\x1b[?1049h",
            "\x1b[?1049l",
            "\x1bM",
            "\x1bD",
            "\x1bE",
            "\x1b#8",
            "\x1b7",
            "\x1b8",
            "x\x1b[3b",
            "\x1b]8;;https://example.org\x1b\\",
            "\x1b]8;;\x1b\\",
            "\x1b]4;1;rgb:ff/80/00\x07",
            "\x1b]104\x07",
            "\u{10EEEE}",
            "\x1bc",
        ];
        for case in 0..40 {
            let (cols, rows) = (4 + next(9) as u16, 2 + next(5) as u16);
            let mut t = term(cols, rows, 20);
            let mut parser: Processor = Processor::new();
            let mut cache = ScreenCache::default();
            let mut changes = 0;
            cache.refresh(&mut t, (changes, 0));
            let ((mut was, _), _) = both(&cache, &t);
            let mut versions = cache.versions().to_vec();
            for step in 0..300 {
                match next(20) {
                    0 => t.scroll_display(Scroll::Delta(next(5) as i32 - 2)),
                    1 => t.selection = None,
                    2 => {
                        let at = |line: u64, col: u64| Point::new(Line(line as i32), Column(col as usize));
                        let start = at(next(u64::from(rows)), next(u64::from(cols)));
                        let mut sel = Selection::new(SelectionType::Simple, start, Side::Left);
                        sel.update(at(next(u64::from(rows)), next(u64::from(cols))), Side::Right);
                        t.selection = Some(sel);
                    }
                    _ => {
                        for _ in 0..1 + next(4) {
                            parser.advance(&mut t, pieces[next(pieces.len() as u64) as usize].as_bytes());
                            changes += 1;
                        }
                    }
                }
                // As a pane does after parsing.
                crate::pane::archive::keep_view_in_history(&mut t);
                cache.refresh(&mut t, (changes, 0));
                let ((cached, cached_links), (full, full_links)) = both(&cache, &t);
                assert_eq!(cached, full, "case {case} step {step}");
                assert_eq!(cached_links, full_links, "case {case} step {step}");
                for (r, (old, new)) in versions.iter().zip(cache.versions()).enumerate() {
                    let y = r as u16 + 2;
                    assert_eq!(old == new, row(&was, y) == row(&cached, y), "case {case} step {step} row {r}");
                }
                was = cached;
                versions = cache.versions().to_vec();
            }
        }
    }
}
