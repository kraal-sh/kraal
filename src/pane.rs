//! A pane: a process on a PTY with its terminal emulator, or a rich preview block.

use std::cell::RefCell;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Instant;

use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Direction, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::search::{Match, RegexSearch};
use alacritty_terminal::term::{Config as TermConfig, Term, TermMode};
use alacritty_terminal::vi_mode::ViMotion;
use alacritty_terminal::vte::ansi::{Color as AColor, CursorShape as ACursorShape, NamedColor, Processor, Rgb};
use anyhow::{Context, Result};
use crossbeam_channel::Sender;
use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};

use crate::agent::AgentTracker;
use crate::ansi::{Cursor, CursorShape};
use crate::input::PaneInputModes;
use crate::layout::PaneId;
use crate::preview::Preview;
use crate::server::Event;

/// What to run in a pane; kept so the pane can be restarted and persisted.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PaneSpec {
    /// Empty = the user's shell.
    pub command: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub close_on_exit: bool,
    pub preview: Option<PathBuf>,
    /// Typed into the shell once it starts (used to resume agents after a restore).
    pub startup_input: Option<String>,
    /// Inventory name of the server this pane connects to, when kraal started the connection.
    pub server: Option<String>,
    /// Close the pane a few seconds after its command succeeds (transfers).
    pub close_on_success: bool,
    /// A two-panel file manager on these locations (`/path` or `server:path`).
    pub files: Option<[String; 2]>,
    /// Extra environment for the process, and variables to remove.
    pub env: Vec<(String, String)>,
    pub env_remove: Vec<String>,
    /// `agent:account` the pane's agent runs on, when kraal launched it.
    pub account: Option<String>,
    /// Conversation id the agent was started with, for resuming it.
    pub session: Option<String>,
    /// The git worktree kraal created for this pane's agent.
    pub worktree: Option<crate::git::Worktree>,
}

#[derive(Clone, Default)]
pub struct Listener(Rc<RefCell<Vec<TermEvent>>>);

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        self.0.borrow_mut().push(event);
    }
}

#[derive(Clone, Copy)]
struct Size {
    cols: u16,
    rows: u16,
}

impl Dimensions for Size {
    fn total_lines(&self) -> usize {
        self.rows as usize
    }
    fn screen_lines(&self) -> usize {
        self.rows as usize
    }
    fn columns(&self) -> usize {
        self.cols as usize
    }
}

/// Things the server must act on after feeding output to a pane.
#[derive(Debug, Clone)]
pub enum PaneSignal {
    Notification { title: String, body: String },
    Clipboard(String),
    Bell,
    TitleChanged,
}

pub struct TermPane {
    pub term: Term<Listener>,
    events: Rc<RefCell<Vec<TermEvent>>>,
    parser: Processor,
    master: Box<dyn MasterPty + Send>,
    input: Sender<Vec<u8>>,
    /// Output read from the PTY, waiting to be parsed. Small and bounded: a program printing
    /// faster than kraal parses is slowed down here, alone, without delaying other panes.
    output: crossbeam_channel::Receiver<Vec<u8>>,
    killer: Box<dyn ChildKiller + Send + Sync>,
    pub pid: Option<u32>,
    osc: OscScanner,
}

pub enum Content {
    Term(Box<TermPane>),
    Preview(Box<Preview>),
    Files(Box<crate::files::FilesPane>),
}

pub struct Pane {
    pub id: PaneId,
    pub name: Option<String>,
    /// Title set by the program (OSC 0/2).
    pub title: String,
    pub spec: PaneSpec,
    pub content: Content,
    pub cols: u16,
    pub rows: u16,
    pub agent: AgentTracker,
    /// Set when the process exited; the pane stays open for command panes.
    pub exit_code: Option<Option<i32>>,
    pub last_output: Instant,
    pub bell: bool,
    /// Current directory reported by the shell (OSC 7).
    pub reported_cwd: Option<PathBuf>,
    /// The server this pane is connected to: from its command, or an `ssh` seen in the foreground.
    pub ssh: Option<crate::servers::Server>,
    /// When to close the pane automatically.
    pub close_at: Option<Instant>,
    /// Replace the spec and restart once the current process exits (account switch).
    pub restart_with: Option<PaneSpec>,
    /// The command has not been started yet (restored session, `start_suspended`, or it failed
    /// to start): Enter starts it.
    pub suspended: bool,
    search: Option<SearchState>,
}

