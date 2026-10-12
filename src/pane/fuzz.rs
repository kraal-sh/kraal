//! Randomized tests of a pane fed any output (see `crate::fuzz`): nothing panics or hangs, the
//! history stays within the scrollback, the screen drawn from the cache is the screen, and the
//! emulator shows what alacritty alone shows when kraal passes the output on.

use std::sync::Arc;

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::selection::SelectionType;
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::vi_mode::ViMotion;
use alacritty_terminal::vte::ansi::Processor;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use super::pty::{Listener, Size};
use super::screen::{render_term, row_text};
use super::*;
use crate::fuzz::{self, Rng};

struct Fixture {
    pane: Pane,
    env: PaneEnv,
    tx: crossbeam_channel::Sender<Event>,
    _rx: crossbeam_channel::Receiver<Event>,
}

/// A pane whose program is never started: what it is fed is all it prints.
fn fixture(cols: u16, rows: u16, scrollback: usize, graphics: Arc<crate::graphics::Shared>) -> Fixture {
    let (tx, rx) = crossbeam_channel::unbounded();
    let env = PaneEnv { socket: "/nonexistent".into(), session: "t".into(), shell: "sh".into(), scrollback, graphics };
    let spec = PaneSpec { command: vec!["true".into()], ..Default::default() };
    let batch = super::spawn::batch();
    let pane = Pane::spawn(1, None, spec, cols, rows, &env, &tx).unwrap();
    drop(batch);
    Fixture { pane, env, tx, _rx: rx }
}

fn graphics(rng: &mut Rng) -> Arc<crate::graphics::Shared> {
    let shared = Arc::new(crate::graphics::Shared::new(!rng.one_in(8), rng.pick(&[1 << 20, 64 << 20]).to_owned()));
    shared.set_available(!rng.one_in(4));
    shared.set_cell(rng.range(1, 30) as u16, rng.range(1, 40) as u16);
    shared
}

const MOTIONS: [ViMotion; 21] = [
    ViMotion::Up,
    ViMotion::Down,
    ViMotion::Left,
    ViMotion::Right,
    ViMotion::First,
    ViMotion::Last,
    ViMotion::FirstOccupied,
    ViMotion::High,
    ViMotion::Middle,
    ViMotion::Low,
    ViMotion::SemanticLeft,
    ViMotion::SemanticRight,
    ViMotion::SemanticLeftEnd,
    ViMotion::SemanticRightEnd,
    ViMotion::WordLeft,
    ViMotion::WordRight,
    ViMotion::WordLeftEnd,
    ViMotion::WordRightEnd,
    ViMotion::Bracket,
    ViMotion::ParagraphUp,
    ViMotion::ParagraphDown,
];

const SELECTIONS: [SelectionType; 4] =
    [SelectionType::Simple, SelectionType::Block, SelectionType::Semantic, SelectionType::Lines];

/// A size panes take, small ones often.
fn size(rng: &mut Rng) -> (u16, u16) {
    match rng.below(4) {
        0 => (rng.range(0, 4) as u16, rng.range(0, 3) as u16),
        1 => (rng.range(2, 300) as u16, rng.range(1, 120) as u16),
        _ => (rng.range(5, 90) as u16, rng.range(2, 30) as u16),
    }
}

