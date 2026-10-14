//! The process side of a terminal pane: its PTY, the threads that read and write it, and the
//! emulator its output feeds.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::vte::ansi::Processor;
use anyhow::{Context, Result};
use crossbeam_channel::Sender;
use portable_pty::{CommandBuilder, MasterPty, PtySize};

use super::archive::Archive;
use super::commands::Commands;
use super::guard::KeyboardStackGuard;
use super::*;
use crate::layout::PaneId;
use crate::server::{Event, EventSender};

#[derive(Clone, Default)]
pub struct Listener(pub(super) Rc<RefCell<Vec<TermEvent>>>);

impl EventListener for Listener {
    fn send_event(&self, event: TermEvent) {
        self.0.borrow_mut().push(event);
    }
}

#[derive(Clone, Copy)]
pub(super) struct Size {
    pub(super) cols: u16,
    pub(super) rows: u16,
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

/// Input a pane may have waiting for its program to read.
pub(super) const MAX_PENDING_INPUT: usize = 16 << 20;

/// The pane's process. It is reaped only when the pane lets it go: until then (a zombie once it
/// ended) its pid, and the number of the process group it leads, belong to nothing else, so a
/// signal sent to them cannot reach another program.
#[derive(Default)]
pub(super) struct Process {
    token: String,
    /// Set once it started (a pane set up in a batch starts its program a moment later).
    pid: std::sync::OnceLock<u32>,
    child: Mutex<Option<PaneChild>>,
}

impl Process {
    /// The process `child` is.
    #[cfg(test)]
    pub(super) fn of(child: PaneChild) -> Process {
        let process = Process::default();
        process.started(child);
        process
    }

    /// The program started: this is its process.
    fn started(&self, child: PaneChild) {
        if let Some(pid) = child.process_id() {
            let _ = self.pid.set(pid);
        }
        *self.child.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
    }

    pub(super) fn pid(&self) -> Option<u32> {
        self.pid.get().copied()
    }

    /// Send `signal` to the process and its group (the pane's session leads one, with what it
    /// started in the foreground).
    pub(super) fn signal_group(&self, signal: i32) {
        let Some(pid) = self.pid() else { return };
        let child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        if child.is_none() {
            return;
        }
        #[cfg(unix)]
        // SAFETY: plain syscalls; the pid and the group number are ours while the child is.
        unsafe {
            let pid = pid as libc::pid_t;
            libc::kill(pid, signal);
            libc::kill(-pid, signal);
        }
        let _ = (child, signal);
    }

    /// Hang up the whole session the process leads, as a terminal going away would for good:
    /// background jobs in groups of their own too. What left the session (a daemon, `setsid`)
    /// is not the pane's anymore.
    pub(super) fn hang_up_session(&self) {
        let Some(pid) = self.pid() else { return };
        let child = self.child.lock().unwrap_or_else(|e| e.into_inner());
        if child.is_none() {
            return;
        }
        #[cfg(unix)]
        for member in crate::process::session_members(pid as libc::pid_t) {
            // SAFETY: plain syscall; the session is ours while its leader is (unreaped).
            unsafe {
                libc::kill(member, libc::SIGHUP);
            }
        }
        drop(child);
    }
}

/// A hold on a pane's process, for whoever must signal it after the pane let it go: while one
/// is kept, the process is not reaped, so neither its pid nor the number of the process group it
/// leads can be given to another program.
#[derive(Clone)]
pub struct ProcessHold(Arc<Process>);

impl ProcessHold {
    pub(super) fn new(process: &Arc<Process>) -> ProcessHold {
        ProcessHold(process.clone())
    }

    /// Whether both hold the same process (not one restarted in the pane since).
    pub fn is(&self, other: &ProcessHold) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// Whether anything still runs in the process group the process leads, the process itself
    /// included; an ended, unreaped process does not count.
    pub fn group_running(&self) -> bool {
        self.0.pid().is_some_and(|pid| crate::process::group_running(pid as libc::pid_t))
    }

    /// The running members of the session whose leader this hold keeps unreaped.
    pub fn session_members(&self) -> Vec<u32> {
        self.0
            .pid()
            .map(|pid| crate::process::session_members(pid as libc::pid_t).into_iter().map(|p| p as u32).collect())
            .unwrap_or_default()
    }

    pub fn session_owner(&self) -> Option<crate::process::SessionOwner> {
        let leader = self.0.pid()?;
        Some(crate::process::SessionOwner {
            leader,
            started: crate::process::start_time(leader)?,
            token: self.0.token.clone(),
        })
    }

    /// Send `signal` to the process and its group.
    pub fn signal(&self, signal: i32) {
        self.0.signal_group(signal);
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        let Some(mut child) = self.child.get_mut().unwrap_or_else(|e| e.into_inner()).take() else { return };
        // One still running (its pane went first) is reaped when it ends, off this thread. The
        // reaper exists before any pane's program does, so this cannot fail for want of a thread.
        if !matches!(child.try_wait(), Ok(Some(_)))
            && let Some(reaper) = REAPER.get()
        {
            let _ = reaper.send(child);
        }
    }
}

use super::spawn::{Launch, PaneChild};

static REAPER: std::sync::OnceLock<crossbeam_channel::Sender<PaneChild>> = std::sync::OnceLock::new();

/// Start, once, the thread that reaps the programs of closed panes as they end.
fn start_reaper() -> Result<()> {
    if REAPER.get().is_some() {
        return Ok(());
    }
    let (tx, rx) = crossbeam_channel::unbounded::<PaneChild>();
    crate::threads::named("pane-reaper").spawn(move || {
        let mut waiting: Vec<PaneChild> = Vec::new();
        loop {
            let next = if waiting.is_empty() {
                rx.recv().map_err(|_| crossbeam_channel::RecvTimeoutError::Disconnected)
            } else {
                rx.recv_timeout(std::time::Duration::from_millis(200))
            };
            match next {
                Ok(child) => waiting.push(child),
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) if waiting.is_empty() => return,
                Err(_) => {}
            }
            waiting.retain_mut(|c| matches!(c.try_wait(), Ok(None)));
        }
    })?;
    // Another thread may have started one first: this one then ends with its channel.
    let _ = REAPER.set(tx);
    Ok(())
}

pub struct TermPane {
    pub term: Term<Listener>,
    /// The part of the history the emulator no longer holds.
    pub(super) archive: Archive,
    pub(super) events: Rc<RefCell<Vec<TermEvent>>>,
    pub(super) parser: Processor,
    /// The start of a character the last output ended with, cut there.
    pub(super) cut: Vec<u8>,
    pub(super) master: Box<dyn MasterPty + Send>,
    pub(super) input: Sender<Vec<u8>>,
    /// Bytes sent to the PTY not written yet: a program that stops reading its input cannot
    /// make kraal hold an unbounded amount of it.
    pub(super) input_pending: Arc<AtomicUsize>,
    /// The PTY does not wait when it is full: input is written to it at once, from the server's
    /// thread, while nothing waits for the writer thread (see [`start_io`]).
    direct: bool,
    /// Input was dropped because of that, since the server last asked.
    pub input_dropped: bool,
    /// Replies to the program held back by one that an image being prepared owes (`None`): it
    /// gets them in the order it asked.
    owed: VecDeque<Option<Vec<u8>>>,
    /// Output read from the PTY, waiting to be parsed.
    pub(super) output: Arc<Output>,
    pub(super) process: Arc<Process>,
    /// Tell the reader and the writer to stop, even if the program never writes again or
    /// never reads its input (it may outlive the pane, ignoring the hangup).
    reader_stop: ReaderStop,
    writer_stop: WriterStop,
    /// The program, until it is started (see [`super::spawn::batch`]), and what its waiter
    /// thread is then given.
    pending: Option<(Launch, Waiter)>,
    /// Gone with the pane: tells a program not started yet that it is not to be (see
    /// [`Pending::start`]).
    here: Arc<()>,
    pub(super) osc: OscScanner,
    pub(super) guard: KeyboardStackGuard,
    /// The shell's commands, as it marks them.
    pub(super) commands: Commands,
    pub graphics: crate::graphics::PaneGraphics,
    /// The screen as last drawn, for frames.
    pub screen: super::cache::ScreenCache,
    /// Counts the changes made to the emulator's grid (see `TermPane::marked`): the screen
    /// needs no look while it stays the same.
    pub(super) changes: u64,
    /// Counts the times the archive moved rows on the grid, which the emulator's damage does
    /// not tell (see `archive.rs`): the screen is looked at whole again.
    pub(super) moved: u64,
    /// The process's directory as last read for a frame.
    pub(super) cwd_seen: RefCell<Option<super::CwdSeen>>,
    /// The lines of history the emulator keeps.
    scrollback: usize,
}