struct SearchState {
    regex: RegexSearch,
    current: Option<Match>,
}

/// Escape regex metacharacters so a plain query matches literally.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if "\\.+*?()|[]{}^$#&-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Environment given to every pane process.
pub struct PaneEnv {
    pub socket: PathBuf,
    pub session: String,
    pub shell: String,
    pub scrollback: usize,
}

impl Pane {
    pub fn spawn(
        id: PaneId,
        name: Option<String>,
        spec: PaneSpec,
        cols: u16,
        rows: u16,
        env: &PaneEnv,
        tx: &Sender<Event>,
    ) -> Result<Pane> {
        let cols = cols.max(2);
        let rows = rows.max(1);
        let content = if let Some(path) = &spec.preview {
            Content::Preview(Box::new(Preview::open(path)))
        } else {
            Content::Term(Box::new(TermPane::spawn(id, &spec, cols, rows, env, tx)?))
        };
        Ok(Pane {
            id,
            name,
            title: String::new(),
            spec,
            content,
            cols,
            rows,
            agent: AgentTracker::default(),
            exit_code: None,
            last_output: Instant::now(),
            bell: false,
            reported_cwd: None,
            ssh: None,
            close_at: None,
            restart_with: None,
            suspended: false,
            search: None,
        })
    }

    /// A file manager pane.
    pub fn files(id: PaneId, name: Option<String>, spec: PaneSpec, files: crate::files::FilesPane) -> Pane {
        Pane {
            id,
            name,
            title: String::new(),
            spec,
            content: Content::Files(Box::new(files)),
            cols: 80,
            rows: 24,
            agent: AgentTracker::default(),
            exit_code: None,
            last_output: Instant::now(),
            bell: false,
            reported_cwd: None,
            ssh: None,
            close_at: None,
            restart_with: None,
            suspended: false,
            search: None,
        }
    }

    /// Restart the command of an exited pane, keeping its scrollback.
    pub fn respawn(&mut self, env: &PaneEnv, tx: &Sender<Event>) -> Result<()> {
        let Content::Term(old) = &mut self.content else { return Ok(()) };
        let mut fresh = TermPane::spawn(self.id, &self.spec, self.cols, self.rows, env, tx)?;
        // Keep history: move the old emulator over and reset its modes.
        std::mem::swap(&mut fresh.term, &mut old.term);
        std::mem::swap(&mut fresh.events, &mut old.events);
        fresh.parser.advance(
            &mut fresh.term,
            b"\x1b[0m\x1b[?25h\x1b[?1049l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\r\n",
        );
        **old = fresh;
        self.exit_code = None;
        self.suspended = false;
        self.agent = AgentTracker::default();
        Ok(())
    }

    pub fn display_name(&self) -> String {
        if let Some(n) = &self.name {
            return n.clone();
        }
        match &self.content {
            Content::Preview(p) => p.title(),
            Content::Files(f) => f.title(),
            Content::Term(_) if !self.title.is_empty() => self.title.clone(),
            Content::Term(_) => self
                .spec
                .command
                .first()
                .map(|c| c.rsplit('/').next().unwrap_or(c).to_string())
                .unwrap_or_else(|| "shell".into()),
        }
    }

    pub fn is_preview(&self) -> bool {
        matches!(self.content, Content::Preview(_))
    }

    /// A process on a PTY (as opposed to a preview or a file manager).
    pub fn is_terminal(&self) -> bool {
        matches!(self.content, Content::Term(_))
    }

    pub fn term(&self) -> Option<&TermPane> {
        match &self.content {
            Content::Term(t) => Some(t),
            _ => None,
        }
    }

    pub fn term_mut(&mut self) -> Option<&mut TermPane> {
        match &mut self.content {
            Content::Term(t) => Some(t),
            _ => None,
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        let cols = cols.max(2);
        let rows = rows.max(1);
        if (cols, rows) == (self.cols, self.rows) {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        if let Content::Term(t) = &mut self.content {
            let _ = t.master.resize(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 });
            t.term.resize(Size { cols, rows });
        }
    }

    /// Feed process output. Returns signals for the server.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<PaneSignal> {
        self.last_output = Instant::now();
        let Content::Term(t) = &mut self.content else { return Vec::new() };
        let mut signals = Vec::new();
        for osc in t.osc.scan(bytes) {
            match osc {
                Osc::Notify { title, body } => signals.push(PaneSignal::Notification { title, body }),
                Osc::Cwd(p) => self.reported_cwd = Some(p),
            }
        }
        t.parser.advance(&mut t.term, bytes);
        self.drain_events(&mut signals);
        signals
    }

