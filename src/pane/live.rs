//! A terminal pane carried over a live restart (see `crate::server::upgrade`): its PTY and
//! program stay as they are, and what its emulator shows is written down as text with its
//! attributes (the scrollback's own format), the cursors and the modes the program set, then
//! shown again by the emulator of the kraal that takes over.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::TermMode;
use alacritty_terminal::vte::ansi::{CursorShape, CursorStyle};
use serde::{Deserialize, Serialize};

use super::*;

/// A terminal pane as a live restart hands it over.
#[derive(Debug, Serialize, Deserialize)]
pub struct LivePane {
    pub id: PaneId,
    /// The PTY, kept open over the exec.
    pub fd: i32,
    /// The program, still a child of the server (exec keeps the pid), and the process group
    /// it leads.
    pub pid: u32,
    pub pgid: i32,
    pub spec: PaneSpec,
    pub cols: u16,
    pub rows: u16,
    pub exit_code: Option<Option<i32>>,
    pub suspended: bool,
    pub preparing: bool,
    pub title: String,
    pub bell: bool,
    pub reported_cwd: Option<PathBuf>,
    pub reported_host: String,
    pub ssh: Option<crate::servers::Server>,
    /// What its commands worked on, and the contexts its shell over SSH reported: whether it is
    /// production.
    pub seen: crate::context::Seen,
    pub reported: Option<(u32, crate::context::RemoteContexts)>,
    pub reported_since_command: bool,
    /// The line that marks where text restored after a cold restart ends.
    pub restored: Option<String>,
    pub agent: LiveAgent,
    pub screen: LiveScreen,
}

impl LivePane {
    /// A terminal kept open over a live restart whose hand-off could not be read: all that is
    /// known is its PTY (`fd`), its size and the program that leads its session (`pid`, 0 when
    /// none is known).
    #[cfg(target_os = "linux")]
    pub fn salvaged(id: PaneId, fd: i32, pid: u32, pgid: i32, (cols, rows): (u16, u16)) -> LivePane {
        LivePane {
            id,
            fd,
            pid,
            pgid,
            spec: PaneSpec::default(),
            cols,
            rows,
            exit_code: None,
            suspended: false,
            preparing: false,
            title: String::new(),
            bell: false,
            reported_cwd: None,
            reported_host: String::new(),
            ssh: None,
            seen: Default::default(),
            reported: None,
            reported_since_command: false,
            restored: None,
            agent: LiveAgent::default(),
            screen: LiveScreen::default(),
        }
    }
}

/// What kraal knows of the agent in a pane.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct LiveAgent {
    pub kind: Option<String>,
    pub state: crate::agent::AgentState,
    /// For how long it has been in that state.
    pub for_ms: u64,
    pub hooked: bool,
    pub session_id: Option<String>,
    pub message: Option<String>,
    /// The process that ran its last hook: its id and start time.
    pub reporter: Option<(u32, u64)>,
}

/// What an emulator shows: the primary screen with its history, the alternate screen while a
/// full-screen program uses it, and the modes.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct LiveScreen {
    pub primary: ScreenText,
    /// The kitty keyboard flags pushed on the primary and the alternate screen, oldest first:
    /// a program pops back to the flags it pushed before.
    pub keyboard_stacks: [Vec<u8>; 2],
    pub alternate: Option<ScreenText>,
    pub modes: Modes,
    /// The cursor's shape (DECSCUSR), when the program changed it.
    pub cursor_shape: Option<u8>,
}

/// One screen's text and cursor.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ScreenText {
    /// Its lines, history first, as `scrollback_ansi` writes them.
    pub text: String,
    /// The screen row of the last of those lines (below 0: a line of the history).
    pub last_row: i32,
    pub cursor_row: u16,
    pub cursor_col: u16,
    /// The attributes the program writes with next (SGR).
    pub pen: String,
}

/// The modes a program set that change what it is sent or how it shows. The kitty keyboard
/// flags are those in force on the screen in use (set without a push, they are not the last
/// pushed).
#[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
pub struct Modes {
    pub app_cursor: bool,
    pub app_keypad: bool,
    pub mouse_click: bool,
    pub mouse_drag: bool,
    pub mouse_motion: bool,
    pub sgr_mouse: bool,
    pub utf8_mouse: bool,
    pub focus_events: bool,
    pub bracketed_paste: bool,
    pub alternate_scroll: bool,
    pub cursor_hidden: bool,
    pub no_wrap: bool,
    pub insert: bool,
    pub newline: bool,
    pub keyboard: u8,
}