/// The emulator of a pane of `size` that keeps `scrollback` lines of history.
fn emulator(scrollback: usize, size: Size, events: &Rc<RefCell<Vec<TermEvent>>>) -> Term<Listener> {
    // Programs may enable the kitty keyboard protocol (Claude Code does, for Shift+Enter).
    let config = TermConfig { scrolling_history: scrollback, kitty_keyboard: true, ..TermConfig::default() };
    Term::new(config, &size, Listener(events.clone()))
}

/// A PTY of `cols × rows`, with its size in pixels from the outer terminal's cells: programs
/// that draw images size them from it.
pub(super) fn pty_size(cols: u16, rows: u16, graphics: &crate::graphics::Shared) -> PtySize {
    let (cw, ch) = graphics.cell();
    PtySize { rows, cols, pixel_width: cols.saturating_mul(cw), pixel_height: rows.saturating_mul(ch) }
}

impl Drop for TermPane {
    fn drop(&mut self) {
        self.reader_stop.stop();
        self.writer_stop.stop();
    }
}

impl TermPane {
    pub(super) fn spawn(
        id: PaneId,
        spec: &PaneSpec,
        cols: u16,
        rows: u16,
        env: &PaneEnv,
        tx: &EventSender,
    ) -> Result<TermPane> {
        // A command, or input typed into the shell, meant for a directory never runs in another
        // one: a cleanup started in the wrong place deletes the wrong files. A plain shell starts
        // at home when its directory is gone.
        let strict = !spec.command.is_empty() || spec.startup_input.is_some();
        let (cwd, own_dir) = spawn_dir(spec.cwd.as_deref(), strict)?;
        start_reaper().context("starting the thread that reaps panes' programs")?;
        let pty = portable_pty::native_pty_system();
        let pair = pty.openpty(pty_size(cols, rows, &env.graphics)).context("opening a pty")?;
        let program: Vec<&OsStr> = if spec.command.is_empty() {
            vec![OsStr::new(&env.shell)]
        } else {
            spec.command.iter().map(OsStr::new).collect()
        };
        if own_dir {
            // Run through `sh`, a program that cannot be found would only show as sh's error:
            // the same failure to start as without it, instead.
            let path = spec.env.iter().rev().find(|(k, _)| k == "PATH").map(|(_, v)| v.into());
            let path = if spec.env_remove.iter().any(|k| k == "PATH") {
                None
            } else {
                path.or_else(|| std::env::var_os("PATH"))
            };
            if !findable(program[0], &cwd, path.as_deref()) {
                anyhow::bail!("starting {:?}: not found", program[0]);
            }
        }
        let mut cmd = command_in(&program, &cwd, own_dir);
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
        for (k, v) in spec.env.iter().chain(&spec.secret_env.0) {
            cmd.env(k, v);
        }
        let token = crate::accounts::new_uuid();
        cmd.env("KRAAL_PROCESS", &token);
        // Ready to start: a program that cannot be found fails here. It starts below, or after
        // the other panes of its batch are set up (see `super::spawn::batch`).
        let what = spec.command.first().unwrap_or(&env.shell).clone();
        let launch =
            super::spawn::prepare(&*pair.master, pair.slave, cmd).with_context(|| format!("starting {what:?}"))?;
        let io = start_io(id, &*pair.master, tx)?;

        let events = Rc::new(RefCell::new(Vec::new()));
        let mut pane = TermPane {
            term: emulator(env.scrollback, Size { cols, rows }, &events),
            archive: Archive::new(env.scrollback, usize::from(rows)),
            events,
            parser: Processor::new(),
            cut: Vec::new(),
            master: pair.master,
            input: io.input,
            input_pending: io.input_pending,
            direct: io.direct,
            input_dropped: false,
            owed: VecDeque::new(),
            output: io.output,
            process: Arc::new(Process { token, pid: Default::default(), child: Default::default() }),
            reader_stop: io.stop,
            writer_stop: io.writer_stop,
            pending: Some((
                launch,
                Waiter {
                    id,
                    tx: tx.clone(),
                    eof: io.eof,
                    what,
                    login_lock: spec.login.as_ref().and_then(|l| l.lock.clone()),
                },
            )),
            here: Arc::new(()),
            osc: OscScanner::default(),
            guard: KeyboardStackGuard::default(),
            commands: Commands::new(env.scrollback),
            graphics: crate::graphics::PaneGraphics::new(env.graphics.clone()),
            screen: Default::default(),
            changes: 0,
            moved: 0,
            cwd_seen: Default::default(),
            scrollback: env.scrollback,
        };
        if let Some(text) = &spec.startup_input {
            pane.write(text.clone().into_bytes());
        }
        if !super::spawn::batching()
            && let Some((launch, waiting)) = pane.pending.take()
        {
            watch(&pane.process, waiting, launch.run())?;
        }
        Ok(pane)
    }

    /// Make the emulator anew, at `size`, when the pane's program has not started and nothing
    /// is written on it: whether it was. That is a pane taking its place in a layout. Resized,
    /// an emulator keeps the cells it had: those of the provisional size the pane was made with
    /// (`Server::spawn_pane`), often several times what its place shows, for as long as it lives.
    pub(super) fn renew(&mut self, size: Size) -> bool {
        let unwritten = self.pending.is_some() && self.changes == 0;
        if unwritten {
            self.term = emulator(self.scrollback, size, &self.events);
        }
        unwritten
    }

    /// The process id of the pane's program, once it started.
    pub fn pid(&self) -> Option<u32> {
        self.process.pid()
    }

    /// The program, set up in a batch and not started yet, to start elsewhere.
    pub(super) fn take_pending(&mut self) -> Option<Pending> {
        let (launch, waiting) = self.pending.take()?;
        Some(Pending { launch, process: self.process.clone(), waiting, pane: Arc::downgrade(&self.here) })
    }

    /// Hang up, as closing a terminal does: SIGHUP to the program, its foreground job, and the
    /// rest of its session.
    pub(super) fn hang_up(&self) {
        self.process.signal_group(libc::SIGHUP);
        self.process.hang_up_session();
        #[cfg(unix)]
        if let Some(fg) = self.master.process_group_leader()
            && Some(fg as u32) != self.pid()
        {
            // SAFETY: plain syscall to the terminal's current foreground group.
            unsafe {
                libc::kill(-fg, libc::SIGHUP);
            }
        }
    }

    /// Send `bytes` to the program: written at once as far as the PTY takes them, when nothing
    /// waits for the writer thread, which writes the rest (in order) as the program reads.
    pub(super) fn write(&mut self, mut bytes: Vec<u8>) {
        if bytes.len() > self.input_room() {
            self.input_dropped = true;
            return;
        }
        if self.input_pending.load(Ordering::Acquire) == 0
            && self.direct
            && let Some(fd) = self.master.as_raw_fd()
        {
            let done = write_now(fd, &bytes);
            if done == bytes.len() {
                return;
            }
            bytes.drain(..done);
        }
        self.input_pending.fetch_add(bytes.len(), Ordering::AcqRel);
        let _ = self.input.send(bytes);
    }

    /// The most input the program can be sent at once now: a larger write is dropped whole.
    pub(super) fn input_room(&self) -> usize {
        MAX_PENDING_INPUT.saturating_sub(self.input_pending.load(Ordering::Relaxed))
    }