/// What a client does with the pane, between reads of its output.
fn act(rng: &mut Rng, f: &mut Fixture, recent: &[u8]) {
    let p = &mut f.pane;
    let action = rng.below(30);
    let t = p.term().unwrap();
    fuzz::trace(|| {
        let g = t.term.grid();
        format!(
            "act {action} on {}x{} history {} offset {} vi {:?} lens {}",
            p.cols,
            p.rows,
            g.history_size(),
            g.display_offset(),
            t.term.vi_mode_cursor.point,
            t.archive.lens_rows()
        )
    });
    match action {
        0 => {
            let (cols, rows) = size(rng);
            p.resize(cols, rows);
        }
        1 => {
            p.rest();
        }
        2 => p.scroll(rng.range(0, 60) as i32 - 30),
        3 => p.scroll(rng.number() as i32),
        4 => p.scroll_page(rng.one_in(2)),
        5 => {
            if rng.one_in(2) {
                p.scroll_to_top()
            } else {
                p.scroll_to_bottom()
            }
        }
        6 => p.copy_mode(!p.in_copy_mode()),
        7..10 => {
            p.copy_mode(true);
            for _ in 0..rng.range(1, 20) {
                let motion = *rng.pick(&MOTIONS);
                fuzz::trace(|| format!("  {motion:?} from {:?}", p.term().unwrap().term.vi_mode_cursor.point));
                p.vi_motion(motion);
                check_grid(p);
            }
        }
        10 => p.vi_edge(rng.one_in(2)),
        11 => p.vi_scroll(rng.range(0, 80) as i32 - 40),
        12 => p.vi_select(*rng.pick(&SELECTIONS)),
        13 => {
            // A query from what was printed, or anything.
            let query = if !recent.is_empty() && rng.one_in(2) {
                let at = rng.below(recent.len());
                let n = rng.range(1, 12).min(recent.len() - at);
                String::from_utf8_lossy(&recent[at..at + n]).into_owned()
            } else {
                String::from_utf8_lossy(&rng.text(10)).into_owned()
            };
            p.search(&query);
            for _ in 0..rng.below(6) {
                p.search_next(rng.one_in(2));
            }
        }
        14 => p.end_search(),
        15 => {
            p.copy_mode(true);
            p.select_command(rng.one_in(2));
            let _ = p.selection_text();
        }
        16 => {
            p.copy_mode(true);
            p.jump_to_prompt(rng.one_in(2));
        }
        17 => {
            let _ = p.last_command_output();
            let _ = p.last_failure();
            let _ = p.last_command();
        }
        18 => {
            let _ = p.text(Some(rng.size(5000)));
            let _ = p.text(None);
            let _ = p.live_text();
        }
        19 => {
            let _ = p.screen_rows();
            let (x, y) = (rng.below(400) as u16, rng.below(200) as u16);
            let _ = p.url_at(x, y);
            let _ = p.cursor(Rect::new(0, 0, p.cols, p.rows));
        }
        20 => {
            let (x, y) = (rng.below(400) as u16, rng.below(200) as u16);
            let kind = *rng.pick(&SELECTIONS);
            let (x2, y2) = (rng.below(400) as u16, rng.below(200) as u16);
            fuzz::trace(|| format!("  select {kind:?} from {x},{y} to {x2},{y2}"));
            p.selection_start(x, y, kind);
            p.selection_update(x2, y2);
            let _ = p.selection_text();
        }
        21 => p.clear_selection(),
        22 => {
            let _ = p.poll_sync_timeout();
        }
        23 => {
            let saved = p.scrollback_ansi();
            if let Some(saved) = saved {
                let mut fresh = fixture(p.cols, p.rows, rng.range(0, 3000), f.env.graphics.clone());
                fresh.pane.restore_scrollback(&saved, rng.one_in(2).then_some(1_700_000_000), rng.size(5000));
                let _ = fresh.pane.text(Some(100));
            }
        }
        24 => {
            let _ = p.respawn(&f.env, &f.tx);
        }
        25 => {
            let graphics = &f.env.graphics;
            graphics.set_available(!graphics.available());
        }
        _ => {
            // A frame: what a client is sent, drawn twice.
            draw_and_compare(p);
        }
    }
}

/// The screen drawn from the cache brought up to date is the screen drawn whole.
fn draw_and_compare(p: &mut Pane) {
    let (cols, rows) = (p.cols, p.rows);
    let Some(cache) = p.refresh_screen(cols, rows) else { return };
    let area = Rect::new(1, 2, cols, rows);
    let screen = Rect::new(0, 0, cols + 2, rows + 3);
    let (mut cached, mut full) = (Buffer::empty(screen), Buffer::empty(screen));
    let (mut cached_links, mut full_links) = (crate::ansi::Links::new(), crate::ansi::Links::new());
    cache.draw(area, &mut cached, |_| Some((0, cols)), &mut cached_links);
    p.render(area, &mut full, &mut full_links);
    assert_eq!(cached, full, "the cached screen differs from the screen");
    assert_eq!(cached_links, full_links);
    p.show_images(area, &mut full, true);
    let _ = p.images();
}