    /// The next chunk of output read from the PTY, if any.
    pub fn take_output(&self) -> Option<Vec<u8>> {
        self.term()?.output.try_recv().ok()
    }

    pub fn has_output(&self) -> bool {
        self.term().is_some_and(|t| !t.output.is_empty())
    }

    /// Flush a synchronized update whose timeout expired.
    pub fn poll_sync_timeout(&mut self) -> bool {
        let Content::Term(t) = &mut self.content else { return false };
        match t.parser.sync_timeout().sync_timeout() {
            Some(deadline) if deadline <= Instant::now() => {
                t.parser.stop_sync(&mut t.term);
                true
            }
            _ => false,
        }
    }

    fn drain_events(&mut self, signals: &mut Vec<PaneSignal>) {
        let Content::Term(t) = &mut self.content else { return };
        let events: Vec<TermEvent> = t.events.borrow_mut().drain(..).collect();
        for ev in events {
            match ev {
                TermEvent::PtyWrite(s) => t.write(s.into_bytes()),
                TermEvent::Title(s) => {
                    self.title = sanitize_title(&s);
                    signals.push(PaneSignal::TitleChanged);
                }
                TermEvent::ResetTitle => {
                    self.title.clear();
                    signals.push(PaneSignal::TitleChanged);
                }
                TermEvent::Bell => {
                    self.bell = true;
                    signals.push(PaneSignal::Bell);
                }
                TermEvent::ClipboardStore(_, text) => signals.push(PaneSignal::Clipboard(text)),
                TermEvent::ColorRequest(index, format) => {
                    let rgb = t.term.colors()[index].unwrap_or_else(|| default_color(index));
                    t.write(format(rgb).into_bytes());
                }
                TermEvent::TextAreaSizeRequest(format) => {
                    let size = WindowSize { num_lines: self.rows, num_cols: self.cols, cell_width: 8, cell_height: 16 };
                    t.write(format(size).into_bytes());
                }
                _ => {}
            }
        }
    }

    pub fn write_input(&mut self, bytes: Vec<u8>) {
        if let Content::Term(t) = &mut self.content {
            t.term.scroll_display(Scroll::Bottom);
            t.write(bytes);
        }
    }

    pub fn input_modes(&self) -> PaneInputModes {
        let Some(t) = self.term() else { return PaneInputModes::default() };
        let m = *t.term.mode();
        PaneInputModes {
            app_cursor: m.contains(TermMode::APP_CURSOR),
            app_keypad: m.contains(TermMode::APP_KEYPAD),
            bracketed_paste: m.contains(TermMode::BRACKETED_PASTE),
            mouse_click: m.contains(TermMode::MOUSE_REPORT_CLICK),
            mouse_drag: m.contains(TermMode::MOUSE_DRAG),
            mouse_motion: m.contains(TermMode::MOUSE_MOTION),
            sgr_mouse: m.contains(TermMode::SGR_MOUSE),
            utf8_mouse: m.contains(TermMode::UTF8_MOUSE),
            focus_events: m.contains(TermMode::FOCUS_IN_OUT),
            alternate_scroll: m.contains(TermMode::ALTERNATE_SCROLL),
            alt_screen: m.contains(TermMode::ALT_SCREEN),
            kitty_flags: [
                (TermMode::DISAMBIGUATE_ESC_CODES, 1),
                (TermMode::REPORT_EVENT_TYPES, 2),
                (TermMode::REPORT_ALTERNATE_KEYS, 4),
                (TermMode::REPORT_ALL_KEYS_AS_ESC, 8),
                (TermMode::REPORT_ASSOCIATED_TEXT, 16),
            ]
            .iter()
            .filter(|(f, _)| m.contains(*f))
            .map(|(_, b)| b)
            .sum(),
        }
    }

    pub fn wants_mouse(&self) -> bool {
        self.term().is_some_and(|t| t.term.mode().intersects(TermMode::MOUSE_MODE))
    }