impl Modes {
    fn of(m: TermMode) -> Modes {
        Modes {
            app_cursor: m.contains(TermMode::APP_CURSOR),
            app_keypad: m.contains(TermMode::APP_KEYPAD),
            mouse_click: m.contains(TermMode::MOUSE_REPORT_CLICK),
            mouse_drag: m.contains(TermMode::MOUSE_DRAG),
            mouse_motion: m.contains(TermMode::MOUSE_MOTION),
            sgr_mouse: m.contains(TermMode::SGR_MOUSE),
            utf8_mouse: m.contains(TermMode::UTF8_MOUSE),
            focus_events: m.contains(TermMode::FOCUS_IN_OUT),
            bracketed_paste: m.contains(TermMode::BRACKETED_PASTE),
            alternate_scroll: m.contains(TermMode::ALTERNATE_SCROLL),
            cursor_hidden: !m.contains(TermMode::SHOW_CURSOR),
            no_wrap: !m.contains(TermMode::LINE_WRAP),
            insert: m.contains(TermMode::INSERT),
            newline: m.contains(TermMode::LINE_FEED_NEW_LINE),
            keyboard: keyboard_flags(m),
        }
    }

    /// The sequences that set them on an emulator that starts with none (the keyboard flags
    /// apart: they go with their screen).
    fn sequence(&self) -> String {
        let mut out = String::new();
        let modes = [
            (self.app_cursor, "\x1b[?1h"),
            (self.app_keypad, "\x1b="),
            // One mouse mode at a time, as the emulator keeps them.
            (self.mouse_click, "\x1b[?1000h"),
            (self.mouse_drag, "\x1b[?1002h"),
            (self.mouse_motion, "\x1b[?1003h"),
            (self.sgr_mouse, "\x1b[?1006h"),
            (self.utf8_mouse, "\x1b[?1005h"),
            (self.focus_events, "\x1b[?1004h"),
            (self.bracketed_paste, "\x1b[?2004h"),
            (!self.alternate_scroll, "\x1b[?1007l"),
            (self.cursor_hidden, "\x1b[?25l"),
            (self.no_wrap, "\x1b[?7l"),
            (self.insert, "\x1b[4h"),
            (self.newline, "\x1b[20h"),
        ];
        for (on, seq) in modes {
            if on {
                out.push_str(seq);
            }
        }
        out
    }
}

fn keyboard_flags(m: TermMode) -> u8 {
    [
        (TermMode::DISAMBIGUATE_ESC_CODES, 1),
        (TermMode::REPORT_EVENT_TYPES, 2),
        (TermMode::REPORT_ALTERNATE_KEYS, 4),
        (TermMode::REPORT_ALL_KEYS_AS_ESC, 8),
        (TermMode::REPORT_ASSOCIATED_TEXT, 16),
    ]
    .iter()
    .filter(|(f, _)| m.contains(*f))
    .map(|(_, b)| b)
    .sum()
}

/// The DECSCUSR parameter of a cursor style other than the default one.
fn cursor_param(style: CursorStyle) -> Option<u8> {
    if style == CursorStyle::default() {
        return None;
    }
    let steady = u8::from(!style.blinking);
    match style.shape {
        CursorShape::Block => Some(1 + steady),
        CursorShape::Underline => Some(3 + steady),
        CursorShape::Beam => Some(5 + steady),
        CursorShape::HollowBlock | CursorShape::Hidden => None,
    }
}

impl ScreenText {
    fn of<T>(term: &alacritty_terminal::Term<T>) -> ScreenText {
        let (text, last_row) = scrollback::encode_rows(term, MAX_SAVED_BYTES);
        let cursor = &term.grid().cursor;
        ScreenText {
            text: String::from_utf8_lossy(&text).into_owned(),
            last_row,
            cursor_row: cursor.point.line.0.max(0) as u16,
            cursor_col: cursor.point.column.0 as u16,
            pen: String::from_utf8_lossy(&scrollback::sgr_of(&cursor.template)).into_owned(),
        }
    }