/// The emulator's state is one it can read: the view, the copy-mode cursor and the selection
/// on its grid, the lens in the primary screen's history.
fn check_grid(p: &Pane) {
    let Some(t) = p.term() else { return };
    let term = &t.term;
    let grid = term.grid();
    let (top, bottom, last) = (term.topmost_line(), term.bottommost_line(), term.last_column());
    assert!(
        grid.display_offset() <= grid.history_size(),
        "view {} rows up, of {}",
        grid.display_offset(),
        grid.history_size()
    );
    if term.mode().contains(TermMode::ALT_SCREEN) {
        assert_eq!(t.archive.lens_rows(), 0, "a lens on the alternate screen");
    } else {
        assert!(
            t.archive.lens_rows() <= grid.history_size(),
            "a lens of {} rows in {}",
            t.archive.lens_rows(),
            grid.history_size()
        );
    }
    if term.mode().contains(TermMode::VI) {
        let point = term.vi_mode_cursor.point;
        assert!(
            point.line >= top && point.line <= bottom && point.column <= last,
            "copy-mode cursor {point:?} off {top:?}..={bottom:?}"
        );
    }
    if let Some(range) = term.selection.as_ref().and_then(|s| s.to_range(term)) {
        let on = |p: alacritty_terminal::index::Point| p.line >= top && p.line <= bottom && p.column <= last;
        assert!(on(range.start) && on(range.end), "selection {range:?} off {top:?}..={bottom:?}");
    }
}

/// What the pane holds stays within its limits.
fn check_bounds(p: &mut Pane, scrollback: usize) {
    let Some(t) = p.term_mut() else { return };
    let width = t.term.columns();
    let own = t.own_history();
    let archived: usize = t.archive.lines().oldest_first().map(|l| l.rows_at(width)).sum();
    let lens = t.archive.lens_rows();
    // The emulator keeps a couple of screens and a batch of its own beyond what the archive
    // trims to, and the lens is a copy.
    let screens = t.term.screen_lines();
    assert!(
        archived + own <= scrollback + 2 * screens.max(32) + 1300,
        "history of {archived} archived and {own} rows, for a scrollback of {scrollback} ({lens} in the lens)"
    );
    assert!(t.archive.lines().bytes() <= (scrollback + 1) * (width * 64 + 64), "the archive outgrew its rows");
}

#[test]
fn fuzz_pane_output() {
    fuzz::run("pane_output", 12, |rng| {
        let (cols, rows) = size(rng);
        let scrollback = *rng.pick(&[0, 10, 100, 1000, 3000]);
        let mut f = fixture(cols, rows, scrollback, graphics(rng));
        let mut recent = Vec::new();
        for _ in 0..rng.range(1, 40) {
            if rng.below(3) > 0 {
                recent = fuzz::terminal_output(rng, 6000, true);
                fuzz::trace(|| format!("feed {:?}", String::from_utf8_lossy(&recent)));
                for chunk in rng.chunks(&recent) {
                    let signals = f.pane.feed(chunk);
                    for s in signals {
                        if let PaneSignal::Notification { title, body } = s {
                            assert!(title.chars().count() <= 300 && body.chars().count() <= 300);
                        }
                    }
                }
            } else {
                act(rng, &mut f, &recent);
            }
            check_grid(&f.pane);
            check_bounds(&mut f.pane, scrollback);
        }
        draw_and_compare(&mut f.pane);
    });
}

/// An emulator that keeps its whole history, fed as a pane's would be without kraal.
struct Reference {
    term: Term<Listener>,
    parser: Processor,
    /// The start of a character the output so far ended with.
    cut: Vec<u8>,
}

impl Reference {
    fn new(cols: u16, rows: u16, scrollback: usize) -> Reference {
        let config = TermConfig { scrolling_history: scrollback, kitty_keyboard: true, ..TermConfig::default() };
        let term = Term::new(config, &Size { cols, rows }, Listener::default());
        Reference { term, parser: Processor::new(), cut: Vec::new() }
    }

