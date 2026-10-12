//! The shell's command cycle, as shells mark it with OSC 133 (FinalTerm's marks): where each
//! prompt, command line and output starts, how each command ended and how long it ran.
//!
//! A mark is taken where the cursor is when it arrives, and kept as a line counted from the
//! pane's first line, so that it stays on its text as that scrolls into the history. While the
//! history fills up, its growth gives the lines that scrolled; once it is full, it stays the same
//! length. Then kraal lifts the view one line while output runs: the emulator moves a lifted view
//! along with its text, and how far it moved is how many lines scrolled. Without a history, the
//! lines above the cursor, written already, show how far the screen scrolled.
//!
//! When the text moves in a way kraal cannot follow (a scroll region below the first line, a
//! rewrap to a new width, a history cleared), the marks lose their places but the commands stay,
//! with how each ended and how long it ran.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::TermMode;

use super::{Content, Pane, TermPane};

/// Commands kept per pane, the oldest dropped first.
const MAX_COMMANDS: usize = 1000;
/// Longest command line kept.
const MAX_TEXT: usize = 500;

/// A mark of OSC 133.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mark {
    /// `A`: a prompt starts.
    Prompt,
    /// `B`: the prompt ends and the command line starts.
    Input,
    /// `C`: the command runs; its output starts.
    Output,
    /// `D`: the command ended, with its exit status when the shell gives it.
    End(Option<i32>),
}

/// A place in a pane's text: its line counted from the pane's first line, and its column. The
/// column is one past the last one when the cursor waited there to wrap: the line was full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Spot {
    line: i64,
    col: usize,
}

impl Spot {
    /// A place lost: the text moved in a way kraal could not follow.
    const GONE: Spot = Spot { line: i64::MIN, col: 0 };

    fn gone(self) -> bool {
        self.line == i64::MIN
    }
}

/// The text of the screen's lines and where the cursor was, to tell how far the screen scrolled
/// when there is no history to count with.
struct Screen {
    rows: Vec<Vec<char>>,
    cursor: Point,
}

/// One command, from its prompt on.
#[derive(Debug, Clone)]
pub struct Command {
    prompt: Spot,
    input: Option<Spot>,
    output: Option<Spot>,
    end: Option<Spot>,
    /// The command line as the shell showed it, when the shell marks where it starts.
    pub text: Option<String>,
    pub exit: Option<i32>,
    started: Option<Instant>,
    ended: Option<Instant>,
}

impl Command {
    fn new(prompt: Spot) -> Command {
        Command { prompt, input: None, output: None, end: None, text: None, exit: None, started: None, ended: None }
    }

    /// Whether something ran: Enter on an empty line, or ctrl+c at the prompt, runs nothing.
    pub fn ran(&self) -> bool {
        self.output.is_some()
    }

    pub fn running(&self) -> bool {
        self.output.is_some() && self.ended.is_none()
    }

    pub fn duration(&self) -> Option<Duration> {
        Some(self.ended?.saturating_duration_since(self.started?))
    }
}

/// A command that just ended.
#[derive(Debug, Clone, PartialEq)]
pub struct Finished {
    pub text: Option<String>,
    pub exit: Option<i32>,
    pub duration: Duration,
}

/// The commands of a pane and how its lines have scrolled since the first was marked.
#[derive(Debug, Default)]
pub struct Commands {
    list: VecDeque<Command>,
    /// Lines that scrolled into the history while marks were kept: grid line `l` is pane line
    /// `scrolled + l`.
    scrolled: i64,
    /// Length of the primary screen's history when last counted, less the rows the archive took
    /// from its top since and plus those it put back (see `archive.rs`).
    pub(super) history_seen: i64,
    /// The view's offset when kraal started counting with it, and whether kraal lifted it from
    /// the bottom (to put back). Kept while the alternate screen is in front: the primary
    /// screen's view only moves again once it is back.
    lifted: Option<(usize, bool)>,
    /// The emulator's history limit.
    history_max: usize,
    /// When a long command ended in this pane, until someone looks at it.
    pub unseen: Option<Instant>,
}

impl Commands {
    pub(super) fn new(history_max: usize) -> Commands {
        // The emulator never keeps more than this, whatever it is asked.
        Commands { history_max: history_max.min(100_000), ..Commands::default() }
    }

