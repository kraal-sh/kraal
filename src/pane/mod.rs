//! A pane: a process on a PTY with its terminal emulator, or a rich preview block.

use std::path::PathBuf;
use std::time::Instant;

use alacritty_terminal::event::{Event as TermEvent, EventListener, WindowSize};
use alacritty_terminal::grid::Scroll;
use alacritty_terminal::term::TermMode;
use anyhow::Result;
use crossbeam_channel::Sender;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

use crate::agent::AgentTracker;
use crate::input::PaneInputModes;
use crate::layout::PaneId;
use crate::preview::Preview;
use crate::server::Event;

mod commands;
mod guard;
mod osc;
mod pty;
mod screen;
mod scrollback;

pub use commands::{Finished, short_duration};
use osc::sanitize_title;
use osc::{Osc, OscScanner};
pub use osc::{osc_notify, osc52};
use pty::Size;
pub use pty::{ProcessHold, TermPane};
#[cfg(test)]
pub use screen::render_term;
pub use screen::set_outer_colors;
use screen::{SearchState, default_color};
pub use scrollback::MAX_SAVED_BYTES;

/// Every mode a program may have left set, back to how a fresh terminal starts, without RIS
/// (which would clear the scrollback a restarted pane keeps): the alternate screen, the kitty
/// keyboard stacks of both screens and the alternate screen's saved cursor, the scrolling region
/// (cursor kept), origin, cursor-key and keypad modes, wrapping, insert and newline modes, mouse
/// and focus reporting, bracketed paste and the cursor. Tab stops, character sets and colours
/// follow in [`reset_sequence`].
const RESET_MODES: &[u8] = b"\x1b[?1049l\x1b[?1049h\x1b[<4096u\x1b[=0;1u\x1b(B\x1b)B\x1b*B\x1b+B\x0f\x1b[0m\x1b7\
\x1b[?1049l\x1b[<4096u\x1b[=0;1u\
\x1b[?6l\x1b7\x1b[r\x1b8\x1b[?1l\x1b>\x1b[?7h\x1b[4l\x1b[20l\
\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1005l\x1b[?1006l\x1b[?1004l\x1b[?2004l\x1b[?1007h\
\x1b[?25h\x1b[0 q";

/// What a restarted pane's emulator gets: [`RESET_MODES`], then tab stops every 8 columns of
/// `cols` (the cursor kept), the four character sets back to ASCII with G0 in use, and the
/// colours. The character sets come after the cursor is restored, and the clean state is saved
/// last: a saved cursor carries character sets, and the old program's must not come back.
fn reset_sequence(cols: u16) -> Vec<u8> {
    let mut out = RESET_MODES.to_vec();
    out.extend_from_slice(b"\x1b7\x1b[3g");
    for col in (9..=cols).step_by(8) {
        out.extend_from_slice(format!("\x1b[{col}G\x1bH").as_bytes());
    }
    out.extend_from_slice(b"\x1b8\x1b(B\x1b)B\x1b*B\x1b+B\x0f\x1b[0m\r\n\x1b7");
    out
}

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
    /// The connection itself (user, port, jump host, key), when kraal started it: the entry
    /// named `server` may reach that machine another way.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connection: Option<crate::servers::Server>,
    /// Close the pane a few seconds after its command succeeds (transfers).
    pub close_on_success: bool,
    /// With `close_on_exit`: stay open when the command fails, to show why (tools).
    pub keep_on_failure: bool,
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
    /// An agent waiting for its account (the rules depend on what git says about its
    /// directory): what to start once that is known, kept across a restart.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_agent: Option<PendingAgent>,
    /// Environment holding secrets: given to the process only, never saved with the session.
    #[serde(skip)]
    pub secret_env: SecretEnv,
    /// Text to show as a preview instead of a file's (a paste buffer): in memory only, never
    /// saved with the session.
    #[serde(skip)]
    pub text: Option<ShownText>,
}

/// Text a preview pane shows, under a title. Debug output says how long it is, not what it says.
#[derive(Clone)]
pub struct ShownText {
    pub title: String,
    pub text: String,
}

impl std::fmt::Debug for ShownText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ShownText({:?}, {} bytes)", self.title, self.text.len())
    }
}