    /// What draws it on a screen of `rows` rows, from its top: the text (what scrolls off the
    /// top goes to the history), then blank lines until its last line is on its row, the
    /// cursor and the attributes. Only text, attributes and links are taken from it.
    fn replay(&self, rows: u16) -> Vec<u8> {
        let mut out = b"\x1b[H".to_vec();
        if !self.text.is_empty() {
            out.extend(scrollback::clean(self.text.as_bytes(), usize::MAX));
            let below = (i32::from(rows) - 1 - self.last_row).max(0) as usize;
            out.extend("\r\n".repeat(below).as_bytes());
        }
        out.extend(format!("\x1b[{};{}H", self.cursor_row + 1, self.cursor_col + 1).as_bytes());
        out.extend(scrollback::clean(self.pen.as_bytes(), 1));
        out
    }
}

impl TermPane {
    /// Write down what the emulator shows. The emulator is left as it was, but for the
    /// alternate screen's saved cursor.
    fn capture(&mut self) -> LiveScreen {
        let modes = Modes::of(*self.term.mode());
        let cursor_shape = cursor_param(self.term.cursor_style());
        let keyboard_stacks = self.guard.keyboard_stacks().clone();
        if !self.term.mode().contains(TermMode::ALT_SCREEN) {
            let primary = ScreenText::of(&self.term);
            return LiveScreen { primary, keyboard_stacks, alternate: None, modes, cursor_shape };
        }
        let alternate = ScreenText::of(&self.term);
        // The primary screen is out of reach behind the alternate one: switch to it, then back
        // (which clears the alternate screen, and puts the flags of the top of its stack in
        // force: it is drawn again, and the flags it had set again).
        self.term.swap_alt();
        let primary = ScreenText::of(&self.term);
        self.term.swap_alt();
        let rows = self.term.grid().screen_lines() as u16;
        let mut again = alternate.replay(rows);
        again.extend(format!("\x1b[={};1u", modes.keyboard).as_bytes());
        self.advance(&again);
        self.events.borrow_mut().clear();
        LiveScreen { primary, keyboard_stacks, alternate: Some(alternate), modes, cursor_shape }
    }

    /// Show `screen` on this emulator, fresh and of the same size as the one it was taken from.
    fn show(&mut self, screen: &LiveScreen) {
        let rows = self.term.grid().screen_lines() as u16;
        let push = |out: &mut Vec<u8>, stack: &[u8]| {
            for flags in stack {
                out.extend(format!("\x1b[>{flags}u").as_bytes());
            }
        };
        let [primary_stack, alternate_stack] = &screen.keyboard_stacks;
        let mut out = Vec::new();
        // The alternate screen keeps its stack while the primary one shows.
        if screen.alternate.is_none() && !alternate_stack.is_empty() {
            out.extend_from_slice(b"\x1b[?1049h");
            push(&mut out, alternate_stack);
            out.extend_from_slice(b"\x1b[?1049l");
        }
        out.extend(screen.primary.replay(rows));
        push(&mut out, primary_stack);
        if let Some(alt) = &screen.alternate {
            out.extend_from_slice(b"\x1b[?1049h");
            out.extend(alt.replay(rows));
            push(&mut out, alternate_stack);
        }
        out.extend(format!("\x1b[={};1u", screen.modes.keyboard).as_bytes());
        out.extend(screen.modes.sequence().as_bytes());
        if let Some(shape) = screen.cursor_shape {
            out.extend(format!("\x1b[{shape} q").as_bytes());
        }
        self.advance(&out);
        // Nothing in it asks for an answer.
        self.events.borrow_mut().clear();
    }
}

impl Pane {
    /// What a live restart hands over of this pane: None for a pane that is not a terminal,
    /// or whose PTY or program is unknown.
    pub fn live(&mut self) -> Option<LivePane> {
        let Content::Term(t) = &mut self.content else { return None };
        let (fd, pid) = (t.master_fd()?, t.pid?);
        // SAFETY: plain syscall; the pid is ours (unreaped) while the pane holds its process.
        let pgid = unsafe { libc::getpgid(pid as libc::pid_t) };
        let screen = t.capture();
        Some(LivePane {
            id: self.id,
            fd,
            pid,
            pgid,
            spec: self.spec.clone(),
            cols: self.cols,
            rows: self.rows,
            exit_code: self.exit_code,
            suspended: self.suspended,
            preparing: self.preparing,
            title: self.title.clone(),
            bell: self.bell,
            reported_cwd: self.reported_cwd.clone(),
            reported_host: self.reported_host.clone(),
            ssh: self.ssh.clone(),
            seen: self.seen.clone(),
            reported: self.reported.clone(),
            reported_since_command: self.reported_since_command,
            restored: self.restored.clone(),
            agent: LiveAgent {
                kind: self.agent.kind.clone(),
                state: self.agent.state,
                for_ms: self.agent.since.elapsed().as_millis() as u64,
                hooked: self.agent.hooked,
                session_id: self.agent.session_id.clone(),
                message: self.agent.message.clone(),
                reporter: self.agent.reporter,
            },
            screen,
        })
    }