    /// Send the program a reply, after those it is owed already.
    pub(super) fn reply(&mut self, reply: Vec<u8>) {
        if self.owed.is_empty() {
            self.write(reply);
        } else {
            self.owed.push_back(Some(reply));
        }
    }

    /// An image being prepared owes the program a reply: the replies that follow wait for it.
    pub(super) fn owe_reply(&mut self) {
        self.owed.push_back(None);
    }

    /// The reply owed first is given (none when the program is not to hear of the outcome): it
    /// goes, then those it held back, up to the next one owed.
    pub(super) fn pay_reply(&mut self, reply: Option<Vec<u8>>) {
        self.owed.pop_front();
        let held = self.owed.iter().take_while(|r| r.is_some()).count();
        let ready: Vec<Vec<u8>> = reply.into_iter().chain(self.owed.drain(..held).flatten()).collect();
        for reply in ready {
            self.write(reply);
        }
    }

    /// Answer what the program asked the emulator so far (its attributes, the cursor, colours,
    /// the window's size): replies to what it asks next go after these. Its other events are
    /// left for the pane.
    pub(super) fn answer_queries(&mut self) {
        let events: Vec<TermEvent> = self.events.borrow_mut().drain(..).collect();
        let mut others = Vec::new();
        for event in events {
            let reply = match event {
                TermEvent::PtyWrite(s) => self.graphics.device_attributes(&s),
                TermEvent::ColorRequest(index, format) => {
                    format(self.term.colors()[index].unwrap_or_else(|| default_color(index)))
                }
                TermEvent::TextAreaSizeRequest(format) => {
                    let (cell_width, cell_height) = self.graphics.shared().cell();
                    let (num_lines, num_cols) = (self.term.screen_lines() as u16, self.term.columns() as u16);
                    format(WindowSize { num_lines, num_cols, cell_width, cell_height })
                }
                other => {
                    others.push(other);
                    continue;
                }
            };
            self.reply(reply.into_bytes());
        }
        self.events.borrow_mut().extend(others);
    }

    /// The grid point under a cell of the pane, clamped to the grid: the pane may be drawn
    /// larger than its emulator for a moment (before a relayout reaches it).
    pub(super) fn viewport_point(&self, col: u16, row: u16) -> Point {
        let offset = self.term.grid().display_offset() as i32;
        let cols = self.term.columns();
        let row = (row as usize).min(self.term.screen_lines().saturating_sub(1)) as i32;
        Point::new(Line(row - offset), Column((col as usize).min(cols.saturating_sub(1))))
    }
}

/// Where a pane's program starts, and whether it must start nowhere else (`strict`): its
/// directory as the kernel resolves it (relative to the server's, symlinks before `..`), resolved
/// once so that portable-pty and the `cd` of [`command_in`] cannot each read it their own way. A
/// plain shell whose directory is gone starts at home.
fn spawn_dir(cwd: Option<&Path>, strict: bool) -> Result<(PathBuf, bool)> {
    match (cwd.and_then(|d| std::fs::canonicalize(d).ok()).filter(|d| d.is_dir()), cwd) {
        (Some(dir), _) => Ok((dir, strict)),
        (None, Some(dir)) if strict => anyhow::bail!("{} is not a directory it can run in", dir.display()),
        _ => Ok((dirs::home_dir().unwrap_or_else(|| PathBuf::from("/")), false)),
    }
}

/// The command line that runs `program` in `dir` (absolute and physical). With `strict`, the
/// program runs there or not at all: portable-pty starts it in HOME instead when the directory
/// is gone by the time it looks, so the child changes directory itself, and gives up (126)
/// without running the program if it cannot.
fn command_in(program: &[&OsStr], dir: &Path, strict: bool) -> CommandBuilder {
    let mut cmd = if strict {
        let mut c = CommandBuilder::new("/bin/sh");
        c.args(["-c", "cd -P -- \"$1\" || exit 126; shift; exec \"$@\"", "kraal"]);
        c.arg(dir);
        c.args(program);
        c
    } else {
        let mut c = CommandBuilder::new(program[0]);
        c.args(&program[1..]);
        c
    };
    cmd.cwd(dir);
    cmd
}

/// Whether `program` can be started in `dir` with this PATH, looked for as portable-pty does: a
/// path relative to `dir`, else a name on PATH.
pub(super) fn findable(program: &OsStr, dir: &Path, path: Option<&OsStr>) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let executable = |p: &Path| {
        let Ok(c) = std::ffi::CString::new(p.as_os_str().as_bytes()) else { return false };
        // SAFETY: access only reads the NUL-terminated path.
        !p.is_dir() && unsafe { libc::access(c.as_ptr(), libc::X_OK) } == 0
    };
    if program.as_bytes().contains(&b'/') {
        return executable(&dir.join(program));
    }
    path.is_some_and(|path| std::env::split_paths(path).any(|d| executable(&dir.join(d).join(program))))
}

/// Output read from a PTY and not parsed yet. The reader adds each read to it; the server takes
/// all of it at once. Bounded: a program printing faster than kraal parses is slowed down
/// here, alone, without delaying other panes.
#[derive(Default)]
pub(super) struct Output {
    waiting: Mutex<Waiting>,
    /// The server took what was waiting: a reader waiting for room goes on.
    taken: std::sync::Condvar,
}

#[derive(Default)]
struct Waiting {
    bytes: Vec<u8>,
    /// The server was told there is output, and has not found it all taken since: it comes
    /// back for more without being told again.
    told: bool,
    /// The reader saw the end of the output: nothing comes after `bytes`.
    ended: bool,
}

impl Waiting {
    /// Whether the server must be told of what changed, which counts it told.
    fn tell(&mut self) -> bool {
        !std::mem::replace(&mut self.told, true)
    }
}

/// Output waiting beyond which the reader stops reading (and the program waits on its writes).
const MAX_WAITING: usize = 64 * 1024;