/// An agent of a workspace definition, before its account is chosen.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct PendingAgent {
    pub agent: String,
    pub args: Vec<String>,
    /// The workspace whose rules apply.
    pub workspace: Option<String>,
    /// Once chosen, wait for Enter (`start_suspended`) rather than start.
    pub suspended: bool,
    pub close_on_exit: bool,
}

/// Variables whose values must not show: debug output lists their names alone.
#[derive(Clone, Default)]
pub struct SecretEnv(pub Vec<(String, String)>);

impl std::fmt::Debug for SecretEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.0.iter().map(|(k, _)| k)).finish()
    }
}

/// Things the server must act on after feeding output to a pane.
#[derive(Debug, Clone)]
pub enum PaneSignal {
    Notification {
        title: String,
        body: String,
    },
    Clipboard(String),
    Bell,
    TitleChanged,
    /// A command the shell marked (OSC 133) ended.
    CommandDone(Finished),
    /// A shell reported the contexts it works on (OSC 7701): they count over SSH only.
    Contexts(crate::context::RemoteContexts),
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
    /// Current directory reported by the shell (OSC 7), and the machine it is on.
    pub reported_cwd: Option<PathBuf>,
    pub reported_host: String,
    /// The server this pane is connected to: from its command, or an `ssh` seen in the foreground.
    pub ssh: Option<crate::servers::Server>,
    /// What the commands run here last worked on (Kubernetes, AWS, Terraform), as seen from
    /// their command lines and environments.
    pub seen: crate::context::Seen,
    /// The contexts a shell over SSH reported at its last prompt, with the process they came
    /// through (the `ssh` in the foreground then): they go when it leaves the foreground.
    pub reported: Option<(u32, crate::context::RemoteContexts)>,
    /// A report came since the last command started. A prompt without one is from a shell that
    /// does not report (the one ssh came back to, or a `sudo -i` on the server): what was
    /// reported no longer holds. (A shell may mark a prompt twice: fish 4 marks its own.)
    reported_since_command: bool,
    /// When to close the pane automatically.
    pub close_at: Option<Instant>,
    /// Replace the spec and restart once the current process exits (account switch).
    pub restart_with: Option<PaneSpec>,
    /// The command has not been started yet (restored session, `start_suspended`, or it failed
    /// to start): Enter starts it.
    pub suspended: bool,
    /// What runs is a new worktree's setup, not the command: the command starts in its place
    /// once the setup succeeds, and waits for Enter when it fails.
    pub preparing: bool,
    search: Option<SearchState>,
    /// The line that marks where text saved before a restart ends, once it was given back.
    restored: Option<String>,
    /// The kraal session of a server that a kraal client running here says it shows.
    attachment: Option<RemoteAttachment>,
    /// Finds the local ports the output mentions (dev servers).
    pub ports: crate::server::net::PortScanner,
}

/// A kraal client in a pane, attached to a server's session (`kraal attach --host`), as it
/// says itself.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteAttachment {
    /// The server, by its name in the inventory.
    pub server: String,
    pub session: String,
    /// That session tells which of its panes this client shows: an older kraal there does not.
    pub reports: bool,
    /// The pane it shows there, as last told.
    pub shows: Option<u32>,
    /// The foreground process group when the client said so: it holds while that client runs.
    group: Option<i32>,
}