    /// Lose the places of the marks: the commands stay, with how they ended and how long they
    /// ran, but none points at a line any more.
    fn unplace(&mut self) {
        for c in &mut self.list {
            c.prompt = Spot::GONE;
            for spot in [&mut c.input, &mut c.output, &mut c.end].into_iter().flatten() {
                *spot = Spot::GONE;
            }
        }
        self.lifted = None;
    }
}

impl TermPane {
    /// Feed the emulator. While the pane has marks, the lines that scroll into the history are
    /// counted, so that the marks stay on their text.
    pub(super) fn advance(&mut self, bytes: &[u8]) {
        for piece in self.graphics.split(bytes) {
            match piece {
                crate::graphics::Piece::Text(text) => {
                    let text = self.guard.filter(&text);
                    self.marked(|t| t.parse(&text));
                }
                image => self.marked(|t| {
                    // Text held for a synchronized update goes first: the image comes after it.
                    if t.parser.sync_timeout().sync_timeout().is_some() {
                        t.stop_sync();
                    }
                    // Images shown in the archived history are still placed.
                    t.graphics.archived = t.archive.lines().images().collect();
                    if let Some(reply) = t.graphics.handle(image, &mut t.term) {
                        t.write(reply);
                    }
                }),
            }
        }
    }

    /// Change the grid with `change`, keeping count of the lines it scrolls so that the marks
    /// stay on their lines.
    pub(super) fn marked(&mut self, change: impl FnOnce(&mut Self)) {
        if self.commands.list.is_empty() {
            change(self);
            self.guard.take_top_margin();
            return;
        }
        let screen = (self.commands.history_max == 0 && !self.alt_screen()).then(|| self.screen());
        if !self.alt_screen() && self.commands.lifted.is_none() {
            let grid = self.term.grid();
            let (offset, history) = (grid.display_offset(), grid.history_size());
            if offset > 0 {
                self.commands.lifted = Some((offset, false));
            } else if history > 0 {
                // The grid's own scroll, not the terminal's: that one would move the copy-mode
                // cursor and its selection.
                self.term.grid_mut().scroll_display(Scroll::Delta(1));
                self.commands.lifted = Some((1, true));
            }
        }
        change(self);
        if !self.alt_screen() {
            self.count_scrolled(screen);
        }
        // A region below the first line scrolls its lines without putting them in the history:
        // nothing tells how far they went.
        if self.guard.take_top_margin() {
            self.commands.unplace();
        }
    }

    /// The text of the screen's lines, one character per cell, and the cursor.
    fn screen(&self) -> Screen {
        let grid = self.term.grid();
        let rows = (0..self.term.screen_lines() as i32)
            .map(|l| {
                let row = &grid[Line(l)];
                (0..self.term.columns()).map(|c| row[Column(c)].c).collect()
            })
            .collect();
        Screen { rows, cursor: grid.cursor.point }
    }

    /// How far the screen scrolled since `before`, read from the text that was above the cursor
    /// then (written already, it only moves): the one shift that puts it where it is now. None
    /// when no shift, or more than one, fits, or when there was no such text to read.
    fn shift_since(&self, before: &Screen) -> Option<usize> {
        let now = self.screen();
        let cursor = before.cursor.line.0.max(0) as usize;
        let col = before.cursor.column.0;
        let rows = &before.rows[..cursor.min(before.rows.len())];
        let typed = before.rows.get(cursor).map(|r| &r[..col.min(r.len())]).unwrap_or_default();
        let written = |r: &[char]| r.iter().any(|c| *c != ' ');
        let mut found = None;
        for shift in 0..=cursor {
            // Blank lines fit any shift: only written text tells one from another.
            if !rows.iter().skip(shift).any(|r| written(r)) && !written(typed) {
                continue;
            }
            let rows_fit = rows.iter().enumerate().skip(shift).all(|(i, r)| now.rows.get(i - shift) == Some(r));
            let typed_fits = now.rows.get(cursor - shift).is_some_and(|r| r.starts_with(typed));
            if rows_fit && typed_fits {
                if found.is_some() {
                    return None;
                }
                found = Some(shift);
            }
        }
        found
    }