    /// Feed `bytes`, the rest of a character cut by the end of the last alone, as kraal does
    /// (vte 0.15 drops the character after it otherwise).
    fn feed(&mut self, bytes: &[u8]) {
        let need = self.cut.first().map_or(0, |b| super::archive::utf8_len(*b).saturating_sub(self.cut.len()));
        let alone =
            if need > 0 && bytes.len() > need && bytes[..need].iter().all(|b| b & 0xc0 == 0x80) { need } else { 0 };
        self.cut.extend_from_slice(bytes);
        let cut = super::archive::incomplete_char(&self.cut);
        self.cut.drain(..self.cut.len() - cut);
        self.parser.advance(&mut self.term, &bytes[..alone]);
        self.parser.advance(&mut self.term, &bytes[alone..]);
    }
}
/// Every row of the history and the screen, oldest first.
/// The text of every row of the history and the screen, oldest first.
fn all_rows<T>(term: &Term<T>) -> Vec<String> {
    let top = -(term.grid().history_size() as i32);
    (top..term.screen_lines() as i32).map(|l| row_text(term, alacritty_terminal::index::Line(l))).collect()
}

fn view<T: alacritty_terminal::event::EventListener>(term: &Term<T>) -> Buffer {
    let area = Rect::new(0, 0, term.columns() as u16, term.screen_lines() as u16);
    let mut buf = Buffer::empty(area);
    render_term(term, area, &mut buf, None);
    buf
}

/// Output kraal passes on to the emulator as it is: no images, no kraal sequences that make it
/// answer, fewer keyboard modes than the guard lets through.
fn passed_on(rng: &mut Rng, max: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let n = rng.size(max);
    while out.len() < n {
        let mut piece = Vec::new();
        fuzz::terminal_piece(rng, &mut piece, false);
        let pushes = piece.windows(2).filter(|w| *w == b"[>").count();
        if pushes > 50 {
            continue;
        }
        out.extend(piece);
    }
    out
}

