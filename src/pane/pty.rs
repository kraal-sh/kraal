//! The process side of a terminal pane: its PTY, the threads that read and write it, and the
//! emulator its output feeds.

use std::cell::RefCell;
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::vte::ansi::Processor;
use anyhow::{Context, Result};
use crossbeam_channel::Sender;
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize};

use super::commands::Commands;
use super::guard::KeyboardStackGuard;
use super::*;
use crate::layout::PaneId;
use crate::server::Event;

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
pub(super) struct Process {
    pub(super) pid: Option<u32>,
    child: Mutex<Option<PaneChild>>,
}

impl Process {
    /// Send `signal` to the process and its group (the pane's session leads one, with what it
    /// started in the foreground).
    pub(super) fn signal_group(&self, signal: i32) {
        let Some(pid) = self.pid else { return };
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
        let Some(pid) = self.pid else { return };
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
        self.0.pid.is_some_and(|pid| crate::process::group_running(pid as libc::pid_t))
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

type PaneChild = Box<dyn Child + Send + Sync>;

static REAPER: std::sync::OnceLock<crossbeam_channel::Sender<PaneChild>> = std::sync::OnceLock::new();

/// Start, once, the thread that reaps the programs of closed panes as they end.
fn start_reaper() -> Result<()> {
    if REAPER.get().is_some() {
        return Ok(());
    }
    let (tx, rx) = crossbeam_channel::unbounded::<PaneChild>();
    std::thread::Builder::new().name("pane-reaper".into()).spawn(move || {
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
    pub(super) events: Rc<RefCell<Vec<TermEvent>>>,
    pub(super) parser: Processor,
    pub(super) master: Box<dyn MasterPty + Send>,
    pub(super) input: Sender<Vec<u8>>,
    /// Bytes sent to the PTY not written yet: a program that stops reading its input cannot
    /// make kraal hold an unbounded amount of it.
    pub(super) input_pending: Arc<AtomicUsize>,
    /// Input was dropped because of that, since the server last asked.
    pub input_dropped: bool,
    /// Output read from the PTY, waiting to be parsed. Small and bounded: a program printing
    /// faster than kraal parses is slowed down here, alone, without delaying other panes.
    pub(super) output: crossbeam_channel::Receiver<Vec<u8>>,
    pub(super) process: Arc<Process>,
    /// Tells the reader to stop, even if the program never writes again (it may outlive the
    /// pane, ignoring the hangup).
    reader_stop: Arc<AtomicBool>,
    pub pid: Option<u32>,
    pub(super) osc: OscScanner,
    pub(super) guard: KeyboardStackGuard,
    /// The shell's commands, as it marks them.
    pub(super) commands: Commands,
    pub graphics: crate::graphics::PaneGraphics,
}

/// A PTY of `cols × rows`, with its size in pixels from the outer terminal's cells: programs
/// that draw images size them from it.
pub(super) fn pty_size(cols: u16, rows: u16, graphics: &crate::graphics::Shared) -> PtySize {
    let (cw, ch) = graphics.cell();
    PtySize { rows, cols, pixel_width: cols.saturating_mul(cw), pixel_height: rows.saturating_mul(ch) }
}

impl Drop for TermPane {
    fn drop(&mut self) {
        self.reader_stop.store(true, Ordering::Relaxed);
    }
}

impl TermPane {
    pub(super) fn spawn(
        id: PaneId,
        spec: &PaneSpec,
        cols: u16,
        rows: u16,
        env: &PaneEnv,
        tx: &Sender<Event>,
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
        let child: PaneChild = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("starting {:?}", spec.command.first().unwrap_or(&env.shell)))?;
        drop(pair.slave);
        let pid = child.process_id();
        let process = Arc::new(Process { pid, child: Mutex::new(Some(child)) });
        // A failure from here on must not leave the program running: it is killed, and reaped
        // when `process` goes (by the reaper, once the kill has taken effect).
        let held = process.clone();
        let mut pane = TermPane::assemble(id, pair.master, process, true, (cols, rows), env, tx)
            .inspect_err(|_| held.signal_group(libc::SIGKILL))?;
        if let Some(text) = &spec.startup_input {
            pane.write(text.clone().into_bytes());
        }
        Ok(pane)
    }

    /// A pane on the PTY (`fd`) and the program (`pid`) a live restart took over: the kraal
    /// this one replaced kept them open and running over its exec. `watch`: wait for the
    /// program to end (it had not, as far as that kraal knew).
    pub(super) fn adopt(
        id: PaneId,
        fd: std::os::fd::OwnedFd,
        pid: u32,
        watch: bool,
        size: (u16, u16),
        env: &PaneEnv,
        tx: &Sender<Event>,
    ) -> Result<TermPane> {
        start_reaper().context("starting the thread that reaps panes' programs")?;
        let child: PaneChild = Box::new(AdoptedChild { pid });
        let process = Arc::new(Process { pid: Some(pid), child: Mutex::new(Some(child)) });
        TermPane::assemble(id, Box::new(AdoptedPty { fd }), process, watch, size, env, tx)
    }

    /// The threads that read and write the PTY, the one that waits for the program to end
    /// (`watch`), and the emulator.
    fn assemble(
        id: PaneId,
        master: Box<dyn MasterPty + Send>,
        process: Arc<Process>,
        watch: bool,
        (cols, rows): (u16, u16),
        env: &PaneEnv,
        tx: &Sender<Event>,
    ) -> Result<TermPane> {
        let io = start_io(id, &*master, tx)?;
        if watch {
            let wait_tx = tx.clone();
            let waited = process.clone();
            let eof_rx = io.eof;
            let started = std::thread::Builder::new().name(format!("pane-{id}-wait")).spawn(move || {
                // Wait for the end without reaping: that is left to whoever drops the process last.
                let code = wait_exited(waited.pid);
                // Report the exit after the last output, so waits and logs see all of it. A program
                // left in the background can keep the terminal open: then do not wait forever.
                let _ = eof_rx.recv_timeout(std::time::Duration::from_secs(2));
                let _ = wait_tx.send(Event::PaneExited(id, waited.pid, code));
            });
            if let Err(e) = started {
                io.stop.store(true, Ordering::Relaxed);
                return Err(e.into());
            }
        }

        let events = Rc::new(RefCell::new(Vec::new()));
        // Programs may enable the kitty keyboard protocol (Claude Code does, for Shift+Enter).
        let config = TermConfig { scrolling_history: env.scrollback, kitty_keyboard: true, ..TermConfig::default() };
        let term = Term::new(config, &Size { cols, rows }, Listener(events.clone()));
        Ok(TermPane {
            term,
            events,
            parser: Processor::new(),
            master,
            input: io.input,
            input_pending: io.input_pending,
            input_dropped: false,
            output: io.output,
            pid: process.pid,
            process,
            reader_stop: io.stop,
            osc: OscScanner::default(),
            guard: KeyboardStackGuard::default(),
            commands: Commands::new(env.scrollback),
            graphics: crate::graphics::PaneGraphics::new(env.graphics.clone()),
        })
    }

    /// Stop reading the PTY: what was read is still to be taken, and then the output channel
    /// says it ended (see `Pane::drain`). What the program writes next waits in the PTY.
    pub(super) fn stop_reading(&self) {
        self.reader_stop.store(true, Ordering::Relaxed);
    }

    /// Read the PTY again after [`TermPane::stop_reading`].
    pub(super) fn resume_reading(&mut self, id: PaneId, tx: &Sender<Event>) -> Result<()> {
        let (output, stop, _eof) = start_reader(id, &*self.master, tx)?;
        self.output = output;
        self.reader_stop = stop;
        Ok(())
    }

    /// The PTY's descriptor, which a live restart keeps open for the next kraal.
    pub(super) fn master_fd(&self) -> Option<std::os::fd::RawFd> {
        self.master.as_raw_fd()
    }

    /// Bytes of input not written to the PTY yet.
    pub(super) fn input_pending(&self) -> usize {
        self.input_pending.load(Ordering::Relaxed)
    }

    /// Hang up, as closing a terminal does: SIGHUP to the program, its foreground job, and the
    /// rest of its session.
    pub(super) fn hang_up(&self) {
        self.process.signal_group(libc::SIGHUP);
        self.process.hang_up_session();
        #[cfg(unix)]
        if let Some(fg) = self.master.process_group_leader()
            && Some(fg as u32) != self.pid
        {
            // SAFETY: plain syscall to the terminal's current foreground group.
            unsafe {
                libc::kill(-fg, libc::SIGHUP);
            }
        }
    }

    pub(super) fn write(&mut self, bytes: Vec<u8>) {
        if self.input_pending.load(Ordering::Relaxed) + bytes.len() > MAX_PENDING_INPUT {
            self.input_dropped = true;
            return;
        }
        self.input_pending.fetch_add(bytes.len(), Ordering::Relaxed);
        let _ = self.input.send(bytes);
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
fn findable(program: &OsStr, dir: &Path, path: Option<&OsStr>) -> bool {
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

/// The threads that move bytes between a pane's PTY and kraal.
struct Io {
    input: Sender<Vec<u8>>,
    input_pending: Arc<AtomicUsize>,
    output: crossbeam_channel::Receiver<Vec<u8>>,
    /// Closed when the reader has seen the end of the output.
    eof: crossbeam_channel::Receiver<()>,
    stop: Arc<AtomicBool>,
}

fn start_io(id: PaneId, master: &(dyn MasterPty + Send), tx: &Sender<Event>) -> Result<Io> {
    let mut writer = master.take_writer()?;
    let (out_rx, stop, eof_rx) = start_reader(id, master, tx)?;
    let (in_tx, in_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
    let input_pending = Arc::new(AtomicUsize::new(0));
    let pending = input_pending.clone();
    let started = std::thread::Builder::new().name(format!("pane-{id}-write")).spawn(move || {
        for data in in_rx {
            if writer.write_all(&data).and_then(|_| writer.flush()).is_err() {
                break;
            }
            pending.fetch_sub(data.len(), Ordering::Relaxed);
        }
    });
    if let Err(e) = started {
        stop.store(true, Ordering::Relaxed);
        return Err(e.into());
    }
    Ok(Io { input: in_tx, input_pending, output: out_rx, eof: eof_rx, stop })
}

/// The thread that reads a PTY's output: the channel it goes to, the flag that stops it, and
/// a channel closed when it has seen the end.
type Reading = (crossbeam_channel::Receiver<Vec<u8>>, Arc<AtomicBool>, crossbeam_channel::Receiver<()>);

fn start_reader(id: PaneId, master: &(dyn MasterPty + Send), tx: &Sender<Event>) -> Result<Reading> {
    let reader = PtyReader::new(master)?;
    let stop = Arc::new(AtomicBool::new(false));
    let ready_tx = tx.clone();
    let (out_tx, out_rx) = crossbeam_channel::bounded::<Vec<u8>>(4);
    let (eof_tx, eof_rx) = crossbeam_channel::bounded::<()>(0);
    let stopped = stop.clone();
    std::thread::Builder::new().name(format!("pane-{id}-read")).spawn(move || {
        let _eof = eof_tx;
        let mut reader = reader;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf, &stopped) {
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
    Ok((out_rx, stop, eof_rx))
}

/// The PTY of a program taken over by a live restart: the descriptor the kraal before kept
/// open over its exec.
struct AdoptedPty {
    fd: std::os::fd::OwnedFd,
}

impl AdoptedPty {
    fn dup(&self) -> Result<std::fs::File> {
        use std::os::fd::AsRawFd;
        // SAFETY: dup of a descriptor we own; the copy is owned by the File.
        let copy = unsafe { libc::fcntl(self.fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if copy < 0 {
            return Err(std::io::Error::last_os_error()).context("duplicating the pty");
        }
        // SAFETY: `copy` is a fresh descriptor nothing else owns.
        Ok(unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(copy) })
    }
}

impl MasterPty for AdoptedPty {
    fn resize(&self, size: PtySize) -> Result<()> {
        use std::os::fd::AsRawFd;
        let ws = libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: size.pixel_width,
            ws_ypixel: size.pixel_height,
        };
        // SAFETY: TIOCSWINSZ reads the winsize we pass.
        if unsafe { libc::ioctl(self.fd.as_raw_fd(), libc::TIOCSWINSZ as _, &ws) } != 0 {
            return Err(std::io::Error::last_os_error()).context("resizing the pty");
        }
        Ok(())
    }

    fn get_size(&self) -> Result<PtySize> {
        use std::os::fd::AsRawFd;
        // SAFETY: TIOCGWINSZ fills the zeroed winsize.
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        if unsafe { libc::ioctl(self.fd.as_raw_fd(), libc::TIOCGWINSZ as _, &mut ws) } != 0 {
            return Err(std::io::Error::last_os_error()).context("reading the pty's size");
        }
        Ok(PtySize { rows: ws.ws_row, cols: ws.ws_col, pixel_width: ws.ws_xpixel, pixel_height: ws.ws_ypixel })
    }

    fn try_clone_reader(&self) -> Result<Box<dyn Read + Send>> {
        Ok(Box::new(self.dup()?))
    }

    fn take_writer(&self) -> Result<Box<dyn Write + Send>> {
        Ok(Box::new(self.dup()?))
    }

    fn process_group_leader(&self) -> Option<libc::pid_t> {
        use std::os::fd::AsRawFd;
        // SAFETY: plain syscall on a descriptor we own.
        match unsafe { libc::tcgetpgrp(self.fd.as_raw_fd()) } {
            pid if pid > 0 => Some(pid),
            _ => None,
        }
    }

    fn as_raw_fd(&self) -> Option<std::os::fd::RawFd> {
        use std::os::fd::AsRawFd;
        Some(self.fd.as_raw_fd())
    }

    fn tty_name(&self) -> Option<PathBuf> {
        None
    }
}

/// A pane's program taken over by a live restart: still this process's child (exec keeps the
/// pid), known by its pid alone.
#[derive(Debug)]
struct AdoptedChild {
    pid: u32,
}

impl portable_pty::ChildKiller for AdoptedChild {
    fn kill(&mut self) -> std::io::Result<()> {
        // SAFETY: plain syscall; the pid is ours until this child is reaped.
        if unsafe { libc::kill(self.pid as libc::pid_t, libc::SIGHUP) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
        Box::new(AdoptedChild { pid: self.pid })
    }
}

impl Child for AdoptedChild {
    fn try_wait(&mut self) -> std::io::Result<Option<portable_pty::ExitStatus>> {
        reap(self.pid, libc::WNOHANG)
    }

    fn wait(&mut self) -> std::io::Result<portable_pty::ExitStatus> {
        loop {
            if let Some(status) = reap(self.pid, 0)? {
                return Ok(status);
            }
        }
    }

    fn process_id(&self) -> Option<u32> {
        Some(self.pid)
    }
}

/// Reap child `pid` (`waitpid` with `flags`): its status once it has ended.
fn reap(pid: u32, flags: libc::c_int) -> std::io::Result<Option<portable_pty::ExitStatus>> {
    use std::os::unix::process::ExitStatusExt;
    let mut status = 0;
    loop {
        // SAFETY: waitpid writes the status into `status`.
        let r = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, flags) };
        match r {
            0 => return Ok(None),
            r if r > 0 => return Ok(Some(std::process::ExitStatus::from_raw(status).into())),
            _ => {
                let e = std::io::Error::last_os_error();
                if e.kind() != std::io::ErrorKind::Interrupted {
                    return Err(e);
                }
            }
        }
    }
}

/// Reads a PTY's output, giving up as soon as it is told to stop, even while the program is
/// silent: a blocked `read` would otherwise keep the PTY open for as long as the program lives.
struct PtyReader {
    file: std::fs::File,
}

impl PtyReader {
    fn new(master: &(dyn MasterPty + Send)) -> Result<PtyReader> {
        use std::os::fd::FromRawFd;
        let fd = master.as_raw_fd().context("the pty has no descriptor")?;
        // SAFETY: dup of a descriptor we own; the copy is owned by the File.
        let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
        if copy < 0 {
            return Err(std::io::Error::last_os_error()).context("duplicating the pty");
        }
        // SAFETY: `copy` is a fresh descriptor nothing else owns.
        Ok(PtyReader { file: unsafe { std::fs::File::from_raw_fd(copy) } })
    }

    fn read(&mut self, buf: &mut [u8], stop: &AtomicBool) -> std::io::Result<usize> {
        use std::os::fd::AsRawFd;
        loop {
            if stop.load(Ordering::Relaxed) {
                return Ok(0);
            }
            let mut fds = libc::pollfd { fd: self.file.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one valid pollfd.
            match unsafe { libc::poll(&mut fds, 1, 200) } {
                0 => continue,
                n if n < 0 => {
                    let e = std::io::Error::last_os_error();
                    if e.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(e);
                }
                _ => return self.file.read(buf),
            }
        }
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
mod tests {
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn a_reader_stops_while_the_program_is_silent() {
        let pair = portable_pty::native_pty_system()
            .openpty(PtySize { rows: 5, cols: 20, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let mut reader = PtyReader::new(&*pair.master).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let (done_tx, done_rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            let _ = done_tx.send(reader.read(&mut buf, &stopped).map_err(|e| e.kind()));
        });
        std::thread::sleep(Duration::from_millis(50));
        stop.store(true, Ordering::Relaxed);
        assert_eq!(done_rx.recv_timeout(Duration::from_secs(2)), Ok(Ok(0)));
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
        let process = Arc::new(Process { pid: Some(pid), child: Mutex::new(Some(child)) });
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
        let process = Process { pid: Some(pid), child: Mutex::new(Some(child)) };
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
        let process = Process { pid: Some(pid), child: Mutex::new(Some(child)) };
        let start = Instant::now();
        let job: libc::pid_t = loop {
            if let Some(j) = std::fs::read_to_string(&pidfile).ok().and_then(|s| s.trim().parse().ok()) {
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
        drop(Process { pid: Some(pid), child: Mutex::new(Some(child)) });
        // SAFETY: signal 0 only checks that the pid is taken (a zombie takes it too).
        let taken = || unsafe { libc::kill(pid as libc::pid_t, 0) == 0 };
        assert!(taken());
        let start = Instant::now();
        while taken() && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!taken(), "ended, and not left a zombie");
    }
}