    fn alt_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    /// Count the lines that scrolled into the history since the last count (or, without a
    /// history, since the screen was `before`), and put a lifted view back at the bottom. Marks
    /// that cannot be placed any more lose their places: the history was cleared, or more lines
    /// went through it than it holds.
    fn count_scrolled(&mut self, before: Option<Screen>) {
        if self.commands.list.is_empty() {
            return;
        }
        let shifted = before.and_then(|b| self.shift_since(&b));
        let (history, offset) = (self.own_history(), self.term.grid().display_offset());
        let c = &mut self.commands;
        let lifted = c.lifted.take();
        let moved = if c.history_max == 0 {
            shifted
        } else if history < c.history_max && history as i64 >= c.history_seen {
            Some((history as i64 - c.history_seen) as usize)
        } else {
            match lifted {
                Some((from, _)) if offset >= from && offset < c.history_max => Some(offset - from),
                _ => None,
            }
        };
        c.history_seen = history as i64;
        match moved {
            Some(n) => c.scrolled += n as i64,
            None => c.unplace(),
        }
        if let Some((_, true)) = lifted {
            self.term.grid_mut().scroll_display(Scroll::Bottom);
            // Output scrolling under a lifted view may leave the copy-mode cursor a line above
            // the bottom view; it belongs at its top.
            let cursor = &mut self.term.vi_mode_cursor.point;
            cursor.line = cursor.line.max(Line(0));
        }
    }

    /// The emulator resized: the text may have moved between the screen and the history. Marks
    /// keep their distance to the cursor when only the height changed; a new width rewraps the
    /// lines, and they lose their places.
    pub(super) fn resize_marked(&mut self, size: super::pty::Size) {
        let before = self.term.grid().cursor.point.line.0;
        let width = self.term.columns();
        let alt = self.alt_screen();
        self.term.resize(size);
        // The emulator's scroll region is the whole screen again.
        self.guard.screen_resized();
        if self.commands.list.is_empty() {
            return;
        }
        if alt || usize::from(size.cols) != width {
            // Rewrapped (on the alternate screen, the primary one changes out of sight).
            self.commands.unplace();
            self.commands.history_seen = self.own_history() as i64;
            return;
        }
        let after = self.term.grid().cursor.point.line.0;
        self.commands.scrolled += i64::from(before - after);
        self.commands.history_seen = self.own_history() as i64;
        self.commands.lifted = None;
    }

    /// A mark from the shell, where the cursor is. Returns the command it ends, if any.
    pub(super) fn mark(&mut self, mark: Mark) -> Option<Finished> {
        // A synchronized update holds output back: what came before the mark must be on the grid.
        if self.parser.sync_timeout().sync_timeout().is_some() {
            self.marked(|t| t.stop_sync());
        }
        // Full screen programs have no commands; the primary screen's are out of sight.
        if self.alt_screen() {
            return None;
        }
        if self.commands.list.is_empty() {
            self.commands.history_seen = self.own_history() as i64;
            self.commands.lifted = None;
        }
        let spot = self.cursor_spot();
        let now = Instant::now();
        match mark {
            Mark::Prompt => {
                let list = &mut self.commands.list;
                // The same prompt drawn again (a redraw, or a shell and its prompt theme both
                // marking it): one command.
                if let Some(last) = list.back_mut().filter(|c| !c.ran() && c.prompt.line == spot.line) {
                    *last = Command::new(spot);
                } else {
                    list.push_back(Command::new(spot));
                    if list.len() > MAX_COMMANDS {
                        list.pop_front();
                    }
                }
            }
            Mark::Input => {
                if let Some(last) = self.commands.list.back_mut().filter(|c| !c.ran()) {
                    last.input = Some(spot);
                }
            }
            Mark::Output => {
                // Already running: the same mark twice (a shell and a plugin both sending it).
                if self.commands.list.back().is_some_and(|c| c.running()) {
                    return None;
                }
                if self.commands.list.back().is_none_or(|c| c.ran()) {
                    // A shell that only marks commands: the command starts here.
                    self.commands.list.push_back(Command::new(spot));
                    if self.commands.list.len() > MAX_COMMANDS {
                        self.commands.list.pop_front();
                    }
                }
                let text =
                    self.commands.list.back().and_then(|c| c.input).and_then(|from| self.text_between(from, spot));
                let last = self.commands.list.back_mut()?;
                last.output = Some(spot);
                last.started = Some(now);
                last.text = text.map(|t| {
                    let t: String = t.split_whitespace().collect::<Vec<_>>().join(" ");
                    crate::text::truncate(&t, MAX_TEXT)
                });
            }
            Mark::End(exit) => {
                let last = self.commands.list.back_mut().filter(|c| c.running())?;
                last.end = Some(spot);
                last.exit = exit;
                last.ended = Some(now);
                return Some(Finished { text: last.text.clone(), exit, duration: last.duration().unwrap_or_default() });
            }
        }
        None
    }