    /// The pane `live` describes, on its PTY (`fd`, owned from now on) and its program (unless
    /// it is not this process's: `process`), showing what it showed. `name` and the place in
    /// the layout come with the session's state.
    pub fn adopt(
        live: &LivePane,
        fd: std::os::fd::OwnedFd,
        process: bool,
        name: Option<String>,
        env: &PaneEnv,
        tx: &Sender<Event>,
    ) -> Result<Pane> {
        let (cols, rows) = (live.cols.max(2), live.rows.max(1));
        let watch = live.exit_code.is_none();
        let pid = process.then_some(live.pid);
        let mut term = TermPane::adopt(live.id, fd, pid, watch, (cols, rows), env, tx)?;
        term.show(&live.screen);
        let since = Instant::now().checked_sub(Duration::from_millis(live.agent.for_ms)).unwrap_or_else(Instant::now);
        // A finished transfer's window closes by itself, as it was going to.
        let closes = live.spec.close_on_success && live.exit_code == Some(Some(0));
        Ok(Pane {
            id: live.id,
            name,
            title: live.title.clone(),
            spec: live.spec.clone(),
            content: Content::Term(Box::new(term)),
            cols,
            rows,
            agent: AgentTracker {
                kind: live.agent.kind.clone(),
                state: live.agent.state,
                since,
                hooked: live.agent.hooked,
                session_id: live.agent.session_id.clone(),
                message: live.agent.message.clone(),
                reporter: live.agent.reporter,
            },
            exit_code: live.exit_code,
            last_output: Instant::now(),
            bell: live.bell,
            reported_cwd: live.reported_cwd.clone(),
            reported_host: live.reported_host.clone(),
            ssh: live.ssh.clone(),
            seen: live.seen.clone(),
            reported: live.reported.clone(),
            reported_since_command: live.reported_since_command,
            close_at: closes.then(|| Instant::now() + Duration::from_secs(3)),
            restart_with: None,
            suspended: live.suspended,
            preparing: live.preparing,
            search: None,
            restored: live.restored.clone(),
            recovered: None,
        })
    }

    /// Whether a full-screen program shows (the alternate screen).
    pub fn full_screen(&self) -> bool {
        self.term().is_some_and(|t| t.term.mode().contains(TermMode::ALT_SCREEN))
    }

    /// Tell the program its terminal is a row smaller, so that it draws itself again once
    /// [`Pane::refresh_pixel_size`] gives the real size back: one size change could be one the
    /// program skips, as the size it reads is the one it had.
    pub fn nudge(&mut self) {
        let Content::Term(t) = &mut self.content else { return };
        let (cols, rows) = if self.rows > 1 { (self.cols, self.rows - 1) } else { (self.cols - 1, self.rows) };
        let _ = t.master.resize(pty::pty_size(cols, rows, t.graphics.shared()));
    }

    /// Stop reading the PTY (see [`Pane::drain`]).
    pub fn stop_reading(&self) {
        if let Some(t) = self.term() {
            t.stop_reading();
        }
    }

    /// Output read before [`Pane::stop_reading`] took effect, a chunk at a time.
    pub fn drain(&self) -> Drained {
        let Some(t) = self.term() else { return Drained::Stopped };
        match t.output.try_recv() {
            Ok(data) => Drained::Output(data),
            Err(crossbeam_channel::TryRecvError::Empty) => Drained::Reading,
            Err(crossbeam_channel::TryRecvError::Disconnected) => Drained::Stopped,
        }
    }