impl Output {
    fn lock(&self) -> std::sync::MutexGuard<'_, Waiting> {
        self.waiting.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Swap what is waiting into `into` (its own bytes are dropped, its room kept for the
    /// reader); false when nothing was waiting. The server is then told of the next output,
    /// unless it `comes_back` for it on its own.
    pub(super) fn take(&self, into: &mut Vec<u8>, comes_back: bool) -> bool {
        into.clear();
        let mut w = self.lock();
        if w.bytes.is_empty() {
            w.told &= comes_back;
            return false;
        }
        std::mem::swap(&mut w.bytes, into);
        drop(w);
        self.taken.notify_one();
        true
    }

    pub(super) fn is_empty(&self) -> bool {
        self.lock().bytes.is_empty()
    }

    /// Whether all the output was taken: the reader saw its end, and nothing waits.
    pub(super) fn finished(&self) -> bool {
        let w = self.lock();
        w.ended && w.bytes.is_empty()
    }

    /// Add a read once there is room for it (None when told to stop meanwhile): whether the
    /// server must be told, which counts it told.
    fn add(&self, read: &[u8], stop: &AtomicBool) -> Option<bool> {
        let mut w = self.lock();
        while w.bytes.len() >= MAX_WAITING {
            if stop.load(Ordering::Acquire) {
                return None;
            }
            w = self.taken.wait_timeout(w, Duration::from_millis(200)).ok()?.0;
        }
        w.bytes.extend_from_slice(read);
        Some(w.tell())
    }

    /// The reader saw the end of the output: whether the server must be told.
    fn end(&self) -> bool {
        let mut w = self.lock();
        w.ended = true;
        w.tell()
    }

    /// Telling the server failed: the next read tries again.
    fn untell(&self) {
        self.lock().told = false;
    }
}

/// Read a PTY until its end or until told to stop, adding its output to `out`, and telling the
/// server (`tell`) of output it does not know of yet, and of the end: it may wait for it to act
/// on the program's exit.
///
/// Output is read as soon as the PTY has some, however much already waits for the server: a line
/// or two at a time from a program that prints without a pause. Those wake-ups are most of what
/// kraal spends on such a program, and they are not to be saved: a reader that lets output gather
/// in the PTY (a pause between reads, or reading less far ahead of a server that is behind) wakes
/// 2 to 15 times less often, and the program then pays more for each write than the reader saved.
/// Measured on Linux 7.2 after 0.3.0: the program and the kernel's workers spend 30 to 90% more
/// CPU per megabyte, one flooding pane prints 29% less, fifteen print 18 to 37% less, and sixteen
/// panes printing at once take 28 to 66% longer.
fn read_output(mut reader: PtyReader, out: &Output, stop: &AtomicBool, tell: impl Fn() -> bool) {
    let mut buf = vec![0u8; 64 * 1024];
    while let Some(n) = reader.read(&mut buf, stop) {
        match out.add(&buf[..n], stop) {
            None => return,
            Some(true) if !tell() => out.untell(),
            Some(_) => {}
        }
    }
    if out.end() && !tell() {
        out.untell();
    }
}

/// What the waiter thread of a pane is given once its program started.
struct Waiter {
    id: PaneId,
    tx: EventSender,
    /// Closed when the reader has seen the end of the output.
    eof: crossbeam_channel::Receiver<()>,
    /// The program, as errors name it.
    what: String,
    /// Closing the pane must not release its login until the program has actually ended.
    login_lock: Option<Arc<std::fs::File>>,
}

/// The program `started` (or not) is `process`'s: watch it end. A failure leaves nothing
/// running.
fn watch(process: &Arc<Process>, waiting: Waiter, started: Result<PaneChild>) -> Result<()> {
    let Waiter { id, tx, eof, what, login_lock } = waiting;
    process.started(started.with_context(|| format!("starting {what:?}"))?);
    let waited = process.clone();
    let started = crate::threads::named(format!("pane-{id}-wait")).spawn(move || {
        // Wait for the end without reaping: that is left to whoever drops the process last.
        let code = wait_exited(waited.pid());
        // Report the exit after the last output, so waits and logs see all of it. A program
        // left in the background can keep the terminal open: then do not wait forever.
        let _ = eof.recv_timeout(std::time::Duration::from_secs(2));
        drop(login_lock);
        let _ = tx.send(Event::PaneExited(id, waited.pid(), code));
    });
    if let Err(e) = started {
        // It is killed, and reaped when the process goes (by the reaper, once the kill has
        // taken effect).
        process.signal_group(libc::SIGKILL);
        return Err(e.into());
    }
    Ok(())
}

/// The program of a pane set up in a batch, to start on any thread (see [`launcher`]).
pub struct Pending {
    launch: Launch,
    process: Arc<Process>,
    waiting: Waiter,
    pane: std::sync::Weak<()>,
}

#[must_use]
pub enum Started {
    Cancelled,
    Running,
    Failed(PaneId, ProcessHold, String),
}

impl Pending {
    /// Start the program, unless its pane is gone, and watch it end. Return a failure to the
    /// caller: this may be the server itself, which cannot wait for room in its own queue.
    pub fn start(self) -> Started {
        let Pending { launch, process, waiting, pane } = self;
        if pane.strong_count() == 0 {
            return Started::Cancelled;
        }
        let id = waiting.id;
        match watch(&process, waiting, launch.run()) {
            Ok(()) => Started::Running,
            Err(e) => Started::Failed(id, ProcessHold(process), format!("{e:#}")),
        }
    }
}

/// Start the thread that starts the programs of the panes of a batch, one after the other in
/// the order they are given. Each start waits for its program to run: off the server's thread,
/// which shows the first prompts meanwhile. What is sent once the thread is gone (it could not
/// be started) is for the sender to start.
pub fn launcher() -> Sender<Pending> {
    let (tx, rx) = crossbeam_channel::unbounded::<Pending>();
    let _ = crate::threads::named("pane-launch").spawn(move || {
        for pending in rx {
            let tx = pending.waiting.tx.clone();
            if let Started::Failed(id, process, why) = pending.start() {
                let _ = tx.send(Event::PaneFailed(id, process, why));
            }
        }
    });
    tx
}

/// The threads that move bytes between a pane's PTY and kraal.
struct Io {
    input: Sender<Vec<u8>>,
    input_pending: Arc<AtomicUsize>,
    /// The PTY does not wait: input can be written to it from the server's thread.
    direct: bool,
    output: Arc<Output>,
    /// Closed when the reader has seen the end of the output.
    eof: crossbeam_channel::Receiver<()>,
    stop: ReaderStop,
    writer_stop: WriterStop,
}

/// Start the reader and the writer threads of a PTY. On Linux the PTY is made not to wait
/// (`O_NONBLOCK`): a key is then written from the server's thread without handing it to the
/// writer, which only writes what the PTY had no room for, and the reader waits in `poll`.
fn start_io(id: PaneId, master: &(dyn MasterPty + Send), tx: &EventSender) -> Result<Io> {
    let stop = ReaderStop::new(master.tty_name());
    #[cfg(target_os = "linux")]
    let direct = master.as_raw_fd().is_some_and(set_nonblocking);
    #[cfg(not(target_os = "linux"))]
    let direct = false;
    let reader = PtyReader::new(master, stop.wakes())?;
    let polled = if direct { Some(reader.duplicate()?) } else { None };
    let writer = master.take_writer()?;
    let ready_tx = tx.clone();
    let output = Arc::new(Output::default());
    let out = output.clone();
    let (eof_tx, eof_rx) = crossbeam_channel::bounded::<()>(0);
    let stopped = stop.flag.clone();
    crate::threads::named(format!("pane-{id}-read")).spawn(move || {
        let _eof = eof_tx;
        read_output(reader, &out, &stopped, || ready_tx.pane_ready(id));
    })?;
    let (input, writer_stop) = start_writer(id, writer, master.tty_name(), polled).inspect_err(|_| stop.stop())?;
    let input_pending = writer_stop.pending.clone();
    Ok(Io { input, input_pending, direct, output, eof: eof_rx, stop, writer_stop })
}

/// Start the thread that writes what is sent to `writer`, the PTY `tty` is the other end of;
/// `polled`, a copy of a PTY that does not wait, is where it waits for room.
fn start_writer(
    id: PaneId,
    mut writer: Box<dyn Write + Send>,
    tty: Option<PathBuf>,
    polled: Option<std::fs::File>,
) -> std::io::Result<(Sender<Vec<u8>>, WriterStop)> {
    let (in_tx, in_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
    let (gone_tx, gone) = crossbeam_channel::bounded::<()>(0);
    let stop = WriterStop { flag: Arc::default(), pending: Arc::default(), tty, gone };
    let (stopped, pending) = (stop.flag.clone(), stop.pending.clone());
    crate::threads::named(format!("pane-{id}-write")).spawn(move || {
        let _gone = gone_tx;
        for data in in_rx {
            // What waits once the pane went is no one's.
            if stopped.load(Ordering::Acquire) || write_waiting(&mut writer, polled.as_ref(), &stopped, &data).is_err()
            {
                break;
            }
            // Written: once nothing is left, the server writes again itself.
            pending.fetch_sub(data.len(), Ordering::AcqRel);
        }
        // Before it is said gone: letting go of it writes an end of input to the PTY, which
        // may wait for room too.
        drop(writer);
    })?;
    Ok((in_tx, stop))
}

/// Tells a pane's writer to stop, even while the program reads none of its input: a write
/// waiting for room would otherwise keep the PTY open for as long as the program lives (it may
/// outlive the pane, ignoring the hangup). The input waiting in the PTY is read from its other
/// end, and dropped, until the writer is gone.
pub(super) struct WriterStop {
    flag: Arc<AtomicBool>,
    /// Bytes sent to the writer and not written yet.
    pending: Arc<AtomicUsize>,
    /// The PTY's other end.
    tty: Option<PathBuf>,
    /// Closed once the writer is gone.
    gone: crossbeam_channel::Receiver<()>,
}

impl WriterStop {
    fn stop(&self) {
        self.flag.store(true, Ordering::Release);
        // One with nothing to write ends with its channel.
        if self.pending.load(Ordering::Relaxed) == 0 {
            return;
        }
        let Some(tty) = self.tty.clone() else { return };
        let gone = self.gone.clone();
        let _ = crate::threads::named("pane-input-drop").spawn(move || drop_input(&tty, &gone));
    }
}

/// Read and drop the input waiting at `tty`, a PTY's other end, until `gone` closes.
fn drop_input(tty: &Path, gone: &crossbeam_channel::Receiver<()>) {
    use std::os::unix::fs::OpenOptionsExt;
    let opened = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK).open(tty);
    let Ok(mut input) = opened else { return };
    let mut buf = [0u8; 4096];
    while gone.recv_timeout(Duration::from_millis(1)) == Err(crossbeam_channel::RecvTimeoutError::Timeout) {
        while matches!(input.read(&mut buf), Ok(n) if n > 0) {}
    }
}

/// Make descriptor `fd` (and its copies) not wait.
#[cfg(target_os = "linux")]
fn set_nonblocking(fd: std::os::fd::RawFd) -> bool {
    // SAFETY: plain calls on a descriptor the PTY owns.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        flags >= 0 && libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) == 0
    }
}