    /// The cursor as a spot: past the last column while it waits to wrap.
    fn cursor_spot(&self) -> Spot {
        let cursor = &self.term.grid().cursor;
        let col = cursor.point.column.0 + usize::from(cursor.input_needs_wrap);
        Spot { line: self.commands.scrolled + i64::from(cursor.point.line.0), col }
    }

    /// Where a spot is on the grid now, if its line is still there. A spot past the last column
    /// is where the next line starts.
    fn point(&self, spot: Spot) -> Option<Point> {
        if spot.gone() {
            return None;
        }
        let columns = self.term.columns();
        let (line, col) = if spot.col >= columns { (spot.line + 1, 0) } else { (spot.line, spot.col) };
        let line = line - self.commands.scrolled;
        let top = -(self.own_history() as i64);
        let bottom = self.term.screen_lines() as i64;
        (top..bottom).contains(&line).then(|| Point::new(Line(line as i32), Column(col)))
    }

    /// The point just before `spot`: the end of the previous line when it starts a line.
    fn before(&self, spot: Spot) -> Option<Point> {
        if spot.gone() {
            return None;
        }
        if spot.col > 0 {
            return self.point(Spot { line: spot.line, col: spot.col - 1 });
        }
        let p = self.point(Spot { line: spot.line - 1, col: 0 })?;
        Some(Point::new(p.line, Column(self.term.columns().saturating_sub(1))))
    }

    /// The text from `from` up to `to` (not included); None when nothing of it is left.
    fn text_between(&self, from: Spot, to: Spot) -> Option<String> {
        if from.gone() || to.gone() || (to.line, to.col) <= (from.line, from.col) {
            return None;
        }
        let start = self.point(from).or_else(|| self.point(Spot { line: self.top_line(), col: 0 }))?;
        let end = self.before(to)?;
        (start <= end).then(|| self.term.bounds_to_string(start, end))
    }

    /// The pane line of the oldest line still in the history.
    fn top_line(&self) -> i64 {
        self.commands.scrolled - self.own_history() as i64
    }

    /// Bring the rows down to `spot` back from the archive when it is above the emulator's
    /// history. Whether it is on the grid now.
    fn bring_back(&mut self, spot: Spot) -> bool {
        if !spot.gone() && spot.line < self.top_line() {
            self.fill_lens();
            let above = (self.top_line() - spot.line).max(0) as usize;
            self.bring_back_rows(self.term.grid().history_size() + above);
        }
        self.point(spot).is_some()
    }

    /// The output of a command so far: up to its end, or to the cursor while it runs. Lines
    /// gone from the history are left out.
    fn output_text(&self, c: &Command) -> Option<String> {
        let from = c.output.filter(|s| !s.gone())?;
        let to = c.end.unwrap_or_else(|| self.cursor_spot());
        if to.gone() {
            return None;
        }
        if (to.line, to.col) <= (from.line, from.col) {
            return Some(String::new());
        }
        let text = self.text_between(from, to)?;
        Some(text.trim_end_matches(['\n', ' ']).to_string())
    }
}

impl Pane {
    fn commands(&self) -> Option<&Commands> {
        match &self.content {
            Content::Term(t) => Some(&t.commands),
            _ => None,
        }
    }

    /// Whether the shell in this pane marks its prompts.
    pub fn has_prompts(&self) -> bool {
        self.commands().is_some_and(|c| !c.list.is_empty())
    }

    /// The last command that ran, unless it is running.
    pub fn last_command(&self) -> Option<&Command> {
        self.commands()?.list.iter().rev().find(|c| c.ran()).filter(|c| !c.running())
    }