/// Environment given to every pane process.
pub struct PaneEnv {
    pub socket: PathBuf,
    pub session: String,
    pub shell: String,
    pub scrollback: usize,
    /// Images: whether they can be shown, the outer terminal's cell size, the session quota.
    pub graphics: std::sync::Arc<crate::graphics::Shared>,
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
        let mut spec = spec;
        let content = if let Some(shown) = spec.text.take() {
            Content::Preview(Box::new(Preview::of_text(&shown.title, &shown.text)))
        } else if let Some(path) = &spec.preview {
            let mut preview = Preview::open(path);
            if env.graphics.enabled() {
                preview.set_graphics(env.graphics.clone());
            }
            let tx = tx.clone();
            preview.on_change(move || {
                let _ = tx.try_send(Event::Wake);
            });
            Content::Preview(Box::new(preview))
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
            reported_host: String::new(),
            seen: Default::default(),
            reported: None,
            reported_since_command: false,
            ssh: None,
            close_at: None,
            restart_with: None,
            suspended: false,
            preparing: false,
            search: None,
            restored: None,
            attachment: None,
            ports: Default::default(),
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
            reported_host: String::new(),
            seen: Default::default(),
            reported: None,
            reported_since_command: false,
            ssh: None,
            close_at: None,
            restart_with: None,
            suspended: false,
            preparing: false,
            search: None,
            restored: None,
            attachment: None,
            ports: Default::default(),
        }
    }

    /// Restart the command of an exited pane, keeping its scrollback.
    pub fn respawn(&mut self, env: &PaneEnv, tx: &Sender<Event>) -> Result<()> {
        let Content::Term(old) = &mut self.content else { return Ok(()) };
        let mut fresh = TermPane::spawn(self.id, &self.spec, self.cols, self.rows, env, tx)?;
        // Keep history: move the old emulator over and reset its modes.
        std::mem::swap(&mut fresh.term, &mut old.term);
        std::mem::swap(&mut fresh.events, &mut old.events);
        std::mem::swap(&mut fresh.commands, &mut old.commands);
        // The images its scrollback shows stay with it.
        std::mem::swap(&mut fresh.graphics, &mut old.graphics);
        let reset = reset_sequence(self.cols);
        fresh.marked(|t| t.parser.advance(&mut t.term, &reset));
        **old = fresh;
        // A new process: nothing of the old one's lifecycle carries over, but its conversation.
        self.reported = None;
        self.reported_since_command = false;
        self.exit_code = None;
        self.suspended = false;
        self.close_at = None;
        let session = self.agent.session_id.take();
        self.agent = AgentTracker { session_id: session, ..AgentTracker::default() };
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

    /// A preview of text kraal holds rather than of a file: nothing of it is saved.
    pub fn shows_text(&self) -> bool {
        matches!(&self.content, Content::Preview(p) if p.in_memory())
    }

    /// The preview this pane shows of `path`, if it is one.
    pub fn preview_of(&mut self, path: &std::path::Path) -> Option<&mut Preview> {
        match &mut self.content {
            Content::Preview(p) if p.path() == path => Some(p),
            _ => None,
        }
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
            let _ = t.master.resize(pty::pty_size(cols, rows, t.graphics.shared()));
            t.resize_marked(Size { cols, rows });
        }
    }

    /// Feed process output. Returns signals for the server.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<PaneSignal> {
        self.last_output = Instant::now();
        let Content::Term(t) = &mut self.content else { return Vec::new() };
        let mut signals = Vec::new();
        // The emulator gets the output up to each command mark, so that the mark lands where
        // the shell put it.
        let mut start = 0;
        for (end, osc) in t.osc.scan(bytes) {
            match osc {
                Osc::Notify { title, body } => signals.push(PaneSignal::Notification { title, body }),
                Osc::Cwd { host, path } => {
                    self.reported_cwd = Some(path);
                    self.reported_host = host;
                }
                Osc::Mark(mark) => {
                    t.advance(&bytes[start..end]);
                    start = end;
                    signals.extend(t.mark(mark).map(PaneSignal::CommandDone));
                    match mark {
                        commands::Mark::Output => self.reported_since_command = false,
                        commands::Mark::Prompt if !self.reported_since_command => {
                            // A report read earlier in this output is just as old: the server
                            // must not take it after this.
                            self.reported = None;
                            signals.retain(|s| !matches!(s, PaneSignal::Contexts(_)));
                        }
                        _ => {}
                    }
                }
                // The answer goes after the replies to what the program asked before, and
                // before those to what it asks after (the shell reads up to the next one).
                Osc::Query => {
                    t.advance(&bytes[start..end]);
                    start = end;
                    t.events.borrow_mut().push(TermEvent::PtyWrite(osc::KRAAL_ANSWER.into()));
                }
                Osc::Contexts(contexts) => {
                    self.reported_since_command = true;
                    signals.push(PaneSignal::Contexts(contexts));
                }
                Osc::Attached(to) => {
                    let group = t.master.process_group_leader();
                    self.attachment = to.map(|(server, session)| RemoteAttachment {
                        server,
                        session,
                        reports: false,
                        shows: None,
                        group,
                    });
                }
                Osc::Shows(pane) => {
                    if let Some(a) = &mut self.attachment {
                        a.reports = true;
                        a.shows = pane;
                    }
                }
            }
        }
        t.advance(&bytes[start..]);
        self.drain_events(&mut signals);
        signals
    }

    /// Tell the program the pane's size in pixels again: the outer terminal's cells changed.
    pub fn refresh_pixel_size(&mut self) {
        if let Content::Term(t) = &mut self.content {
            let _ = t.master.resize(pty::pty_size(self.cols, self.rows, t.graphics.shared()));
        }
    }

    /// Make the image placeholders `area` shows from this pane displayable, or blank when the
    /// client cannot show images. A preview's image becomes placeholders when it can.
    pub fn show_images(&mut self, area: Rect, buf: &mut Buffer, can_show: bool) {
        let size = (self.cols, self.rows);
        match &mut self.content {
            Content::Term(t) => t.graphics.show_cells(area, buf, can_show),
            Content::Preview(p) if can_show => {
                p.show_image(area, size, buf);
            }
            _ => {}
        }
    }

    /// The images this pane shows, by kraal's id.
    pub fn images(&self) -> Vec<(u32, crate::graphics::Live)> {
        match &self.content {
            Content::Term(t) => t
                .graphics
                .shown()
                .map(|(kid, s)| (kid, (s.generation, s.picture.clone(), s.placements.clone())))
                .collect(),
            Content::Preview(p) => p.image().into_iter().collect(),
            Content::Files(_) => Vec::new(),
        }
    }

    /// The output read from the PTY since the last call, put in `into` (what it held is
    /// dropped, its room reused): false when there was none. Then the server is told of the
    /// next output, unless it `comes_back` for it on its own.
    pub fn take_output_into(&self, into: &mut Vec<u8>, comes_back: bool) -> bool {
        self.term().is_some_and(|t| t.output.take(into, comes_back))
    }

    #[cfg(test)]
    pub fn take_output(&self) -> Option<Vec<u8>> {
        let mut out = Vec::new();
        self.take_output_into(&mut out, false).then_some(out)
    }

    pub fn has_output(&self) -> bool {
        self.term().is_some_and(|t| !t.output.is_empty())
    }

    /// Flush a synchronized update whose timeout expired: the signals of what it held (replies
    /// to queries are sent to the program right away), or None when nothing was due.
    pub fn poll_sync_timeout(&mut self) -> Option<Vec<PaneSignal>> {
        let Content::Term(t) = &mut self.content else { return None };
        match t.parser.sync_timeout().sync_timeout() {
            Some(deadline) if deadline <= Instant::now() => {
                t.marked(|t| t.parser.stop_sync(&mut t.term));
                let mut signals = Vec::new();
                self.drain_events(&mut signals);
                Some(signals)
            }
            _ => None,
        }
    }

    fn drain_events(&mut self, signals: &mut Vec<PaneSignal>) {
        let Content::Term(t) = &mut self.content else { return };
        let events: Vec<TermEvent> = t.events.borrow_mut().drain(..).collect();
        for ev in events {
            match ev {
                TermEvent::PtyWrite(s) => t.write(t.graphics.device_attributes(&s).into_bytes()),
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
                    let (cell_width, cell_height) = t.graphics.shared().cell();
                    let size = WindowSize { num_lines: self.rows, num_cols: self.cols, cell_width, cell_height };
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

    /// Hang up, as closing a terminal does (SIGHUP to the program and its foreground job).
    pub fn kill(&mut self) {
        if let Content::Term(t) = &mut self.content {
            t.hang_up();
        }
    }

    /// Send `signal` to the pane's process group and its foreground job (what runs in it).
    pub fn signal(&self, signal: i32) {
        let Some(t) = self.term() else { return };
        t.process.signal_group(signal);
        #[cfg(unix)]
        if let Some(fg) = self.foreground_pid().filter(|fg| Some(*fg) != t.pid) {
            // SAFETY: plain syscall to the terminal's current foreground group.
            unsafe {
                libc::kill(-(fg as libc::pid_t), signal);
            }
        }
    }

    pub fn pid(&self) -> Option<u32> {
        self.term().and_then(|t| t.pid)
    }

    /// A hold on the pane's current process (see [`ProcessHold`]).
    pub fn process_hold(&self) -> Option<ProcessHold> {
        self.term().map(|t| ProcessHold::new(&t.process))
    }

    /// The server's session a kraal client running here shows, while the client that said so
    /// is still the one in the foreground (one that was killed says nothing more).
    pub fn attachment(&self) -> Option<&RemoteAttachment> {
        let a = self.attachment.as_ref()?;
        let group = self.term()?.master.process_group_leader();
        (self.exit_code.is_none() && a.group.is_some() && a.group == group).then_some(a)
    }

    /// Command line of the foreground process group leader.
    pub fn foreground_argv(&self) -> Option<Vec<String>> {
        let t = self.term()?;
        let pgid = t.master.process_group_leader()?;
        crate::process::argv(pgid as u32)
    }

    /// Whether something other than the pane's shell is in the foreground.
    pub fn has_foreground_job(&self) -> bool {
        let Some(t) = self.term() else { return false };
        match (t.master.process_group_leader(), t.pid) {
            (Some(pg), Some(pid)) => pg as u32 != pid,
            _ => false,
        }
    }

    /// Whether a shell waits at its prompt here: nothing else in the foreground, and what the
    /// pane runs is a shell (not `vim` started as the pane's command, or by `exec`). When the
    /// system does not say what runs, only the foreground counts.
    pub fn shell_waiting(&self) -> bool {
        if self.has_foreground_job() {
            return false;
        }
        let program = self.pid().and_then(crate::process::argv).and_then(|a| a.into_iter().next());
        program.is_none_or(|p| is_shell(&p))
    }

    /// Best-known working directory on this machine: OSC 7, else the shell's.
    /// A directory reported by a shell on another machine (over ssh) does not count.
    pub fn current_cwd(&self) -> Option<PathBuf> {
        if let Some(p) = &self.reported_cwd
            && !self.reported_remotely()
        {
            return Some(p.clone());
        }
        crate::process::cwd(self.pid()?)
    }

    /// The directory a shell on another machine reported (a remote shell that sends OSC 7).
    pub fn remote_cwd(&self) -> Option<PathBuf> {
        if self.reported_remotely() { self.reported_cwd.clone() } else { None }
    }

    fn reported_remotely(&self) -> bool {
        let host = self.reported_host.as_str();
        if host.is_empty() || host == "localhost" {
            return false;
        }
        static LOCAL: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
        let local = LOCAL.get_or_init(crate::context::hostname);
        // Shells report either the short or the full name.
        local.as_deref().is_some_and(|l| {
            let short = |s: &str| s.split('.').next().unwrap_or(s).to_ascii_lowercase();
            short(l) != short(host)
        })
    }

    /// Process id of the foreground process group leader.
    pub fn foreground_pid(&self) -> Option<u32> {
        let t = self.term()?;
        t.master.process_group_leader().map(|p| p as u32)
    }
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
        .map(crate::exec::reap_later)
}

/// Whether `program` (an argv[0]: a path, or `-zsh` for a login shell) is a shell.
pub fn is_shell(program: &str) -> bool {
    let name = program.rsplit('/').next().unwrap_or(program).trim_start_matches('-');
    matches!(
        name,
        "sh" | "bash"
            | "zsh"
            | "fish"
            | "dash"
            | "ash"
            | "ksh"
            | "mksh"
            | "yash"
            | "tcsh"
            | "csh"
            | "nu"
            | "xonsh"
            | "elvish"
            | "pwsh"
    )
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use alacritty_terminal::term::{Config as TermConfig, Term};
    use alacritty_terminal::vte::ansi::{Processor, Rgb};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Modifier};

    use super::pty::Listener;
    use super::screen::{parse_xcolor, render_term, url_in_text};
    use super::*;
    use super::{pty, reset_sequence};

    #[test]
    fn a_restarted_pane_starts_with_fresh_modes() {
        let size = pty::Size { cols: 20, rows: 6 };
        let config = TermConfig { kitty_keyboard: true, ..TermConfig::default() };
        let mut term = Term::new(config, &size, Listener::default());
        let mut parser: Processor = Processor::new();
        parser.advance(
            &mut term,
            b"\x1b[?1049h\x1b[>1u\x1b[?1049l\x1b[>3u\x1b[?1h\x1b=\x1b[?6h\x1b[2;4r\x1b[?1003h\x1b[?1004h\
              \x1b[?2004h\x1b[4h\x1b[?25l\x1b(0\x1b7\x1b[?1049h",
        );
        // Line drawing in G1, selected with SO, and a single tab stop at column 3; line drawing
        // in G0 too, kept in the alternate screen's saved cursor (the primary's is kept above).
        parser.advance(&mut term, b"\x1b)0\x0e\x1b[3g\x1b[3G\x1bH\r\x1b(0\x1b7");
        assert_ne!(*term.mode(), TermMode::default());
        parser.advance(&mut term, &reset_sequence(20));
        assert_eq!(*term.mode(), TermMode::default());
        // ASCII again, and tab stops every 8 columns.
        parser.advance(&mut term, b"\rq\tx");
        let line = term.grid().cursor.point.line;
        let row = &term.grid()[line];
        assert_eq!(row[alacritty_terminal::index::Column(0)].c, 'q');
        assert_eq!(row[alacritty_terminal::index::Column(8)].c, 'x');
        // A saved cursor brings back no character set of the old program, on either screen.
        let restored_q = |term: &mut Term<Listener>, parser: &mut Processor| {
            parser.advance(term, b"\x1b8q");
            let at = term.grid().cursor.point;
            term.grid()[at.line][at.column - 1].c
        };
        assert_eq!(restored_q(&mut term, &mut parser), 'q');
        // The other screen's keyboard modes are gone too.
        parser.advance(&mut term, b"\x1b[?1049h");
        assert_eq!(*term.mode() & !TermMode::ALT_SCREEN, TermMode::default());
        assert_eq!(restored_q(&mut term, &mut parser), 'q');
    }

    #[test]
    fn replies_held_in_a_synchronized_update_go_out_when_it_times_out() {
        let (tx, _rx) = crossbeam_channel::bounded(64);
        let env = PaneEnv {
            socket: "/nonexistent".into(),
            session: "t".into(),
            shell: "sh".into(),
            scrollback: 100,
            graphics: Default::default(),
        };
        // The program asks where the cursor is inside a synchronized update that never ends, then
        // prints the answer it reads.
        let spec = PaneSpec {
            command: vec![
                "sh".into(),
                "-c".into(),
                "stty raw -echo; printf '\\033[?2026h\\033[6n'; r=$(dd bs=1 count=6 2>/dev/null); \
                 printf 'got%s' \"$r\"; sleep 1"
                    .into(),
            ],
            ..Default::default()
        };
        let mut pane = Pane::spawn(1, None, spec, 40, 5, &env, &tx).unwrap();
        let start = Instant::now();
        let mut seen = String::new();
        while !seen.contains('R') && start.elapsed() < std::time::Duration::from_secs(5) {
            while let Some(data) = pane.take_output() {
                seen.push_str(&String::from_utf8_lossy(&data));
                pane.feed(&data);
            }
            pane.poll_sync_timeout();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(seen.contains("got\x1b[1;1R"), "the reply never reached the program: {seen:?}");
    }

    /// A shell over SSH asks whether its terminal is kraal, then for the terminal's attributes:
    /// kraal's answer comes first, as the shell reads up to the attributes.
    #[test]
    fn kraal_answers_the_question_whether_it_is_kraal_before_the_attributes() {
        let (tx, _rx) = crossbeam_channel::bounded(64);
        let env = PaneEnv {
            socket: "/nonexistent".into(),
            session: "t".into(),
            shell: "sh".into(),
            scrollback: 100,
            graphics: Default::default(),
        };
        let spec = PaneSpec {
            command: vec![
                "sh".into(),
                "-c".into(),
                "stty raw -echo; printf '\\033[c\\033]7701;?\\007\\033[c'; r=$(dd bs=1 count=23 2>/dev/null); \
                 printf 'got%s.' \"$r\" | od -An -c; sleep 1"
                    .into(),
            ],
            ..Default::default()
        };
        let mut pane = Pane::spawn(1, None, spec, 80, 5, &env, &tx).unwrap();
        let start = Instant::now();
        let mut seen = String::new();
        while !seen.contains('.') && start.elapsed() < std::time::Duration::from_secs(5) {
            while let Some(data) = pane.take_output() {
                seen.push_str(&String::from_utf8_lossy(&data));
                pane.feed(&data);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        // `od -c` spells ESC as 033 and BEL as \a.
        let seen: String = seen.split_whitespace().collect();
        assert!(seen.contains("got033[?6c033]7701;kraal\\a033[?6c."), "{seen:?}");
    }

    #[test]
    fn kraal_sequences_are_read_and_a_prompt_without_a_report_ends_it() {
        let mut s = OscScanner::default();
        let data = b"\x1b]7701;?\x07x\x1b]7701;context;k8s=prod;ns=web;tf=;aws=ops\x1b\\\x1b]7701;kraal\x07";
        let found: Vec<Osc> = data.chunks(5).flat_map(|c| s.scan(c)).map(|(_, o)| o).collect();
        let reported = crate::context::RemoteContexts::parse("k8s=prod;ns=web;aws=ops");
        assert_eq!(found, vec![Osc::Query, Osc::Contexts(reported.clone())], "an answer is not a question");

        let (tx, _rx) = crossbeam_channel::bounded(64);
        let env = PaneEnv {
            socket: "/nonexistent".into(),
            session: "t".into(),
            shell: "sh".into(),
            scrollback: 100,
            graphics: Default::default(),
        };
        let spec = PaneSpec { command: vec!["sleep".into(), "5".into()], ..Default::default() };
        let mut pane = Pane::spawn(1, None, spec, 40, 5, &env, &tx).unwrap();
        let signals = pane.feed(b"\x1b]133;D;0\x07\x1b]7701;context;k8s=prod;ns=web;aws=ops\x07");
        let got: Vec<_> =
            signals.into_iter().filter_map(|s| if let PaneSignal::Contexts(c) = s { Some(c) } else { None }).collect();
        assert_eq!(got, vec![reported.clone()]);
        // The server took it (over SSH); the prompt that follows the report keeps it, marked
        // twice as fish 4 does, and so does an empty command line.
        pane.reported = Some((1, reported));
        pane.feed(b"\x1b]133;A\x07\x1b]133;A;click_events=1\x1b\\$ \x1b]133;B\x07\r\n\x1b]133;A\x07$ ");
        assert!(pane.reported.is_some());
        // A command runs (`ssh` ends, `sudo -i`), then a prompt from a shell that does not
        // report: what was reported no longer holds.
        pane.feed(b"exit\r\n\x1b]133;C\x07\x1b]133;D;0\x07\x1b]133;A\x07$ ");
        assert!(pane.reported.is_none());
        pane.kill();
    }

    /// The report, the command and the prompt without a report, read at once: the report is as
    /// gone as when they come in several reads.
    #[test]
    fn a_prompt_without_a_report_ends_one_read_in_the_same_output() {
        let (tx, _rx) = crossbeam_channel::bounded(64);
        let env = PaneEnv {
            socket: "/nonexistent".into(),
            session: "t".into(),
            shell: "sh".into(),
            scrollback: 100,
            graphics: Default::default(),
        };
        let spec = PaneSpec { command: vec!["sleep".into(), "5".into()], ..Default::default() };
        let mut pane = Pane::spawn(1, None, spec, 40, 5, &env, &tx).unwrap();
        let contexts = |signals: Vec<PaneSignal>| signals.into_iter().any(|s| matches!(s, PaneSignal::Contexts(_)));
        let output: &[u8] =
            b"\x1b]133;D;0\x07\x1b]7701;context;k8s=prod;ns=web;aws=ops\x07\x1b]133;A\x07$ \x1b]133;B\x07\
            exit\r\n\x1b]133;C\x07\x1b]133;D;0\x07\x1b]133;A\x07$ ";
        assert!(!contexts(pane.feed(output)), "a report the prompt after it ended");
        assert!(pane.reported.is_none());
        // A report after the prompt that ends the one before stays.
        let next: &[u8] = b"exit\r\n\x1b]133;C\x07\x1b]133;D;0\x07\x1b]133;A\x07\x1b]7701;context;k8s=dev;tf=;aws=\x07";
        assert!(contexts(pane.feed(next)));
        pane.kill();
    }

    #[test]
    fn a_command_never_runs_in_another_directory_than_its_own() {
        let (tx, _rx) = crossbeam_channel::bounded(64);
        let env = PaneEnv {
            socket: "/nonexistent".into(),
            session: "t".into(),
            shell: "sh".into(),
            scrollback: 100,
            graphics: Default::default(),
        };
        let gone = Some(PathBuf::from("/nonexistent/kraal-test-dir"));
        let command = PaneSpec { command: vec!["true".into()], cwd: gone.clone(), ..Default::default() };
        let err = Pane::spawn(1, None, command, 20, 5, &env, &tx).err().expect("refused");
        assert!(format!("{err:#}").contains("kraal-test-dir"), "{err:#}");
        let typed = PaneSpec { startup_input: Some("rm -rf build\r".into()), cwd: gone.clone(), ..Default::default() };
        assert!(Pane::spawn(2, None, typed, 20, 5, &env, &tx).is_err());
        // A plain shell starts at home instead.
        let mut shell = Pane::spawn(3, None, PaneSpec { cwd: gone, ..Default::default() }, 20, 5, &env, &tx).unwrap();
        shell.kill();
    }

    #[test]
    fn shells_are_told_from_other_programs() {
        for shell in ["zsh", "-zsh", "/bin/bash", "/usr/bin/fish", "sh"] {
            assert!(is_shell(shell), "{shell}");
        }
        for other in ["vim", "/usr/bin/nvim", "ssh", "python3", "zshell"] {
            assert!(!is_shell(other), "{other}");
        }
    }

    #[test]
    fn osc_scanner_finds_notifications_across_chunks() {
        let mut s = OscScanner::default();
        let data = b"hello\x1b]777;notify;Claude;needs you\x07 and \x1b]9;done!\x1b\\ \x1b]9;4;1;50\x07";
        let mut found = Vec::new();
        for chunk in data.chunks(3) {
            found.extend(s.scan(chunk).into_iter().map(|(_, o)| o));
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
    fn a_notification_sent_both_ways_at_once_is_one() {
        // What kraal itself sends a terminal for one notification, read in pieces.
        let mut s = OscScanner::default();
        let twice =
            [crate::pane::osc_notify("claude · api", "needs your input"), crate::pane::osc_notify("x", "y")].concat();
        let mut found = Vec::new();
        for chunk in twice.chunks(5) {
            found.extend(s.scan(chunk).into_iter().map(|(_, o)| o));
        }
        assert_eq!(
            found,
            vec![
                Osc::Notify { title: "claude · api".into(), body: "needs your input".into() },
                Osc::Notify { title: "x".into(), body: "y".into() },
            ]
        );
        // Apart, or saying something else, they are two.
        let apart = b"\x1b]777;notify;a;b\x07 \x1b]9;a: b\x07\x1b]777;notify;c;d\x07\x1b]9;other\x07";
        assert_eq!(s.scan(apart).len(), 4);
    }

    #[test]
    fn a_kraal_attached_to_a_server_says_so() {
        let mut s = OscScanner::default();
        let found: Vec<Osc> = s
            .scan(b"\x1b]7701;attach;gpu;main\x07\x1b]7701;shows;12\x1b\\\x1b]7701;shows;\x07\x1b]7701;attach\x07")
            .into_iter()
            .map(|(_, o)| o)
            .collect();
        assert_eq!(
            found,
            vec![
                Osc::Attached(Some(("gpu".into(), "main".into()))),
                Osc::Shows(Some(12)),
                Osc::Shows(None),
                Osc::Attached(None),
            ]
        );
        // Names that are no server's or session's are not taken.
        assert!(s.scan(b"\x1b]7701;attach;g pu;main\x07\x1b]7701;attach;gpu;../x\x07").is_empty());
    }

    #[test]
    fn osc7_cwd() {
        let mut s = OscScanner::default();
        let found: Vec<Osc> =
            s.scan(b"\x1b]7;file://laptop/home/me/My%20Dir\x07").into_iter().map(|(_, o)| o).collect();
        assert_eq!(found, vec![Osc::Cwd { host: "laptop".into(), path: PathBuf::from("/home/me/My Dir") }]);
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
    fn urls_are_found_under_wide_characters_columns() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let mut term = Term::new(TermConfig::default(), &Size { cols: 30, rows: 1 }, Listener(events));
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, "日本 https://example.org".as_bytes());
        let (line, index) = super::screen::row_chars(&term, alacritty_terminal::index::Line(0));
        // The last column of the URL (two wide characters before it take four columns).
        let col = 4 + "https://example.org".len();
        assert_eq!(url_in_text(&line, index[col]).as_deref(), Some("https://example.org"));
        assert_eq!(url_in_text(&line, index[1]), None);
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