    pub fn kill(&mut self) {
        if let Content::Term(t) = &mut self.content {
            let _ = t.killer.kill();
        }
    }

    pub fn pid(&self) -> Option<u32> {
        self.term().and_then(|t| t.pid)
    }

    /// Command line of the foreground process group leader.
    pub fn foreground_argv(&self) -> Option<Vec<String>> {
        let t = self.term()?;
        let pgid = t.master.process_group_leader()?;
        read_cmdline(pgid as u32)
    }

    /// Whether something other than the pane's shell is in the foreground.
    pub fn has_foreground_job(&self) -> bool {
        let Some(t) = self.term() else { return false };
        match (t.master.process_group_leader(), t.pid) {
            (Some(pg), Some(pid)) => pg as u32 != pid,
            _ => false,
        }
    }

    /// Best-known working directory: OSC 7, else the shell's cwd from /proc.
    pub fn current_cwd(&self) -> Option<PathBuf> {
        if let Some(p) = &self.reported_cwd {
            return Some(p.clone());
        }
        let pid = self.pid()?;
        std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
    }

    pub fn scroll(&mut self, lines: i32) {
        match &mut self.content {
            Content::Term(t) => t.term.scroll_display(Scroll::Delta(lines)),
            Content::Preview(p) => p.scroll(-lines),
            Content::Files(f) => f.scroll(0, -lines),
        }
    }

    pub fn scroll_page(&mut self, up: bool) {
        match &mut self.content {
            Content::Term(t) => t.term.scroll_display(if up { Scroll::PageUp } else { Scroll::PageDown }),
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
            Content::Term(t) => t.term.scroll_display(Scroll::Bottom),
            Content::Preview(p) => p.bottom(),
            Content::Files(f) => {
                f.key(crate::files::FileKey::End);
            }
        }
    }

    pub fn scroll_to_top(&mut self) {
        match &mut self.content {
            Content::Term(t) => t.term.scroll_display(Scroll::Top),
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

    /// The link under a cell: an OSC 8 hyperlink, or a URL written in the text.
    pub fn url_at(&self, col: u16, row: u16) -> Option<String> {
        let t = self.term()?;
        let p = t.viewport_point(col, row);
        let grid = t.term.grid();
        if let Some(h) = grid[p.line][p.column].hyperlink() {
            return Some(h.uri().to_string());
        }
        let line = row_text(&t.term, p.line);
        url_in_text(&line, col as usize)
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
            }
        }
    }

    pub fn in_copy_mode(&self) -> bool {
        self.term().is_some_and(|t| t.term.mode().contains(TermMode::VI))
    }

    /// Move the copy-mode cursor, keeping it in view and extending the selection.
    pub fn vi_motion(&mut self, motion: ViMotion) {
        if let Some(t) = self.term_mut() {
            t.term.vi_motion(motion);
            let point = t.term.vi_mode_cursor.point;
            t.term.scroll_to_point(point);
        }
    }