#[test]
fn fuzz_pane_shows_what_the_emulator_shows() {
    fuzz::run("pane_shows_what_the_emulator_shows", 8, |rng| {
        let (cols, rows) = (rng.range(2, 100) as u16, rng.range(1, 40) as u16);
        let scrollback = *rng.pick(&[0, 50, 1000, 3000]);
        let disabled = Arc::new(crate::graphics::Shared::new(false, 0));
        let mut f = fixture(cols, rows, scrollback, disabled);
        let mut r = Reference::new(cols, rows, scrollback);
        let mut resized = false;
        for step in 0..rng.range(1, 30) {
            if rng.one_in(6) {
                // A larger screen (a pane zoomed, a window grown): what each holds is laid out
                // at its width, the screen filled from the history. The history is the same
                // while it is far from full: kraal keeps at least all but what the emulator may
                // hold of its own (a couple of screens and a batch), and drops whole lines where
                // the emulator alone drops rows. (A narrower screen rewraps rows the emulator
                // alone cut at the old width. Behind the alternate screen, the primary one is
                // resized without what the archive holds.)
                if r.term.grid().history_size() + 1300 > scrollback || r.term.mode().contains(TermMode::ALT_SCREEN) {
                    break;
                }
                let (cols, rows) = (r.term.columns(), r.term.screen_lines());
                let (cols, rows) = (rng.range(cols, cols + 60) as u16, rng.range(rows, rows + 30) as u16);
                fuzz::trace(|| {
                    let t = f.pane.term_mut().unwrap();
                    let archived = t.archive.lines().len();
                    format!(
                        "resize {cols}x{rows}: reference history {}, own {}, archived lines {archived}, scrollback {scrollback}",
                        r.term.grid().history_size(),
                        t.own_history(),
                    )
                });
                // kraal ends a synchronized update at a command mark: what it held lands before
                // the new size in both.
                if let Some(t) = f.pane.term_mut() {
                    t.marked(|t| t.stop_sync());
                }
                r.parser.stop_sync(&mut r.term);
                f.pane.resize(cols, rows);
                r.term.resize(Size { cols, rows });
                resized = true;
                let t = f.pane.term().unwrap();
                fuzz::trace(|| {
                    format!(
                        "resized: reference history {} cursor {:?}, own {} cursor {:?}",
                        r.term.grid().history_size(),
                        r.term.grid().cursor.point,
                        t.own_history(),
                        t.term.grid().cursor.point
                    )
                });
                if t.parser.sync_timeout().sync_timeout().is_none() && r.parser.sync_timeout().sync_timeout().is_none()
                {
                    assert_eq!(view(&t.term), view(&r.term), "the screen at step {step}, resized");
                }
            }
            let output = passed_on(rng, 5000);
            for chunk in rng.chunks(&output) {
                fuzz::trace(|| format!("feed {:?}", chunk.escape_ascii().to_string()));
                f.pane.feed(chunk);
            }
            r.feed(&output);
            if rng.one_in(3) {
                f.pane.rest();
            }
            if rng.one_in(5) {
                // Any pending synchronized update lands in both.
                if let Some(t) = f.pane.term_mut() {
                    t.marked(|t| t.stop_sync());
                }
                let r = &mut r;
                r.parser.stop_sync(&mut r.term);
            }
            fuzz::trace(|| {
                let t = f.pane.term_mut().unwrap();
                let width = t.term.columns();
                let archived: usize = t.archive.lines().oldest_first().map(|l| l.rows_at(width)).sum();
                format!(
                    "step {step}: reference history {}, own {}, archived {archived}",
                    r.term.grid().history_size(),
                    t.own_history()
                )
            });
            let t = f.pane.term().unwrap();
            if t.parser.sync_timeout().sync_timeout().is_some() || r.parser.sync_timeout().sync_timeout().is_some() {
                continue;
            }
            assert_eq!(view(&t.term), view(&r.term), "the screen at step {step}");
            // Cell by cell, with what the screen does not show (where lines wrap), until a resize:
            // the emulator alone rewraps rows it rewrapped before, the archive whole lines.
            for l in 0..if resized { 0 } else { t.term.screen_lines() as i32 } {
                let line = alacritty_terminal::index::Line(l);
                let (ours, theirs) = (&t.term.grid()[line], &r.term.grid()[line]);
                // A link without an id is given one by its emulator: only its target counts.
                let cell = |c: &alacritty_terminal::term::cell::Cell| {
                    let link = c.hyperlink().map(|h| h.uri().to_string());
                    (c.c, c.fg, c.bg, c.flags, c.zerowidth().map(<[char]>::to_vec), c.underline_color(), link)
                };
                let differs = (0..t.term.columns())
                    .map(alacritty_terminal::index::Column)
                    .find(|c| cell(&ours[*c]) != cell(&theirs[*c]));
                if let Some(c) = differs {
                    panic!("row {l} at step {step}, column {}: {:?} {:?}", c.0, ours[c], theirs[c]);
                }
            }
            let (ours, theirs) = (&t.term.grid().cursor, &r.term.grid().cursor);
            assert_eq!(ours.input_needs_wrap, theirs.input_needs_wrap, "the wrap pending at step {step}");
            assert_eq!(t.term.mode(), r.term.mode(), "the modes at step {step}");
            assert_eq!(t.term.grid().cursor.point, r.term.grid().cursor.point, "the cursor at step {step}");
            if resized {
                // The archive lays out its lines at the new width as the emulator rewraps its
                // rows, but for the rows a line cut at the old width leaves.
                continue;
            }
            // The history, the newest rows first: kraal drops whole lines where the emulator
            // drops rows.
            let mut ours = t.archived_rows(usize::MAX);
            ours.extend(all_rows(&t.term));
            let mut reference = all_rows(&r.term);
            for rows in [&mut ours, &mut reference] {
                while rows.last().is_some_and(String::is_empty) {
                    rows.pop();
                }
            }
            let n = ours.len().min(reference.len()).saturating_sub(1);
            if let Some(i) = (1..=n).find(|i| ours[ours.len() - i] != reference[reference.len() - i]) {
                let near = |rows: &[String]| rows[rows.len() - (i + 2).min(rows.len())..rows.len() - i + 1].to_vec();
                panic!(
                    "the history at step {step}, {i} rows up of {} and {}:\n{:?}\n{:?}",
                    ours.len(),
                    reference.len(),
                    near(&ours),
                    near(&reference)
                );
            }
        }
    });
}