/// Write what descriptor `fd` takes of `data` without waiting: how much it took. An error is
/// left for the writer thread to meet with the rest.
fn write_now(fd: std::os::fd::RawFd, data: &[u8]) -> usize {
    let mut done = 0;
    while done < data.len() {
        let rest = &data[done..];
        // SAFETY: writes from a live slice to a descriptor the pane owns.
        let n = unsafe { libc::write(fd, rest.as_ptr().cast(), rest.len()) };
        match n {
            n if n > 0 => done += n as usize,
            n if n < 0 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted => {}
            _ => break,
        }
    }
    done
}

/// Write all of `data` to a PTY, waiting for room until told to stop (`stopped`): in `poll` on
/// `polled` (a copy of the PTY) when the PTY does not wait itself.
fn write_waiting(
    writer: &mut impl Write,
    polled: Option<&std::fs::File>,
    stopped: &AtomicBool,
    mut data: &[u8],
) -> std::io::Result<()> {
    use std::io::ErrorKind;
    while !data.is_empty() {
        match writer.write(data) {
            Ok(0) => return Err(ErrorKind::WriteZero.into()),
            Ok(n) => data = &data[n..],
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock && stopped.load(Ordering::Acquire) => return Err(e),
            Err(e) if e.kind() == ErrorKind::WouldBlock => match polled {
                Some(file) => {
                    poll_one(file, libc::POLLOUT, 200)?;
                }
                None => std::thread::sleep(Duration::from_millis(1)),
            },
            Err(e) => return Err(e),
        }
    }
    writer.flush()
}

/// Wait up to `wait_ms` (-1: as long as it takes) for `events` on `file`: whether they came.
fn poll_one(file: &std::fs::File, events: libc::c_short, wait_ms: i32) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;
    let mut fds = libc::pollfd { fd: file.as_raw_fd(), events, revents: 0 };
    // SAFETY: one valid pollfd.
    match unsafe { libc::poll(&mut fds, 1, wait_ms) } {
        n if n < 0 => {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted { Ok(false) } else { Err(e) }
        }
        n => Ok(n > 0),
    }
}

/// Tells a pane's reader to stop, even while the program is silent: a blocked reader would
/// otherwise keep the PTY open for as long as the program lives (it may outlive the pane,
/// ignoring the hangup). The reader waits in `read` (a `poll` before each read slows down a
/// program that prints many small pieces, like `seq`): a byte written to the PTY's other end
/// wakes it.
pub(super) struct ReaderStop {
    flag: Arc<AtomicBool>,
    /// The PTY's other end, where the byte goes.
    tty: Option<PathBuf>,
}

impl ReaderStop {
    fn new(tty: Option<PathBuf>) -> ReaderStop {
        ReaderStop { flag: Arc::new(AtomicBool::new(false)), tty }
    }

    pub(super) fn stop(&self) {
        use std::os::unix::fs::OpenOptionsExt;
        self.flag.store(true, Ordering::Release);
        let Some(tty) = &self.tty else { return };
        // Not as a controlling terminal, and without waiting: a full PTY wakes the reader anyway.
        let opened = std::fs::OpenOptions::new().write(true).custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK).open(tty);
        if let Ok(mut tty) = opened {
            use std::os::fd::AsRawFd;
            // Output stopped (XOFF) would hold the byte back.
            // SAFETY: plain call on a descriptor the File owns.
            unsafe { libc::tcflow(tty.as_raw_fd(), libc::TCOON) };
            let _ = tty.write(b"\0");
        }
    }

    /// Whether a byte can wake the reader: otherwise it checks for a stop now and then.
    fn wakes(&self) -> bool {
        self.tty.is_some()
    }
}

/// Reads a PTY's output, until its end or until told to stop.
struct PtyReader {
    file: std::fs::File,
    /// A stop wakes the reader (see [`ReaderStop`]): it can wait in `read`.
    woken: bool,
}

impl PtyReader {
    fn new(master: &(dyn MasterPty + Send), woken: bool) -> Result<PtyReader> {
        use std::os::fd::FromRawFd;
        let fd = master.as_raw_fd().context("the pty has no descriptor")?;
        // SAFETY: dup of a descriptor we own; the copy is owned by the File.
        let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
        if copy < 0 {
            return Err(std::io::Error::last_os_error()).context("duplicating the pty");
        }
        // SAFETY: `copy` is a fresh descriptor nothing else owns.
        Ok(PtyReader { file: unsafe { std::fs::File::from_raw_fd(copy) }, woken })
    }

    /// The next output, as much as the PTY has, waiting for it; None at the end or once told to
    /// stop (what woke the reader then is dropped).
    fn read(&mut self, buf: &mut [u8], stop: &AtomicBool) -> Option<usize> {
        loop {
            if stop.load(Ordering::Acquire) {
                return None;
            }
            if !self.woken && !self.ready(Duration::from_millis(200)).ok()? {
                continue;
            }
            match self.file.read(buf) {
                Ok(0) => return None,
                Ok(_) if stop.load(Ordering::Acquire) => return None,
                Ok(n) => return Some(n),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                // A PTY that does not wait (see `start_io`): the reader waits for it here, as
                // it would in `read`.
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if self.woken {
                        poll_one(&self.file, libc::POLLIN, -1).ok()?;
                    }
                }
                Err(_) => return None,
            }
        }
    }

    /// Whether output (or the end) came within `wait`.
    fn ready(&self, wait: Duration) -> std::io::Result<bool> {
        poll_one(&self.file, libc::POLLIN, wait.as_millis() as i32)
    }

    /// Another descriptor of the PTY, to wait on.
    fn duplicate(&self) -> Result<std::fs::File> {
        self.file.try_clone().context("duplicating the pty")
    }
}

/// Block until process `pid` has exited, without reaping it (that is the owner's), and return
/// its exit code as the child's own wait reports it: the status, or 1 when a signal ended it.
fn wait_exited(pid: Option<u32>) -> Option<i32> {
    #[cfg(unix)]
    if let Some(pid) = pid {
        loop {
            // SAFETY: waitid writes into the zeroed siginfo only.
            let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
            let r = unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, libc::WEXITED | libc::WNOWAIT) };
            if r == 0 {
                return exit_code(&info);
            }
            if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                return None;
            }
        }
    }
    None
}

#[cfg(unix)]
fn exit_code(info: &libc::siginfo_t) -> Option<i32> {
    match info.si_code {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        // SAFETY: the siginfo of a child's exit carries its status.
        libc::CLD_EXITED => Some(unsafe { info.si_status() }),
        libc::CLD_KILLED | libc::CLD_DUMPED => Some(1),
        _ => None,
    }
}