    /// Read the PTY again after [`Pane::stop_reading`].
    pub fn resume_reading(&mut self, tx: &Sender<Event>) -> Result<()> {
        let id = self.id;
        match &mut self.content {
            Content::Term(t) => t.resume_reading(id, tx),
            _ => Ok(()),
        }
    }

    /// Draw what a synchronized update holds back, now: the signals of what it held.
    pub fn end_sync(&mut self) -> Vec<PaneSignal> {
        let Content::Term(t) = &mut self.content else { return Vec::new() };
        if t.parser.sync_timeout().sync_timeout().is_none() {
            return Vec::new();
        }
        t.marked(|t| t.parser.stop_sync(&mut t.term));
        let mut signals = Vec::new();
        self.drain_events(&mut signals);
        signals
    }

    /// Whether nothing uses the pane's terminal any more (see `TermPane::output_ended`); a
    /// pane that is not a terminal has none.
    pub fn terminal_ended(&self) -> bool {
        self.term().is_none_or(|t| t.output_ended())
    }

    /// Bytes typed or pasted that the PTY has not taken yet.
    pub fn input_pending(&self) -> usize {
        self.term().map_or(0, |t| t.input_pending())
    }

    /// Whether the program's output stops in the middle of a sequence (see [`Sequence`]).
    pub fn mid_sequence(&self) -> bool {
        self.term().is_some_and(|t| t.sequence != Sequence::Ground)
    }
}

/// Where a program's output stands, as the emulator's parser reads it: between sequences, or
/// in the middle of one (an escape sequence, a string, a character of several bytes). A live
/// restart hands a pane over only between sequences: the next emulator starts there, and the
/// end of a sequence cut in two would reach it as text (a query left unanswered).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) enum Sequence {
    #[default]
    Ground,
    /// The rest of a character of several bytes (UTF-8): how many bytes.
    Utf8(u8),
    Escape,
    EscapeIntermediate,
    Csi,
    Osc,
    /// DCS, and SOS, PM and APC (images): strings that end with ST.
    Str,
}

impl Sequence {
    /// Read `bytes`, as vte reads them.
    pub(super) fn feed(&mut self, bytes: &[u8]) {
        for &b in bytes {
            *self = self.next(b);
        }
    }

    fn next(self, b: u8) -> Sequence {
        use Sequence::*;
        match b {
            // Cancel and substitute end any sequence; escape starts one, and ends a string.
            0x18 | 0x1a => return Ground,
            0x1b => return Escape,
            _ => {}
        }
        match self {
            Ground => match b {
                0xc2..=0xdf => Utf8(1),
                0xe0..=0xef => Utf8(2),
                0xf0..=0xf4 => Utf8(3),
                _ => Ground,
            },
            Utf8(n) => match b {
                0x80..=0xbf if n > 1 => Utf8(n - 1),
                0x80..=0xbf => Ground,
                // Not a continuation: the character is dropped, and this byte read afresh.
                _ => Ground.next(b),
            },
            Escape => match b {
                0x20..=0x2f => EscapeIntermediate,
                b'[' => Csi,
                b']' => Osc,
                b'P' | b'X' | b'^' | b'_' => Str,
                0x30..=0x7e => Ground,
                _ => Escape,
            },
            EscapeIntermediate => match b {
                0x30..=0x7e => Ground,
                _ => EscapeIntermediate,
            },
            Csi => match b {
                0x40..=0x7e => Ground,
                _ => Csi,
            },
            Osc => match b {
                0x07 => Ground,
                _ => Osc,
            },
            Str => Str,
        }
    }
}

/// What [`Pane::drain`] found.
pub enum Drained {
    Output(Vec<u8>),
    /// The reader has not stopped yet.
    Reading,
    /// Everything read was taken.
    Stopped,
}

#[cfg(test)]
mod tests {
    use alacritty_terminal::index::{Column, Line};

    use super::*;

    fn env() -> PaneEnv {
        PaneEnv {
            socket: "/nonexistent".into(),
            session: "t".into(),
            shell: "sh".into(),
            scrollback: 100,
            graphics: Default::default(),
        }
    }

    fn screen_of(t: &TermPane) -> Vec<String> {
        let top = -(t.term.grid().history_size() as i32);
        (top..t.term.screen_lines() as i32).map(|l| screen::row_text(&t.term, Line(l))).collect()
    }