    /// Move the copy-mode cursor by whole screens or half screens (negative = up).
    pub fn vi_scroll(&mut self, lines: i32) {
        if let Some(t) = self.term_mut() {
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
        let Ok(regex) = RegexSearch::new(&regex_escape(query)) else { return false };
        self.search = Some(SearchState { regex, current: None });
        self.search_next(true)
    }

    /// Next match: `older` goes up the scrollback, otherwise down. Wraps around.
    pub fn search_next(&mut self, older: bool) -> bool {
        let Content::Term(t) = &mut self.content else { return false };
        let Some(state) = &mut self.search else { return false };
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
        let origin = match (&state.current, older) {
            (Some(_), true) => origin.sub(term, alacritty_terminal::index::Boundary::None, 1),
            (Some(_), false) => origin.add(term, alacritty_terminal::index::Boundary::None, 1),
            (None, _) => origin,
        };
        let Some(m) = term.search_next(&mut state.regex, origin, dir, side, None) else {
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
                let (start, end) = match lines {
                    None => (0, screen),
                    Some(n) => {
                        let bottom = last_nonblank_line(&t.term).unwrap_or(0) + 1;
                        let top = (bottom - n as i32).max(-(grid.history_size() as i32));
                        (top, bottom)
                    }
                };
                let mut out = String::new();
                for l in start..end {
                    out.push_str(&row_text(&t.term, Line(l)));
                    out.push('\n');
                }
                let trimmed = out.trim_end_matches('\n').len();
                out.truncate(trimmed);
                out.push('\n');
                out
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

impl TermPane {
    fn spawn(id: PaneId, spec: &PaneSpec, cols: u16, rows: u16, env: &PaneEnv, tx: &Sender<Event>) -> Result<TermPane> {
        let pty = portable_pty::native_pty_system();
        let pair = pty.openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }).context("opening a pty")?;
        let mut cmd = if spec.command.is_empty() {
            CommandBuilder::new(&env.shell)
        } else {
            let mut c = CommandBuilder::new(&spec.command[0]);
            c.args(&spec.command[1..]);
            c
        };
        let cwd = spec.cwd.clone().filter(|p| p.is_dir()).or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from("/"));
        cmd.cwd(&cwd);
        for (k, _) in std::env::vars_os() {
            let k = k.to_string_lossy();
            if k.starts_with("ZELLIJ") || k == "TMUX" || k == "TMUX_PANE" || k == "TERM_PROGRAM_VERSION" {
                cmd.env_remove(k.as_ref());
            }
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "kraal");
        cmd.env("KRAAL", "1");
        cmd.env("KRAAL_SOCKET", &env.socket);
        cmd.env("KRAAL_SESSION", &env.session);
        cmd.env("KRAAL_PANE", id.to_string());
        for k in &spec.env_remove {
            cmd.env_remove(k);
        }
        for (k, v) in &spec.env {
            cmd.env(k, v);
        }
        let child: Box<dyn Child + Send + Sync> = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("starting {:?}", spec.command.first().unwrap_or(&env.shell)))?;
        drop(pair.slave);
        let pid = child.process_id();
        let killer = child.clone_killer();
        let mut reader = pair.master.try_clone_reader()?;
        let mut writer = pair.master.take_writer()?;

        let ready_tx = tx.clone();
        let (out_tx, out_rx) = crossbeam_channel::bounded::<Vec<u8>>(4);
        std::thread::Builder::new().name(format!("pane-{id}-read")).spawn(move || {
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if out_tx.send(buf[..n].to_vec()).is_err() {
                            return;
                        }
                        // A lost wake-up is harmless: the server also polls panes with output.
                        let _ = ready_tx.try_send(Event::PaneReady(id));
                    }
                }
            }
        })?;
        let (in_tx, in_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        std::thread::Builder::new().name(format!("pane-{id}-write")).spawn(move || {
            for data in in_rx {
                if writer.write_all(&data).and_then(|_| writer.flush()).is_err() {
                    break;
                }
            }
        })?;
        let wait_tx = tx.clone();
        let pid_for_wait = pid;
        let mut child = child;
        std::thread::Builder::new().name(format!("pane-{id}-wait")).spawn(move || {
            let code = child.wait().ok().map(|s| s.exit_code() as i32);
            let _ = wait_tx.send(Event::PaneExited(id, pid_for_wait, code));
        })?;

        let events = Rc::new(RefCell::new(Vec::new()));
        // Programs may enable the kitty keyboard protocol (Claude Code does, for Shift+Enter).
        let config = TermConfig { scrolling_history: env.scrollback, kitty_keyboard: true, ..TermConfig::default() };
        let term = Term::new(config, &Size { cols, rows }, Listener(events.clone()));
        let pane = TermPane {
            term,
            events,
            parser: Processor::new(),
            master: pair.master,
            input: in_tx,
            output: out_rx,
            killer,
            pid,
            osc: OscScanner::default(),
        };
        if let Some(text) = &spec.startup_input {
            pane.write(text.clone().into_bytes());
        }
        Ok(pane)
    }

    fn write(&self, bytes: Vec<u8>) {
        let _ = self.input.send(bytes);
    }

    fn viewport_point(&self, col: u16, row: u16) -> Point {
        let offset = self.term.grid().display_offset() as i32;
        let cols = self.term.columns();
        Point::new(Line(row as i32 - offset), Column((col as usize).min(cols.saturating_sub(1))))
    }
}