    /// The exit status of the last command that ran, when it failed, and how long it ran.
    pub fn last_failure(&self) -> Option<(i32, Option<Duration>)> {
        let c = self.last_command()?;
        c.exit.filter(|e| *e != 0).map(|e| (e, c.duration()))
    }

    /// The last command's line and output: the one running, or the last that ran.
    pub fn last_command_output(&mut self) -> Option<(Option<String>, String)> {
        let t = self.term_mut()?;
        let c = t.commands.list.iter().rev().find(|c| c.ran())?.clone();
        if let Some(from) = c.output {
            t.bring_back(from);
        }
        Some((c.text.clone(), t.output_text(&c)?))
    }

    /// When a long command ended here unseen.
    pub fn command_unseen(&self) -> Option<Instant> {
        self.commands()?.unseen
    }

    pub fn set_command_unseen(&mut self, unseen: bool) -> bool {
        let Content::Term(t) = &mut self.content else { return false };
        let was = t.commands.unseen.is_some();
        t.commands.unseen = unseen.then(Instant::now);
        was != unseen
    }

    /// Copy mode: move the cursor to the start of the prompt above it (or below). Returns
    /// whether there was one.
    pub fn jump_to_prompt(&mut self, up: bool) -> bool {
        let Some(t) = self.term_mut() else { return false };
        // Marks are found on the history whole.
        t.fill_lens();
        let at = t.term.vi_mode_cursor.point.line;
        let target = |t: &TermPane| {
            let prompts = t.commands.list.iter().filter_map(|c| t.point(c.prompt)).map(|p| p.line);
            if up { prompts.filter(|l| *l < at).max() } else { prompts.filter(|l| *l > at).min() }
        };
        let mut found = target(t);
        if found.is_none() && up {
            // The prompt above may have gone to the archive.
            let top = t.top_line();
            let archived = t.commands.list.iter().rev().map(|c| c.prompt).find(|p| !p.gone() && p.line < top);
            if let Some(prompt) = archived
                && t.bring_back(prompt)
            {
                found = target(t);
            }
        }
        let Some(line) = found else { return false };
        t.term.vi_goto_point(Point::new(line, Column(0)));
        true
    }

    /// Copy mode: select the output of the command around the cursor, or with `whole` its
    /// prompt and command line too. Returns whether there was one.
    pub fn select_command(&mut self, whole: bool) -> bool {
        let Some(t) = self.term_mut() else { return false };
        t.fill_lens();
        let at = t.term.vi_mode_cursor.point.line;
        let above = |c: &&Command| !c.prompt.gone() && t.point(c.prompt).is_none_or(|p| p.line <= at);
        let Some(c) = t.commands.list.iter().rev().find(|c| c.ran() && above(c)).cloned() else {
            return false;
        };
        let first = if whole { c.prompt } else { c.output.unwrap_or(c.prompt) };
        t.bring_back(first);
        let start = t.point(first).or_else(|| t.point(Spot { line: t.top_line(), col: 0 }));
        let end = match c.end {
            Some(end) => t.before(end),
            None => Some(t.term.grid().cursor.point),
        };
        let (Some(start), Some(end)) = (start, end) else { return false };
        if end.line < start.line {
            return false;
        }
        let (start, end) = (Point::new(start.line, Column(0)), Point::new(end.line, Column(0)));
        // The cursor at the top of what is selected, in view; moving it moves that edge. (Moving
        // the view after would pull the cursor, and the selection with it, into the view.)
        t.term.selection = None;
        t.term.vi_goto_point(start);
        let mut sel = Selection::new(SelectionType::Lines, end, Side::Right);
        sel.update(start, Side::Left);
        t.term.selection = Some(sel);
        true
    }
}