    #[test]
    fn a_screen_comes_back_as_it_was_with_its_modes() {
        let (tx, _rx) = crossbeam_channel::bounded(64);
        let spec = PaneSpec { command: vec!["sleep".into(), "5".into()], ..Default::default() };
        let mut before = Pane::spawn(1, None, spec.clone(), 20, 5, &env(), &tx).unwrap();
        let lines: String = (1..=12).map(|i| format!("\x1b[3{}mline {i}\x1b[0m\r\n", i % 8)).collect();
        before.feed(lines.as_bytes());
        // A prompt, the cursor after it, then a full-screen program with its own modes.
        before.feed(b"$ vi\x1b[?2004h\x1b[>1u\x1b[?1049h\x1b[?1h\x1b[?1002h\x1b[?1006h\x1b[>5u\x1b[6 q");
        before.feed(b"\x1b[2;3H\x1b[1meditor\x1b[3;1Hrow three\x1b[1;4H\x1b[7m");
        let t = before.term().unwrap();
        let alt_before = screen_of(t);
        let alt_cursor = t.term.grid().cursor.point;
        let modes_before = *t.term.mode();
        let Content::Term(t) = &mut before.content else { panic!() };
        let saved = t.capture();
        // Taking it leaves the pane as it was.
        assert_eq!(screen_of(t), alt_before);
        assert_eq!(t.term.grid().cursor.point, alt_cursor);
        assert_eq!(*t.term.mode(), modes_before);
        let json = serde_json::to_string(&saved).unwrap();
        let saved: LiveScreen = serde_json::from_str(&json).unwrap();

        let mut after = Pane::spawn(2, None, spec, 20, 5, &env(), &tx).unwrap();
        let Content::Term(fresh) = &mut after.content else { panic!() };
        fresh.show(&saved);
        assert_eq!(screen_of(fresh), alt_before);
        assert_eq!(fresh.term.grid().cursor.point, alt_cursor);
        assert_eq!(*fresh.term.mode(), modes_before);
        assert_eq!(fresh.term.cursor_style().shape, CursorShape::Beam);
        assert!(fresh.term.grid().cursor.template.flags.contains(alacritty_terminal::term::cell::Flags::INVERSE));
        assert!(fresh.events.borrow().is_empty());
        // Leaving the full-screen program shows the primary screen and its history, colours
        // included, with the cursor after the prompt and the primary screen's keyboard flags.
        before.feed(b"\x1b[?1049l");
        after.feed(b"\x1b[?1049l");
        let (b, a) = (before.term().unwrap(), after.term().unwrap());
        assert_eq!(screen_of(a), screen_of(b));
        assert_eq!(a.term.grid().cursor.point, b.term.grid().cursor.point);
        assert_eq!(*a.term.mode(), *b.term.mode());
        assert_eq!(keyboard_flags(*a.term.mode()), 1);
        let top = -(a.term.grid().history_size() as i32);
        let red = (top..5).find(|l| screen::row_text(&a.term, Line(*l)) == "line 9").unwrap();
        assert_eq!(
            a.term.grid()[Line(red)][Column(0)].fg,
            alacritty_terminal::vte::ansi::Color::Named(alacritty_terminal::vte::ansi::NamedColor::Red)
        );
        before.kill();
        after.kill();
    }

    /// A program pops back to the keyboard flags it pushed before, on either screen, and keeps
    /// the flags it set without pushing.
    #[test]
    fn keyboard_stacks_come_back_whole() {
        let (tx, _rx) = crossbeam_channel::bounded(64);
        let spec = PaneSpec { command: vec!["sleep".into(), "5".into()], ..Default::default() };
        let flags = |p: &Pane| keyboard_flags(*p.term().unwrap().term.mode());
        for (setup, alternate) in [
            // A full-screen program over a prompt with its own flags.
            (&b"\x1b[>8u\x1b[>1u\x1b[?1049h\x1b[>4u\x1b[>2u\x1b[=3;2u"[..], true),
            // Back on the primary screen: the alternate one keeps its stack.
            (b"\x1b[>8u\x1b[>1u\x1b[?1049h\x1b[>4u\x1b[>2u\x1b[?1049l\x1b[=5;1u", false),
        ] {
            let mut before = Pane::spawn(1, None, spec.clone(), 20, 5, &env(), &tx).unwrap();
            before.feed(setup);
            let modes = *before.term().unwrap().term.mode();
            let Content::Term(t) = &mut before.content else { panic!() };
            let saved = t.capture();
            assert_eq!(*t.term.mode(), modes, "taking it leaves the pane as it was");
            assert_eq!(saved.alternate.is_some(), alternate);
            let saved: LiveScreen = serde_json::from_str(&serde_json::to_string(&saved).unwrap()).unwrap();
            let mut after = Pane::spawn(2, None, spec.clone(), 20, 5, &env(), &tx).unwrap();
            let Content::Term(fresh) = &mut after.content else { panic!() };
            fresh.show(&saved);
            assert_eq!(flags(&after), flags(&before), "{setup:?}");
            for step in [&b"\x1b[<u"[..], b"\x1b[<u", b"\x1b[?1049h", b"\x1b[<u", b"\x1b[?1049l", b"\x1b[<u"] {
                before.feed(step);
                after.feed(step);
                assert_eq!(flags(&after), flags(&before), "{setup:?} then {step:?}");
            }
            before.kill();
            after.kill();
        }
    }