#[cfg(test)]
impl TermPane {
    pub(crate) fn publish_output(&self, id: PaneId, data: Option<&[u8]>, tx: &EventSender) {
        let tell = match data {
            Some(bytes) => self.output.add(bytes, &AtomicBool::new(false)).unwrap(),
            None => self.output.end(),
        };
        if tell {
            assert!(tx.pane_ready(id));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn the_reader_publishes_its_final_burst_and_empty_eof_through_overflow() {
        for data in [b"last-burst".as_slice(), b"".as_slice()] {
            let (read, mut write) = std::io::pipe().unwrap();
            let read = std::fs::File::from(std::os::fd::OwnedFd::from(read));
            let (tx, rx) = crate::server::event_channel(1);
            tx.wake();
            let out = Arc::new(Output::default());
            let output = out.clone();
            let sender = tx.clone();
            let (said, heard) = crossbeam_channel::unbounded();
            let worker = std::thread::spawn(move || {
                read_output(PtyReader { file: read, woken: true }, &output, &AtomicBool::new(false), || {
                    let published = sender.pane_ready(1);
                    said.send(()).unwrap();
                    published
                });
            });
            let mut write = if data.is_empty() {
                drop(write);
                None
            } else {
                write.write_all(data).unwrap();
                Some(write)
            };
            heard.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(out.lock().told, "overflow is a completed notification");
            assert!(matches!(rx.try_recv(), Ok(Event::Wake)));
            assert!(rx.reconcile(false).panes);
            let mut bytes = Vec::new();
            assert_eq!(out.take(&mut bytes, false), !data.is_empty());
            assert_eq!(bytes, data);
            assert!(!out.take(&mut bytes, false));
            if data.is_empty() {
                assert!(out.finished());
            }
            // Only after the final-burst assertion may this silent pipe end.
            drop(write.take());
            worker.join().unwrap();
            assert!(out.finished());
        }
    }

    #[test]
    fn overflow_releases_a_reader_waiting_for_output_space() {
        let (tx, rx) = crate::server::event_channel(1);
        tx.wake();
        let out = Arc::new(Output::default());
        assert_eq!(out.add(&vec![b'x'; MAX_WAITING], &AtomicBool::new(false)), Some(true));
        assert!(tx.pane_ready(1));
        let (entered, adding) = crossbeam_channel::bounded(0);
        let o = out.clone();
        let worker = std::thread::spawn(move || {
            entered.send(()).unwrap();
            assert!(o.add(b"last", &AtomicBool::new(false)).is_some());
            if o.end() {
                assert!(tx.pane_ready(1));
            }
        });
        adding.recv().unwrap();
        // No tick or other producer: taking the overflow token must make room.
        assert!(matches!(rx.try_recv(), Ok(Event::Wake)));
        assert!(rx.reconcile(false).panes);
        let mut bytes = Vec::new();
        assert!(out.take(&mut bytes, false));
        assert_eq!(bytes, vec![b'x'; MAX_WAITING]);
        worker.join().unwrap();
        assert!(out.take(&mut bytes, false));
        assert_eq!(bytes, b"last");
        assert!(out.finished());
        assert!(!out.take(&mut bytes, false));
    }

    #[test]
    fn a_reader_stops_while_the_program_is_silent() {
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize { rows: 5, cols: 20, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        // Woken by a byte from the other end, or (without its name) checking now and then.
        for tty in [pair.master.tty_name(), None] {
            let stop = ReaderStop::new(tty);
            let mut reader = PtyReader::new(&*pair.master, stop.wakes()).unwrap();
            let stopped = stop.flag.clone();
            let (done_tx, done_rx) = crossbeam_channel::bounded(1);
            std::thread::spawn(move || {
                let mut buf = [0u8; 64];
                let _ = done_tx.send(reader.read(&mut buf, &stopped));
            });
            std::thread::sleep(Duration::from_millis(50));
            assert!(done_rx.is_empty());
            stop.stop();
            assert_eq!(done_rx.recv_timeout(Duration::from_secs(2)), Ok(None));
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn a_reader_sleeps_while_the_program_is_silent() {
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize { rows: 5, cols: 20, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let mut writer = pair.master.take_writer().unwrap();
        let stop = ReaderStop::new(pair.master.tty_name());
        let mut reader = PtyReader::new(&*pair.master, stop.wakes()).unwrap();
        let stopped = stop.flag.clone();
        let (tid_tx, tid_rx) = crossbeam_channel::bounded(1);
        let (read_tx, read_rx) = crossbeam_channel::unbounded();
        let thread = std::thread::spawn(move || {
            crate::testing::block_signals();
            // SAFETY: gettid has no preconditions.
            let _ = tid_tx.send(unsafe { libc::gettid() });
            let mut buf = [0u8; 64];
            while let Some(n) = reader.read(&mut buf, &stopped) {
                let _ = read_tx.send(buf[..n].to_vec());
            }
        });
        let tid = tid_rx.recv().unwrap();
        let switches = crate::testing::thread_switches(tid);
        crate::testing::wait_asleep(tid);
        let before = switches();
        crate::testing::sleep_with_signals(tid, Duration::from_millis(700));
        assert_eq!(switches(), before, "the reader woke up while nothing happened");
        // Output wakes it at once (the slave echoes what is typed into the master).
        writer.write_all(b"x").unwrap();
        let got = read_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(got.contains(&b'x'));
        stop.stop();
        thread.join().unwrap();
        drop(pair.slave);
    }

    #[test]
    fn output_is_taken_in_batches_and_the_server_told_once_per_batch() {
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize { rows: 5, cols: 20, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let stop = ReaderStop::new(pair.master.tty_name());
        let reader = PtyReader::new(&*pair.master, stop.wakes()).unwrap();
        let out = Arc::new(Output::default());
        let told = Arc::new(AtomicUsize::new(0));
        let (o, t, f) = (out.clone(), told.clone(), stop.flag.clone());
        std::thread::spawn(move || {
            read_output(reader, &o, &f, || {
                t.fetch_add(1, Ordering::SeqCst);
                true
            })
        });
        let mut tty = std::fs::OpenOptions::new().write(true).open(pair.master.tty_name().unwrap()).unwrap();
        let wait_for = |n: usize| {
            let start = Instant::now();
            while told.load(Ordering::SeqCst) < n && start.elapsed() < Duration::from_secs(2) {
                std::thread::sleep(Duration::from_millis(5));
            }
            told.load(Ordering::SeqCst)
        };
        tty.write_all(b"one").unwrap();
        assert_eq!(wait_for(1), 1);
        // More output before the server came: it is not told again, and takes all of it.
        tty.write_all(b" two").unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(told.load(Ordering::SeqCst), 1);
        let mut got = Vec::new();
        assert!(out.take(&mut got, false));
        assert!(String::from_utf8_lossy(&got).ends_with("one two"), "{got:?}");
        // Found empty by a server that comes back on its own: it is not told.
        assert!(!out.take(&mut got, true) && got.is_empty());
        tty.write_all(b"three").unwrap();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(told.load(Ordering::SeqCst), 1);
        assert!(out.take(&mut got, true) && got == b"three");
        // Found empty otherwise: the next output is told again.
        assert!(!out.take(&mut got, false));
        tty.write_all(b"four").unwrap();
        assert_eq!(wait_for(2), 2);
        stop.stop();
    }

    /// The program prints more than waits for the server and ends while the server is busy: its
    /// exit is told before the reader, holding a read until there is room for it, saw the end.
    /// (On macOS a program that wrote to a terminal does not end before its output is read.)
    #[test]
    #[cfg(target_os = "linux")]
    fn the_output_of_a_program_that_ended_is_all_taken_before_it_is_finished() {
        let (tx, rx) = crate::server::event_channel(4096);
        let env = PaneEnv {
            socket: "/nonexistent".into(),
            session: "t".into(),
            shell: "sh".into(),
            scrollback: 100,
            graphics: Default::default(),
        };
        let printed = MAX_WAITING + 8192;
        let script = format!("head -c {} /dev/zero | tr '\\0' x; printf end", printed - 3);
        let spec = PaneSpec { command: vec!["sh".into(), "-c".into(), script], ..Default::default() };
        let pane = TermPane::spawn(1, &spec, 40, 5, &env, &tx).unwrap();
        let start = Instant::now();
        let limit = Duration::from_secs(10);
        while !matches!(rx.recv_timeout(limit), Ok(Event::PaneExited(..))) {
            assert!(start.elapsed() < limit, "the exit is told");
        }
        // Taking makes room: the reader adds what it held a moment later. Looked at as soon as
        // the server may, the output is not finished meanwhile.
        let (mut taken, mut read) = (Vec::new(), Vec::new());
        loop {
            let took = pane.output.take(&mut read, false);
            let finished = pane.output.finished();
            if took {
                taken.extend_from_slice(&read);
            }
            if finished {
                break;
            }
            assert!(start.elapsed() < limit, "the reader sees the end");
        }
        assert_eq!(taken.len(), printed);
        assert!(taken.ends_with(b"end"));
    }

    /// A program that ignores the hangup and reads none of its input, sent more than its
    /// terminal holds: once its pane goes, nothing keeps the PTY open.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_pane_that_goes_lets_go_of_a_pty_whose_program_reads_no_input() {
        use std::os::unix::fs::OpenOptionsExt;
        let (tx, _rx) = crate::server::event_channel(4096);
        let env = PaneEnv {
            socket: "/nonexistent".into(),
            session: "t".into(),
            shell: "sh".into(),
            scrollback: 100,
            graphics: Default::default(),
        };
        let script = "trap '' HUP; stty raw -echo; echo ready; exec sleep 30".to_string();
        let spec = PaneSpec { command: vec!["sh".into(), "-c".into(), script], ..Default::default() };
        let mut pane = TermPane::spawn(1, &spec, 20, 5, &env, &tx).unwrap();
        let start = Instant::now();
        let mut seen = Vec::new();
        let mut read = Vec::new();
        while !String::from_utf8_lossy(&seen).contains("ready") {
            if pane.output.take(&mut read, false) {
                seen.extend_from_slice(&read);
            }
            assert!(start.elapsed() < Duration::from_secs(5), "the program never started");
            std::thread::sleep(Duration::from_millis(10));
        }
        pane.write(vec![b'x'; 1 << 20]);
        std::thread::sleep(Duration::from_millis(100));
        assert!(pane.input_pending.load(Ordering::Relaxed) > 0, "the writer waits for room");
        let (tty, pid) = (pane.master.tty_name().unwrap(), pane.pid().unwrap());
        drop(pane);
        // Once the PTY is closed, its other end can be written to no more.
        let writable = || {
            let opened =
                std::fs::OpenOptions::new().write(true).custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK).open(&tty);
            opened.and_then(|mut end| end.write(b"x")).is_ok()
        };
        let start = Instant::now();
        while writable() && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let open = writable();
        // SAFETY: plain syscall to the program's group, which ignores the hangup.
        unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
        assert!(!open, "the PTY stays open");
    }

    #[test]
    fn waiting_for_the_end_leaves_the_reaping_to_the_owner() {
        let mut child = std::process::Command::new("sh").arg("-c").arg("exit 3").spawn().unwrap();
        let start = Instant::now();
        assert_eq!(wait_exited(Some(child.id())), Some(3));
        assert!(start.elapsed() < Duration::from_secs(5));
        // Not reaped yet: the pid is still the child's, and its owner gets the status.
        assert_eq!(child.wait().unwrap().code(), Some(3));
        let mut killed = std::process::Command::new("sleep").arg("10").spawn().unwrap();
        killed.kill().unwrap();
        assert_eq!(wait_exited(Some(killed.id())), Some(1));
        killed.wait().unwrap();
    }

    #[test]
    fn a_hold_keeps_an_ended_process_unreaped_after_its_pane_lets_it_go() {
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize { rows: 5, cols: 20, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        // The pane's process ends at once, leaving a process that ignores SIGTERM and SIGHUP in
        // its group (ignored before it starts: the hangup of the leader's end comes at once).
        let mut cmd = CommandBuilder::new("sh");
        cmd.args(["-c", "trap '' TERM HUP; sleep 30 & exit 0"]);
        let child = pair.slave.spawn_command(cmd).unwrap();
        let pid = child.process_id().unwrap();
        let process = Arc::new(Process::of(child));
        let hold = ProcessHold::new(&process);
        // SAFETY: signal 0 only checks that the pid is taken.
        let taken = || unsafe { libc::kill(pid as libc::pid_t, 0) == 0 };
        assert_eq!(wait_exited(Some(pid)), Some(0));
        assert!(hold.group_running(), "what it left runs on");
        // The pane lets it go: the hold keeps it, so its numbers stay its own.
        drop(process);
        assert!(taken());
        hold.signal(libc::SIGTERM);
        std::thread::sleep(Duration::from_millis(200));
        assert!(hold.group_running(), "SIGTERM is ignored");
        hold.signal(libc::SIGKILL);
        let start = Instant::now();
        while hold.group_running() && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!hold.group_running());
        assert!(taken(), "still not reaped while held");
        drop(hold);
        let start = Instant::now();
        while taken() && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!taken());
    }

    #[test]
    fn an_ended_pane_process_keeps_its_pid_until_the_pane_lets_it_go() {
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize { rows: 5, cols: 20, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let mut cmd = CommandBuilder::new("sh");
        cmd.args(["-c", "exit 0"]);
        let child = pair.slave.spawn_command(cmd).unwrap();
        let pid = child.process_id().unwrap();
        let process = Process::of(child);
        // SAFETY: signal 0 only checks that the pid is taken.
        let taken = || unsafe { libc::kill(pid as libc::pid_t, 0) == 0 };
        assert_eq!(wait_exited(Some(pid)), Some(0));
        // Ended but not reaped: the pid is still taken (by the zombie), so it is safe to signal.
        assert!(taken());
        process.signal_group(libc::SIGHUP);
        drop(process);
        // Let go: reaped.
        let start = Instant::now();
        while taken() && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!taken());
    }

    /// The exit status of a PTY child, or a failure that says what it was doing when it does not
    /// end within 10 seconds.
    fn wait_for(child: &mut Box<dyn portable_pty::Child + Send + Sync>, what: &str) -> portable_pty::ExitStatus {
        let start = Instant::now();
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return status;
            }
            if start.elapsed() > Duration::from_secs(10) {
                let pid = child.process_id().map(|p| p.to_string()).unwrap_or_default();
                let ps = std::process::Command::new("ps")
                    .args(["-o", "pid,ppid,pgid,sess,stat,wchan,command", "-p", &pid])
                    .output()
                    .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                    .unwrap_or_default();
                let _ = child.kill();
                panic!("{what}: still running after 10 s:\n{ps}");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_command_whose_directory_went_runs_nowhere() {
        // As if the directory went between kraal's check and portable-pty's, which then starts
        // the command in HOME.
        let home = std::env::temp_dir().join(format!("kraal-gone-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize { rows: 5, cols: 20, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        // What the commands print is read, as kraal does: on macOS a process that wrote to a
        // terminal does not end before its output is read.
        let mut reader = pair.master.try_clone_reader().unwrap();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            while matches!(std::io::Read::read(&mut reader, &mut buf), Ok(n) if n > 0) {}
        });
        let program = [OsStr::new("sh"), OsStr::new("-c"), OsStr::new("touch ran-here")];
        let mut cmd = command_in(&program, &home.join("gone"), true);
        cmd.env("HOME", &home);
        let mut child = pair.slave.spawn_command(cmd).unwrap();
        assert_eq!(wait_for(&mut child, "in a gone directory").exit_code(), 126);
        assert!(!home.join("ran-here").exists(), "the command ran in HOME");
        // Where the directory is, the command runs there. In a terminal of its own: macOS does not
        // hand out one whose first program ended.
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize { rows: 5, cols: 20, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let mut reader = pair.master.try_clone_reader().unwrap();
        std::thread::spawn(move || {
            let mut buf = [0u8; 1024];
            while matches!(std::io::Read::read(&mut reader, &mut buf), Ok(n) if n > 0) {}
        });
        let mut child = pair.slave.spawn_command(command_in(&program, &home, true)).unwrap();
        assert!(wait_for(&mut child, "in its directory").success());
        assert!(home.join("ran-here").exists());
        let _ = std::fs::remove_dir_all(&home);
        assert!(!findable(OsStr::new("no-such-program-kraal"), &home, std::env::var_os("PATH").as_deref()));
        assert!(findable(OsStr::new("sh"), &home, std::env::var_os("PATH").as_deref()));
    }

    #[test]
    fn a_directory_is_resolved_once_as_the_kernel_would() {
        let base = std::env::temp_dir().join(format!("kraal-dirs-{}", std::process::id()));
        let real = base.join("real/inner");
        std::fs::create_dir_all(&real).unwrap();
        let link = base.join("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&real, &link).unwrap();
        // `link/..` is `real`, the parent of where the link points, not `base`.
        let (dir, strict) = spawn_dir(Some(&link.join("..")), true).unwrap();
        assert_eq!(dir, base.join("real").canonicalize().unwrap());
        assert!(strict);
        // A relative directory is the server's directory joined once.
        let (dir, _) = spawn_dir(Some(Path::new(".")), true).unwrap();
        assert_eq!(dir, std::env::current_dir().unwrap().canonicalize().unwrap());
        assert!(spawn_dir(Some(&base.join("gone")), true).is_err());
        let (_, strict) = spawn_dir(Some(&base.join("gone")), false).unwrap();
        assert!(!strict, "a plain shell starts at home");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn closing_hangs_up_background_jobs_in_groups_of_their_own() {
        let dir = std::env::temp_dir().join(format!("kraal-jobs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("job");
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize { rows: 5, cols: 20, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let mut cmd = CommandBuilder::new("sh");
        // Job control: the background job gets a process group of its own.
        cmd.args(["-c", &format!("set -m; sleep 30 & echo $! > {}; wait", pidfile.display())]);
        let child = pair.slave.spawn_command(cmd).unwrap();
        let pid = child.process_id().unwrap();
        let process = Process::of(child);
        let start = Instant::now();
        let job: libc::pid_t = loop {
            if let Some(j) = crate::testing::read_complete_lines(&pidfile).and_then(|s| s.trim().parse().ok()) {
                break j;
            }
            assert!(start.elapsed() < Duration::from_secs(5), "the job never started");
            std::thread::sleep(Duration::from_millis(20));
        };
        // SAFETY: plain syscall.
        assert_ne!(unsafe { libc::getpgid(job) }, pid as libc::pid_t);
        // The pane's group alone: the job is not in it, and lives on.
        process.signal_group(libc::SIGHUP);
        std::thread::sleep(Duration::from_millis(300));
        assert!(crate::process::running(job));
        // The session: the job goes too.
        process.hang_up_session();
        let start = Instant::now();
        while crate::process::running(job) && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!crate::process::running(job));
        drop(process);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_program_let_go_while_it_runs_is_reaped_when_it_ends() {
        start_reaper().unwrap();
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize { rows: 5, cols: 20, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let mut cmd = CommandBuilder::new("sh");
        cmd.args(["-c", "sleep 0.3"]);
        let child = pair.slave.spawn_command(cmd).unwrap();
        let pid = child.process_id().unwrap();
        // Still running when its pane goes (a failed start, or a pane closed under it).
        drop(Process::of(child));
        // SAFETY: signal 0 only checks that the pid is taken (a zombie takes it too).
        let taken = || unsafe { libc::kill(pid as libc::pid_t, 0) == 0 };
        assert!(taken());
        let start = Instant::now();
        while taken() && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!taken(), "ended, and not left a zombie");
    }

    #[test]
    fn input_a_program_does_not_read_yet_waits_for_it_in_order() {
        let dir = std::env::temp_dir().join(format!("kraal-input-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let got = dir.join("got");
        let (tx, _rx) = crate::server::event_channel(4096);
        let env = PaneEnv {
            socket: "/nonexistent".into(),
            session: "t".into(),
            shell: "sh".into(),
            scrollback: 100,
            graphics: Default::default(),
        };
        const N: usize = 300_000;
        // Raw, so that the terminal passes each byte on as it is.
        let script =
            format!("stty raw -echo; echo ready; sleep 0.5; head -c {} > '{}'; echo done", N + 3, got.display());
        let spec = PaneSpec { command: vec!["sh".into(), "-c".into(), script], ..Default::default() };
        let mut pane = TermPane::spawn(1, &spec, 20, 5, &env, &tx).unwrap();
        let mut seen = Vec::new();
        let mut wait_for = |pane: &TermPane, what: &str| {
            let start = Instant::now();
            let mut buf = Vec::new();
            while !String::from_utf8_lossy(&seen).contains(what) {
                assert!(start.elapsed() < Duration::from_secs(10), "{what}: {:?}", String::from_utf8_lossy(&seen));
                if pane.output.take(&mut buf, false) {
                    seen.extend_from_slice(&buf);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        wait_for(&pane, "ready");
        let data: Vec<u8> = (0..N).map(|i| b'a' + (i % 26) as u8).collect();
        let start = Instant::now();
        pane.write(data.clone());
        pane.write(b"END".to_vec());
        // Far more than the PTY holds: the rest waits for the writer thread, not the server.
        assert!(start.elapsed() < Duration::from_millis(200), "{:?}", start.elapsed());
        let pending = pane.input_pending.load(Ordering::Acquire);
        assert!(pending > 0);
        #[cfg(target_os = "linux")]
        assert!(pending < N + 3, "what the PTY had room for was written at once");
        wait_for(&pane, "done");
        let mut want = data;
        want.extend_from_slice(b"END");
        let written = std::fs::read(&got).unwrap();
        assert_eq!(written.len(), want.len());
        assert!(written == want, "every byte, in order");
        assert_eq!(pane.input_pending.load(Ordering::Acquire), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_program_held_in_a_batch_starts_later_if_its_pane_is_still_there() {
        let dir = std::env::temp_dir().join(format!("kraal-held-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        crate::testing::write_script(&dir.join("broken"), "#!/no/such/interpreter\n");
        let (tx, rx) = crate::server::event_channel(4096);
        let env = PaneEnv {
            socket: "/nonexistent".into(),
            session: "t".into(),
            shell: "sh".into(),
            scrollback: 100,
            graphics: Default::default(),
        };
        let spec = |name: &str| PaneSpec {
            command: vec!["sh".into(), "-c".into(), format!("touch '{}'; sleep 30", dir.join(name).display())],
            ..Default::default()
        };
        let batch = super::super::spawn::batch();
        let mut kept = TermPane::spawn(1, &spec("kept"), 20, 5, &env, &tx).unwrap();
        let mut gone = TermPane::spawn(2, &spec("gone"), 20, 5, &env, &tx).unwrap();
        let broken = PaneSpec { command: vec![dir.join("broken").display().to_string()], ..Default::default() };
        let mut failed = TermPane::spawn(3, &broken, 20, 5, &env, &tx).unwrap();
        drop(batch);
        assert_eq!(kept.pid(), None, "held while the batch was");
        let (k, g, f) = (kept.take_pending().unwrap(), gone.take_pending().unwrap(), failed.take_pending().unwrap());
        assert!(kept.take_pending().is_none(), "started once");
        drop(gone);
        // On another thread, as the launcher starts them.
        std::thread::spawn(move || {
            assert!(matches!(g.start(), Started::Cancelled));
            assert!(matches!(k.start(), Started::Running));
            let tx = f.waiting.tx.clone();
            let Started::Failed(id, process, why) = f.start() else { panic!("the start failed") };
            assert!(tx.send(Event::PaneFailed(id, process, why)).is_ok());
        })
        .join()
        .unwrap();
        assert!(kept.pid().is_some());
        let start = Instant::now();
        while !dir.join("kept").exists() && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(dir.join("kept").exists(), "started");
        assert!(!dir.join("gone").exists(), "not started: its pane went first");
        let (process, why) = loop {
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Event::PaneFailed(id, process, why)) => {
                    assert_eq!(id, 3);
                    break (process, why);
                }
                Ok(_) => {}
                Err(_) => panic!("the failure is said"),
            }
        };
        assert!(process.is(&ProcessHold::new(&failed.process)), "said of its process");
        assert!(why.contains("No such file or directory"), "{why}");
        assert_eq!(failed.pid(), None);
        kept.process.signal_group(libc::SIGKILL);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