/// A duration as people read it: 45s, 3m05s, 1h02m.
pub fn short_duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::super::osc::{Osc, OscScanner};
    use super::super::{PaneEnv, PaneSignal, PaneSpec};
    use super::*;

    /// A pane whose program prints nothing: the tests feed it output themselves.
    fn pane(rows: u16, scrollback: usize) -> (Pane, crossbeam_channel::Receiver<crate::server::Event>) {
        let (tx, rx) = crossbeam_channel::bounded(64);
        let env = PaneEnv {
            socket: "/nonexistent".into(),
            session: "t".into(),
            shell: "sh".into(),
            scrollback,
            graphics: Default::default(),
        };
        let spec = PaneSpec { command: vec!["sleep".into(), "30".into()], ..Default::default() };
        (Pane::spawn(1, None, spec, 30, rows, &env, &tx).unwrap(), rx)
    }

    /// A whole command as zsh with kraal's integration prints it.
    fn command(line: &str, output: &[&str], status: i32) -> String {
        let mut s = format!("\x1b]133;A\x07$ \x1b]133;B\x07{line}\r\n\x1b]133;C\x07");
        for o in output {
            s.push_str(o);
            s.push_str("\r\n");
        }
        s.push_str(&format!("\x1b]133;D;{status}\x07"));
        s
    }

    fn cursor_line_text(pane: &Pane) -> String {
        let t = pane.term().unwrap();
        let line = t.term.vi_mode_cursor.point.line;
        let row = &t.term.grid()[line];
        (0..t.term.columns()).map(|c| row[Column(c)].c).collect::<String>().trim_end().to_string()
    }

    #[test]
    fn marks_are_read_whole_across_reads_and_located_in_the_output() {
        let mut s = OscScanner::default();
        let data = b"ab\x1b]133;A;cl=m\x07$ \x1b]133;B\x1b\\x\x1b]133;D;127;aid=7\x07\x1b]133;D\x07\x1b]133;Z\x07";
        let mut found = Vec::new();
        let mut base = 0;
        for chunk in data.chunks(4) {
            found.extend(s.scan(chunk).into_iter().map(|(end, o)| (base + end, o)));
            base += chunk.len();
        }
        let marks: Vec<(usize, Osc)> = vec![
            (15, Osc::Mark(Mark::Prompt)),
            (26, Osc::Mark(Mark::Input)),
            (45, Osc::Mark(Mark::End(Some(127)))),
            (53, Osc::Mark(Mark::End(None))),
        ];
        assert_eq!(found, marks);
        // Each offset is just past the end of its sequence.
        assert_eq!(&data[..15], b"ab\x1b]133;A;cl=m\x07");
    }

    #[test]
    fn a_command_has_its_line_its_output_and_its_status() {
        let (mut p, _rx) = pane(10, 100);
        let signals = p.feed(command("ls -l", &["file1", "file2"], 0).as_bytes());
        let done: Vec<&Finished> = signals
            .iter()
            .filter_map(|s| match s {
                PaneSignal::CommandDone(f) => Some(f),
                _ => None,
            })
            .collect();
        assert_eq!(done.len(), 1);
        assert_eq!((done[0].text.as_deref(), done[0].exit), (Some("ls -l"), Some(0)));
        assert_eq!(p.last_command_output(), Some((Some("ls -l".into()), "file1\nfile2".into())));
        assert_eq!(p.last_failure(), None);
        p.feed(command("false", &[], 1).as_bytes());
        assert_eq!(p.last_failure().map(|f| f.0), Some(1));
        assert_eq!(p.last_command_output(), Some((Some("false".into()), String::new())));
        // A new prompt keeps the failure in view; Enter on an empty line runs nothing.
        p.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07\r\n\x1b]133;D;1\x07\x1b]133;A\x07$ ");
        assert_eq!(p.last_failure().map(|f| f.0), Some(1));
        // While a command runs, nothing failed yet; its output so far can be copied.
        p.feed(b"\x1b]133;B\x07make\r\n\x1b]133;C\x07building\r\n");
        assert_eq!(p.last_failure(), None);
        assert_eq!(p.last_command_output(), Some((Some("make".into()), "building".into())));
        p.kill();
    }

    #[test]
    fn marks_stay_on_their_text_as_it_scrolls_through_a_full_history() {
        let (mut p, _rx) = pane(4, 10);
        // Fill the history first, then two commands.
        for i in 0..30 {
            p.feed(format!("filler {i}\r\n").as_bytes());
        }
        p.feed(command("first", &["one", "two"], 0).as_bytes());
        let lines: Vec<String> = (0..7).map(|i| format!("out {i}")).collect();
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        // Fed a byte at a time and in one piece: the count holds either way.
        for b in command("second", &lines, 3).as_bytes() {
            p.feed(&[*b]);
        }
        p.feed(b"\x1b]133;A\x07$ ");
        p.copy_mode(true);
        assert!(p.jump_to_prompt(true));
        assert_eq!(cursor_line_text(&p), "$ second");
        assert!(p.jump_to_prompt(true));
        assert_eq!(cursor_line_text(&p), "$ first");
        assert!(!p.jump_to_prompt(true), "the fillers have no prompt");
        assert!(p.jump_to_prompt(false));
        assert_eq!(cursor_line_text(&p), "$ second");
        // The output of the command above the cursor, then the whole of it.
        assert!(p.select_command(false));
        assert_eq!(p.selection_text().unwrap().trim_end(), lines.join("\n"));
        p.copy_mode(false);
        // More output than the history holds: the prompts' lines are gone, and so are they; then
        // one feed of more lines than it holds.
        for i in 0..40 {
            p.feed(format!("flood {i}\r\n").as_bytes());
        }
        p.copy_mode(true);
        assert!(!p.jump_to_prompt(true));
        p.copy_mode(false);
        p.feed(command("again", &["z"], 0).as_bytes());
        p.feed("big\r\n".repeat(50).as_bytes());
        p.feed(command("last", &["w"], 0).as_bytes());
        p.feed(b"\x1b]133;A\x07$ ");
        p.copy_mode(true);
        assert!(p.jump_to_prompt(true));
        assert_eq!(cursor_line_text(&p), "$ last");
        assert!(!p.jump_to_prompt(true), "{}", cursor_line_text(&p));
        p.kill();
    }

    #[test]
    fn a_full_screen_program_and_a_resize_leave_the_marks_in_place() {
        let (mut p, _rx) = pane(6, 50);
        p.feed(command("vim notes", &["saved"], 0).as_bytes());
        // A program on the alternate screen scrolls there, and marks nothing.
        p.feed(b"\x1b[?1049h");
        for i in 0..20 {
            p.feed(format!("vim {i}\r\n\x1b]133;A\x07").as_bytes());
        }
        p.feed(b"\x1b[?1049l");
        p.feed(command("ls", &["a", "b", "c"], 0).as_bytes());
        p.feed(b"\x1b]133;A\x07$ ");
        p.resize(30, 3);
        p.copy_mode(true);
        assert!(p.jump_to_prompt(true));
        assert_eq!(cursor_line_text(&p), "$ ls");
        assert!(p.jump_to_prompt(true));
        assert_eq!(cursor_line_text(&p), "$ vim notes");
        assert!(!p.jump_to_prompt(true));
        p.copy_mode(false);
        assert_eq!(p.last_command_output(), Some((Some("ls".into()), "a\nb\nc".into())));
        p.kill();
    }

    #[test]
    fn a_lifted_view_goes_back_down_and_a_scrolled_one_stays() {
        let (mut p, _rx) = pane(4, 20);
        for i in 0..10 {
            p.feed(format!("line {i}\r\n").as_bytes());
        }
        p.feed(command("x", &["y"], 0).as_bytes());
        p.feed(b"more\r\nmore\r\n");
        assert_eq!(p.display_offset(), 0, "output at the bottom stays at the bottom");
        p.scroll(2);
        p.feed(b"more\r\n");
        assert_eq!(p.display_offset(), 3, "a view scrolled back stays on its text");
        p.kill();
    }

    #[test]
    fn output_held_by_a_synchronized_update_counts_when_it_lands() {
        let (mut p, _rx) = pane(4, 20);
        for i in 0..40 {
            p.feed(format!("filler {i}\r\n").as_bytes());
        }
        p.feed(command("first", &["one"], 0).as_bytes());
        // Held, then flushed by the mark that ends the command.
        p.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07second\r\n\x1b]133;C\x07\x1b[?2026h");
        p.feed(b"o1\r\no2\r\no3\r\no4\r\no5\r\n\x1b]133;D;0\x07\x1b[?2026l");
        // Held, then flushed when the update times out.
        p.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07third\r\n\x1b]133;C\x07\x1b[?2026h");
        p.feed(b"t1\r\nt2\r\nt3\r\nt4\r\nt5\r\n");
        std::thread::sleep(Duration::from_millis(300));
        assert!(p.poll_sync_timeout().is_some());
        p.feed(b"\x1b[?2026l\x1b]133;D;0\x07\x1b]133;A\x07$ ");
        p.copy_mode(true);
        assert!(p.jump_to_prompt(true));
        assert_eq!(cursor_line_text(&p), "$ third");
        assert!(p.jump_to_prompt(true));
        assert_eq!(cursor_line_text(&p), "$ second");
        assert!(p.jump_to_prompt(true));
        assert_eq!(cursor_line_text(&p), "$ first");
        p.copy_mode(false);
        p.kill();
    }

    #[test]
    fn a_region_below_the_first_line_loses_the_places_not_the_commands() {
        let (mut p, _rx) = pane(5, 10);
        for i in 0..30 {
            p.feed(format!("filler {i}\r\n").as_bytes());
        }
        p.feed(command("make", &["error"], 3).as_bytes());
        // Lines scroll inside rows 2 to 5: none goes into the history.
        p.feed(b"\x1b[2;5r\x1b[5;1Ha\r\nb\r\nc\r\n\x1b[r\x1b[5;1H\x1b]133;A\x07$ ");
        p.copy_mode(true);
        assert!(!p.jump_to_prompt(true), "no prompt left on a wrong line: {}", cursor_line_text(&p));
        p.copy_mode(false);
        assert_eq!(p.last_failure().map(|f| f.0), Some(3));
        // Marks after it are placed again.
        p.feed(b"\x1b]133;B\x07ls\r\n\x1b]133;C\x07x\r\n\x1b]133;D;0\x07\x1b]133;A\x07$ ");
        p.copy_mode(true);
        assert!(p.jump_to_prompt(true));
        assert_eq!(cursor_line_text(&p), "$ ls");
        p.copy_mode(false);
        p.kill();
    }

    #[test]
    fn without_a_history_the_screen_says_how_far_it_scrolled() {
        let (mut p, _rx) = pane(6, 0);
        p.feed(b"intro\r\n");
        p.feed(command("first", &["one", "two"], 0).as_bytes());
        p.feed(command("second", &["x", "y", "z"], 0).as_bytes());
        p.feed(b"\x1b]133;A\x07$ ");
        p.copy_mode(true);
        assert!(p.jump_to_prompt(true));
        assert_eq!(cursor_line_text(&p), "$ second");
        assert!(!p.jump_to_prompt(true), "the first prompt scrolled away");
        assert!(p.select_command(false));
        assert_eq!(p.selection_text().unwrap().trim_end(), "x\ny\nz");
        p.copy_mode(false);
        assert_eq!(p.last_command_output(), Some((Some("second".into()), "x\ny\nz".into())));
        p.kill();
    }

    #[test]
    fn a_new_width_loses_the_places_and_keeps_how_commands_ended() {
        let (mut p, _rx) = pane(6, 50);
        p.feed(command("ls", &["a long line that the new width will wrap"], 2).as_bytes());
        p.feed(b"\x1b]133;A\x07$ ");
        p.resize(12, 6);
        p.copy_mode(true);
        assert!(!p.jump_to_prompt(true));
        p.copy_mode(false);
        assert_eq!(p.last_failure().map(|f| f.0), Some(2));
        assert_eq!(p.last_command_output(), None, "nothing copied from a wrong line");
        p.feed(b"\x1b]133;B\x07pwd\r\n\x1b]133;C\x07/\r\n\x1b]133;D;0\x07\x1b]133;A\x07$ ");
        assert_eq!(p.last_command_output(), Some((Some("pwd".into()), "/".into())));
        p.kill();
    }

    #[test]
    fn output_that_fills_its_last_line_is_copied_whole() {
        let (mut p, _rx) = pane(6, 50);
        let full = "x".repeat(30);
        p.feed(format!("\x1b]133;A\x07$ \x1b]133;B\x07cmd\r\n\x1b]133;C\x07{full}\x1b]133;D;0\x07\r\n").as_bytes());
        assert_eq!(p.last_command_output(), Some((Some("cmd".into()), full)));
        p.kill();
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(short_duration(Duration::from_secs(45)), "45s");
        assert_eq!(short_duration(Duration::from_secs(185)), "3m05s");
        assert_eq!(short_duration(Duration::from_secs(3720)), "1h02m");
    }
}