    /// A terminal kept from a hand-off that could not be read is never started again (nothing
    /// says what ran in it), and takes no input when its pane is unknown.
    #[test]
    fn a_kept_terminal_is_never_started_again() {
        let (tx, _rx) = crossbeam_channel::bounded(64);
        let spec = PaneSpec { command: vec!["sleep".into(), "5".into()], ..Default::default() };
        let mut pane = Pane::spawn(1, None, spec, 20, 5, &env(), &tx).unwrap();
        pane.recovered = Some(Recovered::Known);
        assert!(pane.takes_typing());
        assert!(pane.respawn(&env(), &tx).is_err());
        pane.recovered = Some(Recovered::Unknown);
        assert!(!pane.takes_typing());
        assert!(pane.respawn(&env(), &tx).is_err());
        pane.kill();
    }

    #[test]
    fn sequences_cut_in_two_are_seen() {
        let after = |bytes: &[u8]| {
            let mut s = Sequence::default();
            s.feed(bytes);
            s
        };
        for done in [
            &b"plain text\r\n"[..],
            b"\x1b[6n",
            b"\x1b[38;2;1;2;3m",
            b"\x1b]0;title\x07",
            b"\x1b]8;;x\x1b\\",
            b"\x1b(B",
            b"\x1b7",
            b"\x1bP$qm\x1b\\",
            b"\x1b_Ga=q;AAAA\x1b\\",
            "日本".as_bytes(),
            b"\x1b[1\x18",
            b"\x1b[\x1b[0m",
        ] {
            assert_eq!(after(done), Sequence::Ground, "{done:?}");
        }
        for cut in [
            &b"\x1b"[..],
            b"\x1b[",
            b"\x1b[6",
            b"\x1b[?10",
            b"\x1b]0;tit",
            b"\x1b]8;;x\x1b",
            b"\x1b(",
            b"\x1bP$q",
            b"\x1b_Ga=q;AA",
            &"日".as_bytes()[..2],
            b"a\xe2\x94",
        ] {
            assert_ne!(after(cut), Sequence::Ground, "{cut:?}");
        }
        // The rest, read later, ends it.
        let mut s = after(b"\x1b[");
        s.feed(b"6n");
        assert_eq!(s, Sequence::Ground);
    }

    #[test]
    fn a_cleared_screen_keeps_its_history_above_it() {
        let (tx, _rx) = crossbeam_channel::bounded(64);
        let spec = PaneSpec { command: vec!["sleep".into(), "5".into()], ..Default::default() };
        let mut before = Pane::spawn(1, None, spec.clone(), 20, 4, &env(), &tx).unwrap();
        before.feed(b"a\r\nb\r\nc\r\nd\r\ne\r\nf\x1b[2J\x1b[2;1Hprompt");
        let Content::Term(t) = &mut before.content else { panic!() };
        let saved = t.capture();
        let mut after = Pane::spawn(2, None, spec, 20, 4, &env(), &tx).unwrap();
        let Content::Term(fresh) = &mut after.content else { panic!() };
        fresh.show(&saved);
        let t = before.term().unwrap();
        assert_eq!(screen_of(fresh), screen_of(t));
        assert_eq!(fresh.term.grid().cursor.point, t.term.grid().cursor.point);
        before.kill();
        after.kill();
    }
}