fn read_cmdline(pid: u32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let argv: Vec<String> =
        raw.split(|b| *b == 0).filter(|s| !s.is_empty()).map(|s| String::from_utf8_lossy(s).into_owned()).collect();
    (!argv.is_empty()).then_some(argv)
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

/// Open a URL with the desktop's default application.
pub fn open_url(url: &str) -> std::io::Result<()> {
    let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    std::process::Command::new(opener)
        .arg(url)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
}

fn sanitize_title(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(120).collect()
}

fn row_text<T>(term: &Term<T>, line: Line) -> String {
    let grid = term.grid();
    let row = &grid[line];
    let mut s = String::new();
    for col in 0..grid.columns() {
        let cell = &row[Column(col)];
        if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
            continue;
        }
        s.push(cell.c);
        if let Some(zw) = cell.zerowidth() {
            s.extend(zw);
        }
    }
    s.trim_end().to_string()
}

fn last_nonblank_line<T>(term: &Term<T>) -> Option<i32> {
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

fn convert_color(c: AColor, colors: &alacritty_terminal::term::color::Colors) -> Color {
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

static OUTER_COLORS: std::sync::Mutex<Option<(Rgb, Rgb)>> = std::sync::Mutex::new(None);

/// Record the outer terminal's foreground/background (`rgb:rrrr/gggg/bbbb` replies to OSC 10/11),
/// so programs querying a pane's colours get the real ones.
pub fn set_outer_colors(fg: &str, bg: &str) {
    let (Some(fg), Some(bg)) = (parse_xcolor(fg), parse_xcolor(bg)) else { return };
    if let Ok(mut c) = OUTER_COLORS.lock() {
        *c = Some((fg, bg));
    }
}

fn parse_xcolor(s: &str) -> Option<Rgb> {
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
fn default_color(index: usize) -> Rgb {
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

/// OSC sequences kraal itself cares about, picked out of the output stream.
#[derive(Debug, Clone, PartialEq)]
enum Osc {
    Notify { title: String, body: String },
    Cwd(PathBuf),
}

#[derive(Default)]
struct OscScanner {
    state: ScanState,
    buf: Vec<u8>,
}

#[derive(Default, Clone, Copy, PartialEq)]
enum ScanState {
    #[default]
    Ground,
    Esc,
    Osc,
    OscEsc,
}

impl OscScanner {
    fn scan(&mut self, bytes: &[u8]) -> Vec<Osc> {
        let mut out = Vec::new();
        for &b in bytes {
            self.state = match (self.state, b) {
                (ScanState::Ground, 0x1b) => ScanState::Esc,
                (ScanState::Ground, _) => ScanState::Ground,
                (ScanState::Esc, b']') => {
                    self.buf.clear();
                    ScanState::Osc
                }
                (ScanState::Esc, 0x1b) => ScanState::Esc,
                (ScanState::Esc, _) => ScanState::Ground,
                (ScanState::Osc, 0x07) => {
                    out.extend(parse_osc(&self.buf));
                    ScanState::Ground
                }
                (ScanState::Osc, 0x1b) => ScanState::OscEsc,
                (ScanState::Osc, b) => {
                    if self.buf.len() < 8192 {
                        self.buf.push(b);
                    }
                    ScanState::Osc
                }
                (ScanState::OscEsc, b'\\') => {
                    out.extend(parse_osc(&self.buf));
                    ScanState::Ground
                }
                (ScanState::OscEsc, b']') => {
                    self.buf.clear();
                    ScanState::Osc
                }
                (ScanState::OscEsc, _) => ScanState::Ground,
            };
        }
        out
    }
}

fn parse_osc(data: &[u8]) -> Option<Osc> {
    let s = String::from_utf8_lossy(data);
    let (code, rest) = s.split_once(';')?;
    match code {
        // iTerm2 style: OSC 9 ; message. `9;4;…` is a progress report, not a notification.
        "9" if !rest.starts_with("4;") => Some(Osc::Notify { title: String::new(), body: clean(rest) }),
        // rxvt/Ghostty style: OSC 777 ; notify ; title ; body
        "777" => {
            let rest = rest.strip_prefix("notify;")?;
            let (title, body) = rest.split_once(';').unwrap_or((rest, ""));
            Some(Osc::Notify { title: clean(title), body: clean(body) })
        }
        "7" => {
            let url = rest.strip_prefix("file://")?;
            let path = &url[url.find('/')?..];
            Some(Osc::Cwd(PathBuf::from(percent_decode(path))))
        }
        _ => None,
    }
}

fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(300).collect()
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(v) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz"), 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Escape sequence asking the outer terminal to copy `text` (OSC 52).
pub fn osc52(text: &str) -> Vec<u8> {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    format!("\x1b]52;c;{b64}\x07").into_bytes()
}

/// Escape sequence for a desktop notification on the outer terminal.
pub fn osc_notify(title: &str, body: &str) -> Vec<u8> {
    let t = clean(title).replace(';', ",");
    let b = clean(body).replace(';', ",");
    format!("\x1b]777;notify;{t};{b}\x07\x1b]9;{t}: {b}\x07").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc_scanner_finds_notifications_across_chunks() {
        let mut s = OscScanner::default();
        let data = b"hello\x1b]777;notify;Claude;needs you\x07 and \x1b]9;done!\x1b\\ \x1b]9;4;1;50\x07";
        let mut found = Vec::new();
        for chunk in data.chunks(3) {
            found.extend(s.scan(chunk));
        }
        assert_eq!(
            found,
            vec![
                Osc::Notify { title: "Claude".into(), body: "needs you".into() },
                Osc::Notify { title: String::new(), body: "done!".into() },
            ]
        );
    }

    #[test]
    fn osc7_cwd() {
        let mut s = OscScanner::default();
        let found = s.scan(b"\x1b]7;file://laptop/home/me/My%20Dir\x07");
        assert_eq!(found, vec![Osc::Cwd(PathBuf::from("/home/me/My Dir"))]);
    }

    #[test]
    fn render_copies_cells_and_styles() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut term = Term::new(TermConfig::default(), &Size { cols: 10, rows: 3 }, Listener(events));
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, b"\x1b[1;31mhi\x1b[0m \xe6\x97\xa5x");
        let area = Rect::new(2, 1, 10, 3);
        let mut buf = Buffer::empty(Rect::new(0, 0, 14, 5));
        render_term(&term, area, &mut buf, None);
        assert_eq!(buf[(2, 1)].symbol(), "h");
        assert_eq!(buf[(2, 1)].fg, Color::Red);
        assert!(buf[(2, 1)].modifier.contains(Modifier::BOLD));
        assert_eq!(buf[(5, 1)].symbol(), "日");
        assert_eq!(buf[(7, 1)].symbol(), "x");
    }

    #[test]
    fn osc8_links_are_collected() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut term = Term::new(TermConfig::default(), &Size { cols: 20, rows: 2 }, Listener(events));
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, b"see \x1b]8;;https://x.org\x1b\\link\x1b]8;;\x1b\\ ok");
        let mut buf = Buffer::empty(Rect::new(0, 0, 20, 2));
        let mut links = crate::ansi::Links::new();
        render_term(&term, Rect::new(0, 0, 20, 2), &mut buf, Some(&mut links));
        let mut cols: Vec<u16> = links.keys().map(|(x, _)| *x).collect();
        cols.sort();
        assert_eq!(cols, vec![4, 5, 6, 7]);
        assert_eq!(&*links[&(4, 0)], "https://x.org");
    }

    #[test]
    fn urls_in_text() {
        let line = "see https://example.org/a?b=1). and more";
        assert_eq!(url_in_text(line, 10).as_deref(), Some("https://example.org/a?b=1"));
        assert_eq!(url_in_text(line, 1), None);
        assert_eq!(url_in_text("(http://x.y/z)", 3).as_deref(), Some("http://x.y/z"));
        assert_eq!(url_in_text("nothing here", 3), None);
    }

    #[test]
    fn xcolor_parsing() {
        assert_eq!(parse_xcolor("rgb:ffff/8080/0000"), Some(Rgb { r: 255, g: 128, b: 0 }));
        assert_eq!(parse_xcolor("rgb:f/8/0"), Some(Rgb { r: 255, g: 136, b: 0 }));
        assert_eq!(parse_xcolor("#fff"), None);
    }

    #[test]
    fn default_palette_is_xterm_like() {
        assert_eq!(default_color(16), Rgb { r: 0, g: 0, b: 0 });
        assert_eq!(default_color(231), Rgb { r: 255, g: 255, b: 255 });
        assert_eq!(default_color(232), Rgb { r: 8, g: 8, b: 8 });
    }
}
