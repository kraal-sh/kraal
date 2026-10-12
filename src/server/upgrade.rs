//! Live restart: the server replaces itself with another kraal binary, and no program in its
//! panes stops.
//!
//! `exec` keeps the process, so the panes' programs stay its children, and their PTYs stay open
//! when their descriptors are not closed on exec. The binary is copied once into a sealed
//! memory file and tried (it runs and reads this very hand-off format), off the event loop and
//! within a time limit, and that copy is run. Before the exec, the server stops taking
//! connections (they wait for the next server), answers the commands it has read and waits
//! until what they were sent is written; attached clients hold what is typed from a point
//! between two keys, for the pane focused, and say so (a terminal attached with an older kraal,
//! which cannot, holds the restart back), and none may be in a paste or a kraal menu. What a
//! client held goes, once it is back, to the pane it was typed for or nowhere, and it is told.
//! It waits for the commands its threads
//! run, and holds off new ones; it stops reading the PTYs (what was read is drawn first), and
//! goes on only when the output of every terminal still in use stands between two sequences and
//! its input has been taken. It writes a private hand-off file of its own: the session's state
//! with each pane's id, and for each terminal pane its PTY's descriptor, its program, and what
//! its emulator shows (text with its attributes, cursors, modes, keyboard stacks), read back
//! first to be sure the next server reads it. The new server reads it (and deletes it), keeps
//! listening on the same socket, rebuilds every pane on its PTY and program, and has full-screen
//! programs draw themselves again; clients come back once it answers.
//!
//! What cannot be carried over is refused, with the reason: a runbook run, a transfer, a file
//! open in the local editor, an account switch, a `kraal wait`; and what does not end within a
//! few seconds (a hook, background work, a command being answered, input a program does not
//! read, a sequence cut in two, a paste still coming). The server then goes on as before.
//! Tunnels keep their ssh; those that list remote sessions' agents are started again.
//!
//! After the exec, nothing is closed under a program, and nothing is started in its place: a
//! pane that cannot be taken over is left out of the layout alone, its PTY kept open and its
//! program running, and is taken over later into workspace "recovered". A hand-off that cannot
//! be read leaves the terminals it kept, and only them, as they are, in workspace "recovered":
//! under the pane ids their programs know, or read-only when unknown; the saved state stays as
//! it was. What became of each pane handed over is kept (`Outcome`), and `kraal restart` says
//! what was not taken over as it was.

use std::collections::HashMap;
use std::ffi::{CString, OsStr};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ratatui::layout::Rect;
use serde::{Deserialize, Serialize};

use super::events::{EventLog, SavedLog};
use super::net::{LiveTunnel, TunnelState};
use super::ui::{ConfirmAction, Mode};
use super::{ClientId, Event, Server};
use crate::layout::PaneId;
use crate::pane::{Drained, LivePane, Pane, Recovered};
use crate::persist::{self, SavedScrollback, SessionSnapshot};
use crate::protocol::{Response, ServerMsg};

/// The format of the hand-off file. A kraal takes over only from one whose format it reads.
pub const HANDOFF_VERSION: u32 = 3;

/// How long a live restart waits for what is in its way to end before it gives up.
const IDLE_WAIT: Duration = Duration::from_secs(5);
/// How long clients have to send what they are sending, and to say they hold what is typed.
const CLIENTS_WAIT: Duration = Duration::from_secs(2);
/// How long the hand-off may take to reach the disk.
const WRITE_WAIT: Duration = Duration::from_secs(5);
/// How long copying and trying the kraal to restart on may take.
const BINARY_WAIT: Duration = Duration::from_secs(15);

/// What a server hands over to the one that replaces it.
#[derive(Serialize, Deserialize)]
pub struct Handoff {
    version: u32,
    /// The version of the kraal that wrote it.
    from: String,
    session: String,
    /// The listening socket, kept open: a client connecting meanwhile waits for the new server.
    listener: RawFd,
    /// The binary run (a copy), kept open to be run: the new server closes it.
    binary: RawFd,
    /// Where that kraal is installed, which the copy cannot tell.
    installed: PathBuf,
    next_id: u32,
    layout_area: (u16, u16, u16, u16),
    sidebar_was_useful: bool,
    /// The layout, each pane named by its id.
    state: SessionSnapshot,
    panes: Vec<LivePane>,
    tunnels: Vec<LiveTunnel>,
    events: SavedLog,
    announced: Vec<PaneId>,
    pane_states: Vec<(PaneId, Option<String>, Option<String>)>,
    ports: Vec<(u16, PaneId)>,
    port_candidates: Vec<(u16, PaneId)>,
    /// Connections accepted and not handled yet.
    connections: Vec<RawFd>,
    /// Children no pane has any more (a closed pane's program that ignored the hangup), still to
    /// be reaped when they end.
    reaping: Vec<u32>,
    /// Live restarts since the session's server started, before this one.
    live_restarts: u32,
}

impl Handoff {
    /// The descriptors it names: they stay open over the exec.
    fn fds(&self) -> Vec<RawFd> {
        let mut fds = vec![self.listener, self.binary];
        fds.extend(self.panes.iter().map(|p| p.fd));
        fds.extend(self.tunnels.iter().filter_map(|t| t.stderr));
        fds.extend(&self.connections);
        fds
    }

    /// The listening socket it hands over, when it is one.
    pub fn listener(&self) -> Option<std::os::unix::net::UnixListener> {
        // SAFETY: fstat fills the zeroed stat.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let socket = unsafe { libc::fstat(self.listener, &mut st) } == 0 && st.st_mode & libc::S_IFMT == libc::S_IFSOCK;
        // SAFETY: the hand-off gives this process the descriptor, which nothing else owns.
        socket.then(|| unsafe { std::os::unix::net::UnixListener::from_raw_fd(self.listener) })
    }
}

/// A hand-off as the next server reads it: as deep as the layout is (each split nests a few
/// levels), which serde_json's default limit would refuse past a few dozen splits.
fn parse_handoff(data: &[u8]) -> serde_json::Result<Handoff> {
    let mut de = serde_json::Deserializer::from_slice(data);
    de.disable_recursion_limit();
    let h = Handoff::deserialize(&mut de)?;
    de.end()?;
    Ok(h)
}

/// What `kraal server --handoff-check` prints when it reads hand-off format `version`.
fn check_answer(version: u32) -> String {
    format!("kraal hand-off {version}")
}

/// `kraal server --handoff-check VERSION`: say whether this kraal can take over a server that
/// writes this format.
pub fn handoff_check(version: u32) -> Result<()> {
    if version != HANDOFF_VERSION {
        bail!("this kraal reads hand-off format {HANDOFF_VERSION}, not {version}");
    }
    println!("{}", check_answer(version));
    Ok(())
}

/// The kraal binary to restart on: `asked` (absolute), else the one installed where this server
/// was started from (`kraal update` may have renamed a new one over the one running).
fn binary_to_run(asked: Option<PathBuf>) -> Result<PathBuf, String> {
    let exe = match asked {
        Some(path) if path.is_absolute() => path,
        Some(path) => return Err(format!("{} is not an absolute path", path.display())),
        None => crate::client::installed_exe().map_err(|e| format!("{e:#}"))?,
    };
    if !exe.is_file() {
        return Err(format!("no kraal at {}", exe.display()));
    }
    Ok(exe)
}

/// The kraal binary a live restart runs: a sealed copy, made once, of the file installed. The
/// copy tried is the copy run, whatever is written to the file or renamed over it meanwhile.
struct Binary {
    /// Where it is installed: what the next server runs jobs from and restarts on.
    path: PathBuf,
    /// The copy (on Linux; elsewhere, where there is no live restart, the file itself).
    file: std::fs::File,
}

impl Binary {
    fn open(asked: Option<PathBuf>) -> Result<Binary, String> {
        let path = binary_to_run(asked)?;
        let file = snapshot(&path).map_err(|e| format!("cannot copy {}: {e}", path.display()))?;
        Ok(Binary { path, file })
    }

    fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

/// A copy of the file at `path` that cannot change: in memory, sealed against writes.
#[cfg(target_os = "linux")]
fn snapshot(path: &Path) -> std::io::Result<std::fs::File> {
    let name = c"kraal";
    let flags = libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING;
    // SAFETY: memfd_create takes a NUL-terminated name. A kernel that runs memfds only when
    // asked takes MFD_EXEC; an older one does not know it.
    let mut fd = unsafe { libc::memfd_create(name.as_ptr(), flags | libc::MFD_EXEC) };
    if fd < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL) {
        // SAFETY: as above.
        fd = unsafe { libc::memfd_create(name.as_ptr(), flags) };
    }
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a fresh descriptor nothing else owns.
    let mut copy = unsafe { std::fs::File::from_raw_fd(fd) };
    std::io::copy(&mut std::fs::File::open(path)?, &mut copy)?;
    let seals = libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE | libc::F_SEAL_SEAL;
    // SAFETY: fcntl on a descriptor we own.
    if unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, seals) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(copy)
}

#[cfg(not(target_os = "linux"))]
fn snapshot(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}

/// Whether `binary` runs here and takes over this hand-off's format: the very copy that is run,
/// through this process's descriptor of it.
fn check_binary(binary: &Binary) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    let fd = binary.fd();
    let mut cmd = std::process::Command::new(format!("/proc/self/fd/{fd}"));
    cmd.args(["server", "--handoff-check", &HANDOFF_VERSION.to_string()]);
    // SAFETY: fcntl is async-signal-safe. A script reads itself through the descriptor: it
    // stays open in the command, and only there.
    unsafe {
        cmd.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let out = crate::exec::run(&mut cmd, Duration::from_secs(10), 4096)
        .map_err(|e| format!("cannot run {}: {e}", binary.path.display()))?;
    if out.success() && out.stdout_text().trim() == check_answer(HANDOFF_VERSION) {
        return Ok(());
    }
    Err(format!("the kraal installed cannot take over live ({}: an older kraal, or not one)", binary.path.display()))
}

/// The kraal to restart on (see `Binary`), copied and tried within a while, off the event loop:
/// a file system that does not answer is given up on, and the copy goes whenever that work ends.
fn prepare_binary(asked: Option<PathBuf>) -> Result<Binary, String> {
    let work = move || Binary::open(asked).and_then(|exe| check_binary(&exe).map(|()| exe));
    match within(BINARY_WAIT, work, drop) {
        Some(prepared) => prepared,
        None => Err("the kraal to restart on does not answer (its file system may hang)".into()),
    }
}

/// A hand-off file of its own for each attempt: one given up on (its disk too slow) and written
/// late never takes the place of the next one's.
fn handoff_path(session: &str) -> PathBuf {
    static ATTEMPTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = ATTEMPTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let pid = std::process::id();
    crate::paths::state_dir().join(format!("handoff-{session}-{pid}-{}-{n}.json", crate::accounts::now()))
}

/// The largest hand-off the next server reads.
const MAX_HANDOFF: usize = 1 << 30;

/// The hand-off as it is written, once sure the next server reads it: it parses back, and is
/// not larger than it reads (`limit`).
fn encode_handoff(h: &Handoff, limit: usize) -> Result<Vec<u8>, String> {
    let data = serde_json::to_vec(h).map_err(|e| format!("cannot write down the session: {e}"))?;
    if data.len() > limit {
        return Err(format!("the session is too large to hand over ({} MiB)", data.len() >> 20));
    }
    parse_handoff(&data).map_err(|e| format!("the session cannot be handed over: {e}"))?;
    Ok(data)
}

/// Have descriptor `fd` stay open over exec (`inherited`), or be closed by it.
fn set_inherited(fd: RawFd, inherited: bool) -> std::io::Result<()> {
    // SAFETY: fcntl on a descriptor of ours.
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        let flags = if inherited { flags & !libc::FD_CLOEXEC } else { flags | libc::FD_CLOEXEC };
        if flags < 0 || libc::fcntl(fd, libc::F_SETFD, flags) < 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Replace this process with `binary` (through its descriptor: that very file), a server of
/// `session` that takes over from `handoff`. Returns only when that failed, with why.
fn exec(binary: &Binary, session: &str, handoff: &Path) -> std::io::Error {
    let arg0 = std::env::args_os().next().unwrap_or_else(|| binary.path.as_os_str().to_owned());
    let args: [&OsStr; 6] = [
        &arg0,
        OsStr::new("--handoff"),
        handoff.as_os_str(),
        OsStr::new("server"),
        OsStr::new("--session"),
        OsStr::new(session),
    ];
    let Ok(argv) = args.iter().map(|a| CString::new(a.as_bytes())).collect::<Result<Vec<_>, _>>() else {
        return std::io::Error::new(std::io::ErrorKind::InvalidInput, "a path holds a NUL byte");
    };
    let env: Vec<CString> = std::env::vars_os()
        .filter_map(|(k, v)| CString::new([k.as_bytes(), b"=", v.as_bytes()].concat()).ok())
        .collect();
    let mut argv_ptrs: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    argv_ptrs.push(std::ptr::null());
    let mut env_ptrs: Vec<*const libc::c_char> = env.iter().map(|a| a.as_ptr()).collect();
    env_ptrs.push(std::ptr::null());
    #[cfg(target_os = "linux")]
    {
        // SAFETY: NUL-terminated strings in NULL-terminated arrays, alive across the call;
        // fexecve returns only when it failed.
        unsafe { libc::fexecve(binary.fd(), argv_ptrs.as_ptr(), env_ptrs.as_ptr()) };
        std::io::Error::last_os_error()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (argv_ptrs, env_ptrs);
        std::io::Error::new(std::io::ErrorKind::Unsupported, "a live restart is only available on Linux")
    }
}

/// Run `work` on a thread of its own, waiting `limit` at most: what it gave, or `None` when it
/// took longer (then `abandoned` gets what it gives once done).
fn within<T: Send + 'static>(
    limit: Duration,
    work: impl FnOnce() -> T + Send + 'static,
    abandoned: impl FnOnce(T) + Send + 'static,
) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<T>(1);
    let given_up = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let late = given_up.clone();
    let spawned = std::thread::Builder::new().name("live-restart".into()).spawn(move || {
        let result = work();
        if late.load(std::sync::atomic::Ordering::SeqCst) {
            abandoned(result);
        } else if let Err(std::sync::mpsc::TrySendError::Full(r) | std::sync::mpsc::TrySendError::Disconnected(r)) =
            tx.try_send(result)
        {
            abandoned(r);
        }
    });
    if spawned.is_err() {
        return None;
    }
    let got = rx.recv_timeout(limit).ok();
    if got.is_none() {
        given_up.store(true, std::sync::atomic::Ordering::SeqCst);
        // Done just then: it is here after all.
        return rx.try_recv().ok();
    }
    got
}

/// Read the hand-off at `path` (deleted at once: it holds what the panes showed) for
/// `session`. The descriptors it names are not inherited by what this server starts.
pub fn read_handoff(path: &Path, session: &str) -> Result<Handoff> {
    let data = crate::paths::read_regular(path, MAX_HANDOFF as u64);
    let _ = std::fs::remove_file(path);
    let data = data.with_context(|| format!("reading {}", path.display()))?;
    #[cfg(debug_assertions)]
    if std::env::var_os("KRAAL_TEST_UNREADABLE_HANDOFF").is_some() {
        bail!("a test says it cannot be read");
    }
    let h = parse_handoff(&data).with_context(|| format!("parsing {}", path.display()))?;
    if h.version != HANDOFF_VERSION {
        bail!("hand-off format {} (this kraal reads {HANDOFF_VERSION})", h.version);
    }
    if h.session != session {
        bail!("the hand-off is session {:?}'s", h.session);
    }
    for fd in h.fds() {
        let _ = set_inherited(fd, false);
    }
    // SAFETY: the binary this runs from, open for the exec and of no use now.
    unsafe {
        libc::close(h.binary);
    }
    crate::client::set_installed(h.installed.clone());
    Ok(h)
}

/// What can be taken of a hand-off that cannot be read: the terminals it kept open, each with
/// the program leading its session and the pane it knows (its `KRAAL_PANE`, when it is this
/// session's), to keep as they are (see `Server::take_salvaged`). They are closed on exec from
/// now on, as everything this server starts must not get them. Every other descriptor it kept
/// open is closed (clients connect again, the socket is made again), and the other children are
/// reaped once they end.
pub fn salvage(socket: &Path) -> Vec<Salvaged> {
    let dir = if cfg!(target_os = "linux") { "/proc/self/fd" } else { "/dev/fd" };
    let fds: Vec<RawFd> = std::fs::read_dir(dir)
        .map(|d| d.flatten().filter_map(|e| e.file_name().to_str()?.parse().ok()).filter(|fd| *fd > 2).collect())
        .unwrap_or_default();
    let mut terminals: Vec<Salvaged> = Vec::new();
    for fd in fds {
        match salvage_terminal(fd) {
            Some(live) => {
                let _ = set_inherited(fd, false);
                let env = crate::process::environ(live.pid, &["KRAAL_PANE", "KRAAL_SOCKET"]);
                let ours = env.get("KRAAL_SOCKET").is_some_and(|s| Path::new(s) == socket);
                let pane = env.get("KRAAL_PANE").and_then(|p| p.parse().ok()).filter(|_| ours && live.pid > 0);
                terminals.push(Salvaged { live, pane });
            }
            // SAFETY: nothing in this process uses these yet (the directory's own is closed
            // already).
            None => unsafe {
                libc::close(fd);
            },
        }
    }
    let leaders: Vec<u32> = terminals.iter().map(|t| t.live.pid).collect();
    for pid in crate::process::children(std::process::id()) {
        if !leaders.contains(&pid) && crate::exec::is_child(pid) {
            crate::exec::reap_pid_later(pid);
        }
    }
    terminals
}

/// Descriptor `fd`, when it is the main side of a PTY, as a pane to take over (its id given
/// later).
#[cfg(not(target_os = "linux"))]
fn salvage_terminal(_fd: RawFd) -> Option<LivePane> {
    None
}

#[cfg(target_os = "linux")]
fn salvage_terminal(fd: RawFd) -> Option<LivePane> {
    // SAFETY: plain syscall on a descriptor of this process.
    if unsafe { libc::isatty(fd) } != 1 {
        return None;
    }
    let target = std::fs::read_link(format!("/proc/self/fd/{fd}")).ok()?;
    if !target.to_string_lossy().ends_with("ptmx") {
        return None;
    }
    let mut session: libc::pid_t = 0;
    // SAFETY: TIOCGSID writes the session of the terminal's other side; TIOCGWINSZ its size.
    let leader = match unsafe { libc::ioctl(fd, libc::TIOCGSID as _, &mut session) } {
        0 if session > 0 => session as u32,
        _ => 0,
    };
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let _ = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ as _, &mut ws) };
    // SAFETY: plain syscall.
    let group = if leader > 0 { unsafe { libc::getpgid(leader as libc::pid_t) } } else { -1 };
    Some(LivePane::salvaged(0, fd, leader, group, (ws.ws_col.max(2), ws.ws_row.max(1))))
}

type Reply = Option<Result<Response, String>>;

/// Who asked for a live restart, and is told when it does not happen.
#[derive(Clone, Copy)]
pub(super) enum Asker {
    /// `kraal restart`: a control request, answered then closed.
    Request(ClientId),
    /// The menu of an attached client.
    Menu(ClientId),
}

impl Asker {
    fn request(self) -> Option<ClientId> {
        match self {
            Asker::Request(c) => Some(c),
            Asker::Menu(_) => None,
        }
    }
}

/// Why a live restart did not happen.
enum NotNow {
    /// Something that ends by itself is in the way: it is tried again for a while.
    Busy(String),
    /// It cannot happen.
    Refused(String),
}

/// What is held while a live restart is prepared: connections not handled yet, which go to the
/// next server.
struct Held {
    connections: Vec<UnixStream>,
}

impl Server {
    /// `kraal restart` asks for a live restart on `binary` (the server's own kraal by default),
    /// or with `check` whether one can happen now. Answered when it does not happen, with why
    /// (or, with `check`, when it can); otherwise the connection closes with the exec.
    pub(super) fn upgrade_request(&mut self, client: ClientId, check: bool, binary: Option<PathBuf>) -> Reply {
        if self.upgrading {
            return Some(Err("a live restart is already under way".into()));
        }
        if let Some(why) = self.upgrade_refusal() {
            return Some(Err(why));
        }
        if check {
            let started = self.background(
                move || prepare_binary(binary).map(drop),
                move |server, result| {
                    server.send_msg(client, &ServerMsg::Response { result: result.map(|()| Response::Ok) });
                    server.clients.remove(&client);
                },
            );
            return (!started).then(|| Err("cannot start trying the binary".into()));
        }
        self.start_upgrade(Asker::Request(client), binary);
        None
    }

    /// The question before a restart from the menu: live when it can happen (nothing stops),
    /// else what stops. Whether it can is known once the kraal installed now has been tried, off
    /// the event loop: the question comes then.
    pub(super) fn ask_restart(&mut self, client: ClientId) -> Mode {
        if let Some(why) = self.upgrade_refusal() {
            return self.cold_restart_question(&why);
        }
        let started = self.background(
            || prepare_binary(None).map(drop),
            move |server, tried| {
                let mode = match tried {
                    Ok(()) => Mode::confirm(
                        format!(
                            "Restart session {} live on the kraal installed now? Every program in its panes keeps running.",
                            server.session
                        ),
                        ConfirmAction::Restart { live: true },
                    ),
                    Err(why) => server.cold_restart_question(&why),
                };
                server.set_mode_if_idle(client, mode);
            },
        );
        if !started {
            return self.cold_restart_question("the kraal installed now could not be tried");
        }
        Mode::Normal
    }

    /// A live restart on the kraal installed now, asked from the menu (see `ask_restart`).
    pub(super) fn upgrade_from_menu(&mut self, client: ClientId) {
        if self.upgrading {
            return self.toast(client, "a live restart is already under way");
        }
        self.start_upgrade(Asker::Menu(client), None);
    }

    /// Copy and try the kraal to restart on (`asked`, else the one installed), off the event
    /// loop, then hand over to it once nothing in the way is left (see `upgrade_now`).
    fn start_upgrade(&mut self, asker: Asker, asked: Option<PathBuf>) {
        self.upgrading = true;
        let started = self.background(
            move || prepare_binary(asked),
            move |server, prepared| match prepared {
                Ok(exe) => server.upgrade_when_idle(asker, Arc::new(exe), Instant::now() + IDLE_WAIT),
                Err(why) => server.upgrade_failed(asker, why),
            },
        );
        if !started {
            self.upgrade_failed(asker, "cannot start trying the binary".into());
        }
    }

    /// What prevents a live restart, if anything: what cannot be carried over.
    fn upgrade_refusal(&self) -> Option<String> {
        if !cfg!(target_os = "linux") {
            return Some("a live restart is only available on Linux for now".into());
        }
        if self.quitting || !self.kill_waiters.is_empty() {
            return Some("the server is stopping".into());
        }
        if let Some(run) = self.runs.first() {
            return Some(format!("runbook {} is running: wait for it to end", run.name));
        }
        if !self.launching.is_empty() || !self.stray_groups.is_empty() {
            return Some("a runbook's commands are starting or being stopped: try again in a moment".into());
        }
        if !self.edits.is_empty() {
            return Some("a server's file is open in the local editor: close it first".into());
        }
        if !self.switches.is_empty() {
            return Some("an agent is switching accounts: try again once it has".into());
        }
        if self.recovering {
            return Some(format!(
                "this server keeps terminals it could not take over as they were (workspace {RECOVERED}): \
                 restart it once you are done with them"
            ));
        }
        let mut ids: Vec<&PaneId> = self.panes.keys().collect();
        ids.sort();
        for id in ids {
            let p = &self.panes[id];
            if p.spec.close_on_success && p.exit_code.is_none() {
                return Some(format!(
                    "{} is still running (a transfer or a job): wait for it to end",
                    self.pane_label(*id)
                ));
            }
            if p.restart_with.is_some() {
                return Some(format!("{} is switching accounts: try again once it has", self.pane_label(*id)));
            }
            if p.is_terminal() && p.pid().is_none() {
                return Some(format!("{} cannot be carried over", self.pane_label(*id)));
            }
        }
        if let Some(w) = self.waits.first() {
            return Some(format!("`kraal wait` is waiting on {}: try again once it ends", self.pane_label(w.pane)));
        }
        // It would neither hold what is typed nor come back: what it is sending would be lost.
        if self.clients.values().any(|c| c.attached && !c.follows_live) {
            return Some("a terminal is attached with a kraal from before live restarts: detach it first".into());
        }
        if let Some(left) = self.unadopted.first() {
            return Some(format!(
                "pane {} was not taken over yet after the last live restart (its program runs): {}",
                left.live.id, left.why
            ));
        }
        None
    }

    /// What a live restart waits for, a few seconds at most: what usually ends by itself in a
    /// moment. A pane's program first: what keeps it busy lasts, and is what the user can act on.
    fn upgrade_busy(&self) -> Option<String> {
        self.pane_busy().or_else(|| self.server_busy())
    }

    /// What the server does that would be lost in the exec.
    fn server_busy(&self) -> Option<String> {
        if self.hooks.running() > 0 {
            return Some("a hook is running".into());
        }
        if !self.jobs.is_empty() || crate::exec::children_running() > 0 {
            return Some("background work is still running".into());
        }
        if !self.held_input.is_empty() {
            return Some("typed input waits for a pane's context to be read".into());
        }
        if self.tunnels.iter().any(|t| t.state == TunnelState::Connecting) {
            return Some("a tunnel is connecting".into());
        }
        None
    }

    /// A pane a live restart cannot take as it is now: a program that has not taken its input,
    /// or whose output stops in the middle of a sequence. Every terminal counts until nothing
    /// uses it: the pane's own program may have ended and left another one in the background.
    fn pane_busy(&self) -> Option<String> {
        let mut ids: Vec<&PaneId> = self.panes.keys().collect();
        ids.sort();
        for id in ids {
            let p = &self.panes[id];
            if p.terminal_ended() {
                continue;
            }
            if p.input_pending() > 0 {
                return Some(format!("{} has input it has not read yet", self.pane_label(*id)));
            }
            if p.mid_sequence() {
                return Some(format!("{} is in the middle of writing an escape sequence", self.pane_label(*id)));
            }
        }
        None
    }

    fn upgrade_when_idle(&mut self, asker: Asker, exe: Arc<Binary>, deadline: Instant) {
        // The command that asked gave up (ctrl+c): nothing happens.
        if let Some(client) = asker.request()
            && !self.clients.contains_key(&client)
        {
            self.upgrading = false;
            return;
        }
        if let Some(why) = self.upgrade_refusal() {
            return self.upgrade_failed(asker, why);
        }
        let busy = match self.upgrade_busy() {
            Some(why) => why,
            None => match self.upgrade_now(asker, &exe) {
                NotNow::Busy(why) => why,
                NotNow::Refused(why) => return self.upgrade_failed(asker, why),
            },
        };
        if Instant::now() >= deadline {
            return self.upgrade_failed(asker, format!("{busy}: try again in a moment"));
        }
        let started = self.background(
            || std::thread::sleep(Duration::from_millis(50)),
            move |server, ()| server.upgrade_when_idle(asker, exe, deadline),
        );
        if !started {
            self.upgrade_failed(asker, busy);
        }
    }

    fn upgrade_failed(&mut self, asker: Asker, why: String) {
        self.upgrading = false;
        match asker {
            Asker::Request(client) => {
                self.send_msg(client, &ServerMsg::Response { result: Err(why) });
                self.clients.remove(&client);
            }
            // Told to come back already: it hears why once it has.
            Asker::Menu(client) if !self.clients.contains_key(&client) => {
                self.warnings.push(format!("no live restart: {why}"));
            }
            Asker::Menu(client) => self.toast(client, format!("no live restart: {why}")),
        }
    }

    /// Hand everything over to `exe` and become it. Returns only when that did not happen, with
    /// why; the server then goes on as before.
    fn upgrade_now(&mut self, asker: Asker, exe: &Binary) -> NotNow {
        // Commands run by other threads end first, and none starts until the exec: it would
        // get the descriptors kept open for it, and be left behind.
        let Some(children) = crate::exec::hold_children(Duration::from_millis(500)) else {
            return NotNow::Busy("background work is still running".into());
        };
        let held = match self.quiesce(asker) {
            Ok(held) => held,
            Err(why) => return NotNow::Busy(why),
        };
        let why = self.hand_over(asker, exe, &held);
        drop(children);
        self.release(held);
        why
    }

    /// Stop taking connections (they wait for the next server in the socket's backlog), and
    /// take in what clients send until nothing more comes: commands are answered, and what they
    /// were sent written; attached clients hold what is typed from a point between two keys,
    /// and say so, every key they sent before handled (one from before live restarts is told to
    /// attach again, and is waited for to go). Connections accepted and not handled yet are
    /// kept for the next server.
    fn quiesce(&mut self, asker: Asker) -> Result<Held, String> {
        use std::sync::atomic::Ordering;
        self.accepting.paused.store(true, Ordering::SeqCst);
        let start = Instant::now();
        while !self.accepting.parked.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(1) {
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut held = Held { connections: Vec::new() };
        let mut told = std::collections::HashSet::new();
        self.hold_attempts += 1;
        let attempt = self.hold_attempts;
        let deadline = Instant::now() + CLIENTS_WAIT;
        loop {
            // Clients that attach meanwhile are asked too; one from before live restarts holds
            // the restart back (see `upgrade_refusal`). The keys held are for the pane focused.
            let asked: Vec<ClientId> = self
                .clients
                .values()
                .filter(|c| c.attached && c.follows_live && !told.contains(&c.id))
                .map(|c| c.id)
                .collect();
            for id in asked {
                told.insert(id);
                let pane = self.client_focus(id);
                self.send_msg(id, &ServerMsg::Hold { on: true, attempt, pane });
            }
            while let Ok(ev) = self.events_rx.try_recv() {
                match ev {
                    Event::Connection(stream) => held.connections.push(stream),
                    other => {
                        self.handle(other);
                        self.follow_focus();
                    }
                }
            }
            // As the event loop would: an escape alone for a moment is the Escape key.
            self.flush_stale_input();
            match self.clients_sending(asker, attempt) {
                None => break,
                Some(why) if Instant::now() >= deadline => {
                    self.release(held);
                    return Err(why);
                }
                Some(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        if let Some(why) = self.clients_busy() {
            self.release(held);
            return Err(why);
        }
        Ok(held)
    }

    /// What clients may still send, or be sent, if anything: `attempt` is this request to hold
    /// what is typed.
    fn clients_sending(&self, asker: Asker, attempt: u32) -> Option<String> {
        if self.clients.values().any(|c| !c.attached && Some(c.id) != asker.request()) {
            return Some("a command is still being answered".into());
        }
        if self.clients.values().any(|c| c.attached && !c.follows_live) {
            return Some("a terminal is attached with a kraal from before live restarts".into());
        }
        if self.clients.values().any(|c| c.attached && c.held != Some(attempt)) {
            return Some("a client has not stopped sending what is typed".into());
        }
        if super::clients::writers() > self.clients.len() + self.subscribers.len() + self.waits.len() {
            return Some("an answer is still being sent".into());
        }
        None
    }

    /// What an attached client is in the middle of, that the next server would not know: a key
    /// or a paste not whole yet, a menu or a prompt of kraal's (what it holds would go to the
    /// pane instead).
    fn clients_busy(&self) -> Option<String> {
        for c in self.clients.values().filter(|c| c.attached) {
            if !c.parser.idle() || c.paste.is_some() {
                return Some("a paste is still coming".into());
            }
            if !matches!(c.mode, Mode::Normal) {
                return Some("a client has a kraal menu or prompt open".into());
            }
        }
        None
    }

    /// The live restart did not happen: attached clients send what they held, and connections
    /// are accepted again.
    fn release(&mut self, held: Held) {
        for stream in held.connections {
            self.new_connection(stream);
        }
        let holding: Vec<ClientId> =
            self.clients.values().filter(|c| c.attached && c.follows_live).map(|c| c.id).collect();
        for id in holding {
            if let Some(c) = self.clients.get_mut(&id) {
                c.held = None;
                // The rest of a key cut in two comes once the client sends what it held.
                if c.pending_since.is_some() {
                    c.pending_since = Some(Instant::now());
                }
            }
            self.send_msg(id, &ServerMsg::Hold { on: false, attempt: 0, pane: None });
        }
        self.accepting.paused.store(false, std::sync::atomic::Ordering::SeqCst);
    }

    /// Stop reading the PTYs and write the hand-off, then exec. Returns only when that did not
    /// happen (the PTYs are read again then).
    fn hand_over(&mut self, asker: Asker, exe: &Binary, held: &Held) -> NotNow {
        // What the PTYs hold stays there from now on, and what was read is drawn: nothing read
        // is lost in the exec.
        let ids: Vec<PaneId> = self.panes.keys().copied().collect();
        for id in &ids {
            self.panes[id].stop_reading();
        }
        let why = self.prepare_and_exec(asker, exe, held, &ids);
        self.resume_panes();
        why
    }

    fn prepare_and_exec(&mut self, asker: Asker, exe: &Binary, held: &Held, ids: &[PaneId]) -> NotNow {
        if let Err(why) = self.drain_panes(ids) {
            return NotNow::Busy(why);
        }
        // What was typed meanwhile, or drawing it, may have started something (a hook, a job).
        if let Some(why) = self.upgrade_refusal() {
            return NotNow::Refused(why);
        }
        if let Some(why) = self.server_busy() {
            return NotNow::Busy(why);
        }
        for id in ids {
            let signals = self.panes.get_mut(id).map(|p| p.end_sync()).unwrap_or_default();
            self.pane_signals(*id, signals);
        }
        // Input and answers on their way reach the programs; a program that does not take
        // them is waited for (they would be lost), and so is one in the middle of a sequence
        // (the next emulator could not read its end).
        let start = Instant::now();
        while self.panes.values().any(|p| !p.terminal_ended() && p.input_pending() > 0)
            && start.elapsed() < Duration::from_secs(1)
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        if let Some(why) = self.pane_busy() {
            return NotNow::Busy(why);
        }
        // The state is saved as always: should the new server not start, `kraal` brings the
        // layout back.
        self.persist();
        self.sync_persister(Duration::from_secs(3));
        let (done, flushed) = crossbeam_channel::bounded(1);
        if self.run_logs.send(crate::runbooks::LogJob::Flush(done)).is_ok() {
            let _ = flushed.recv_timeout(Duration::from_secs(3));
        }
        let mut handoff = self.handoff(exe);
        handoff.connections = held.connections.iter().map(|c| c.as_raw_fd()).collect();
        let fds = handoff.fds();
        let path = handoff_path(&self.session);
        // Written off this thread, and read back as the next server reads it: should either
        // fail, or the disk not answer, nothing happens.
        let data = match encode_handoff(&handoff, MAX_HANDOFF) {
            Ok(data) => data,
            Err(why) => return NotNow::Refused(why),
        };
        let (target, stale) = (path.clone(), path.clone());
        let written = within(
            WRITE_WAIT,
            move || {
                crate::paths::ensure_private_dir(&crate::paths::state_dir())?;
                crate::paths::write_private(&target, &data)
            },
            move |_| {
                let _ = std::fs::remove_file(&stale);
            },
        );
        match written {
            Some(Ok(())) => {}
            Some(Err(e)) => return NotNow::Refused(format!("cannot write {}: {e}", path.display())),
            None => return NotNow::Refused(format!("{} does not answer", crate::paths::state_dir().display())),
        }
        // The ssh that list remote sessions' agents would lose their pipes: the next server
        // starts them again.
        self.stop_remotes();
        self.remotes.clear();
        if let Err(why) = self.let_clients_go(asker) {
            let _ = std::fs::remove_file(&path);
            return NotNow::Refused(why);
        }
        let why = match fds.iter().try_for_each(|fd| set_inherited(*fd, true)) {
            Ok(()) => format!("could not run {}: {}", exe.path.display(), exec(exe, &self.session, &path)),
            Err(e) => format!("cannot keep the panes' terminals open: {e}"),
        };
        for fd in &fds {
            let _ = set_inherited(*fd, false);
        }
        let _ = std::fs::remove_file(&path);
        NotNow::Refused(why)
    }

    /// Feed the panes what their readers read before they stopped.
    fn drain_panes(&mut self, ids: &[PaneId]) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut waiting = ids.to_vec();
        loop {
            let mut still = Vec::new();
            for id in waiting {
                while let Some(p) = self.panes.get(&id) {
                    match p.drain() {
                        Drained::Output(data) => self.pane_output(id, &data),
                        Drained::Reading => {
                            still.push(id);
                            break;
                        }
                        Drained::Stopped => break,
                    }
                }
            }
            let Some(first) = still.first() else { return Ok(()) };
            if Instant::now() >= deadline {
                return Err(format!("the terminal of {} did not stop in time", self.pane_label(*first)));
            }
            waiting = still;
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Read the PTYs again: the live restart did not happen.
    fn resume_panes(&mut self) {
        let ids: Vec<PaneId> = self.panes.keys().copied().collect();
        for id in ids {
            let Some(p) = self.panes.get_mut(&id) else { continue };
            if let Err(e) = p.resume_reading(&self.tx) {
                self.warnings.push(format!("{}: {e:#}", self.pane_label(id)));
            }
        }
        self.mark_dirty();
    }

    fn handoff(&mut self, exe: &Binary) -> Handoff {
        let mut state = persist::snapshot(self, true);
        // Tunnels go with their ssh, below.
        state.tunnels.clear();
        let mut ids: Vec<PaneId> = self.panes.keys().copied().collect();
        ids.sort();
        let panes: Vec<LivePane> = ids.iter().filter_map(|id| self.panes.get_mut(id)?.live()).collect();
        let pane_pids: Vec<u32> = panes.iter().map(|p| p.pid).collect();
        let a = self.layout_area;
        Handoff {
            version: HANDOFF_VERSION,
            from: env!("CARGO_PKG_VERSION").into(),
            session: self.session.clone(),
            listener: self.listener_fd,
            binary: exe.fd(),
            installed: exe.path.clone(),
            next_id: self.next_id,
            layout_area: (a.x, a.y, a.width, a.height),
            sidebar_was_useful: self.sidebar_was_useful,
            state,
            panes,
            tunnels: self.live_tunnels(),
            events: self.events.save(),
            announced: self.announced.iter().copied().collect(),
            pane_states: self.pane_states.iter().map(|(id, (s, p))| (*id, s.clone(), p.clone())).collect(),
            ports: self.ports.iter().map(|(port, pane)| (*port, *pane)).collect(),
            port_candidates: self.port_candidates.iter().map(|(port, pane)| (*port, *pane)).collect(),
            connections: Vec::new(),
            // The panes' own go with them.
            reaping: crate::exec::reaping().into_iter().filter(|pid| !pane_pids.contains(pid)).collect(),
            live_restarts: self.live_restarts,
        }
    }

    /// Tell the attached clients the server restarts live (each comes back to the workspace it
    /// shows), let every connection go but the asker's, and wait until what each was sent is
    /// written: the exec would cut it short. Event streams end there, as with any restart.
    fn let_clients_go(&mut self, asker: Asker) -> Result<(), String> {
        let ids: Vec<ClientId> = self.clients.values().filter(|c| c.attached).map(|c| c.id).collect();
        for id in ids {
            let ws = self.clients.get(&id).and_then(|c| self.ws_index(c.workspace));
            let workspace = ws.map(|wi| self.workspaces[wi].name.clone()).unwrap_or_default();
            let reason = "kraal restarted live: run kraal to attach again".to_string();
            self.send_msg(id, &ServerMsg::Exit { reason, reattach: Some(workspace), live: true });
        }
        let keep = asker.request();
        let gone: Vec<ClientId> = self.clients.keys().copied().filter(|id| Some(*id) != keep).collect();
        for id in gone {
            self.drop_client(id);
        }
        self.subscribers.clear();
        let start = Instant::now();
        while super::clients::writers() > self.clients.len() {
            if start.elapsed() > CLIENTS_WAIT {
                return Err("a client does not take what it is sent".into());
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }

    /// Take over from the server this one replaced: its panes on their PTYs and programs, its
    /// tunnels, its events.
    pub(super) fn take_over(&mut self, h: Handoff) {
        self.next_id = self.next_id.max(h.next_id);
        let (x, y, w, height) = h.layout_area;
        self.layout_area = Rect::new(x, y, w, height);
        self.sidebar_was_useful = h.sidebar_was_useful;
        self.announced = h.announced.into_iter().collect();
        self.pane_states = h.pane_states.into_iter().map(|(id, s, p)| (id, (s, p))).collect();
        self.ports = h.ports.into_iter().collect();
        self.port_candidates = h.port_candidates.into_iter().collect();
        for pid in h.reaping {
            if crate::exec::is_child(pid) {
                crate::exec::reap_pid_later(pid);
            }
        }
        for t in h.tunnels {
            // SAFETY: the hand-off gives this process the descriptor, which nothing else owns.
            let stderr = t.stderr.map(|fd| unsafe { OwnedFd::from_raw_fd(fd) });
            self.adopt_tunnel(t, stderr);
        }
        let mut live: HashMap<PaneId, LivePane> = h.panes.into_iter().map(|p| (p.id, p)).collect();
        self.live_restarts = h.live_restarts + 1;
        self.outcomes.clear();
        // Nothing opens: the workspaces are the same ones, their events said already, and the
        // hooks run for them already. A pane that cannot be taken over is left out of its
        // place, and alone: its program runs on, on its terminal, kept open to try again. No
        // pane is started in the place of one handed over.
        let hooks = std::mem::take(&mut self.hooks.list);
        persist::apply_layout(self, h.state.workspaces, h.state.revision, true, &mut |server, p| {
            let Some(lp) = p.id.and_then(|id| live.remove(&id)) else {
                return match p.id {
                    // A preview or a file manager, opened again where it was (no program runs
                    // in either).
                    Some(id) if p.spec.preview.is_some() || p.spec.files.is_some() => {
                        server.spawn_pane_as(id, p.name.clone(), p.spec.clone())
                    }
                    _ => Err(anyhow::anyhow!("nothing was handed over for it")),
                };
            };
            let id = lp.id;
            let outcome = server.adopt_live(lp, p.name.clone(), p.scrollback.clone(), None);
            let placed = matches!(outcome, Outcome::Kept | Outcome::TerminalOnly);
            server.set_outcome(id, outcome);
            if placed { Ok(id) } else { Err(anyhow::anyhow!("pane {id} was not taken over")) }
        });
        self.hooks.list = hooks;
        self.events = EventLog::restore(h.events);
        // What has no place any more is never closed: its program may still use its terminal.
        for (_, live) in live {
            let id = live.id;
            let why = "it had no place in the layout".to_string();
            self.set_outcome(id, Outcome::Waiting(why.clone()));
            self.unadopted.push(LeftOver { live, name: None, why, recovered: None });
        }
        self.adopt_left_over();
        self.relayout();
        // Full-screen programs draw themselves again: a size change, then back.
        let nudged: Vec<PaneId> =
            self.panes.values().filter(|p| p.full_screen() && p.exit_code.is_none()).map(|p| p.id).collect();
        for id in &nudged {
            if let Some(p) = self.panes.get_mut(id) {
                p.nudge();
            }
        }
        if !nudged.is_empty() {
            self.background(
                || std::thread::sleep(Duration::from_millis(200)),
                move |server, ()| {
                    for id in nudged {
                        if let Some(p) = server.panes.get_mut(&id) {
                            p.refresh_pixel_size();
                        }
                    }
                },
            );
        }
        for fd in h.connections {
            // SAFETY: as above.
            self.new_connection(unsafe { UnixStream::from_raw_fd(fd) });
        }
        let report =
            self.live_report().unwrap_or(crate::protocol::LiveRestart { count: 0, kept: 0, not_kept: Vec::new() });
        let text = format!(
            "restarted live from kraal {}: {} panes kept running, {} not as they were",
            h.from,
            report.kept,
            report.not_kept.len()
        );
        eprintln!("kraal: {text}");
        self.log_message(&text, !report.not_kept.is_empty());
    }

    /// What the last live restart took over, told from what became of each pane.
    pub(super) fn live_report(&self) -> Option<crate::protocol::LiveRestart> {
        (self.live_restarts > 0).then(|| crate::protocol::LiveRestart {
            count: self.live_restarts,
            kept: self.outcomes.values().filter(|o| **o == Outcome::Kept).count() as u32,
            not_kept: self.outcomes.iter().filter_map(|(id, o)| o.line(*id)).collect(),
        })
    }

    /// Pane `id` handed over came to `outcome`: said once, when it is not as it was.
    fn set_outcome(&mut self, id: PaneId, outcome: Outcome) {
        if let Some(line) = outcome.line(id)
            && self.outcomes.get(&id) != Some(&outcome)
        {
            self.warnings.push(line);
        }
        self.outcomes.insert(id, outcome);
    }

    /// The pane `live` describes, made on its PTY and its program (the outcome says whether the
    /// program was taken too: one that is not this process's child, or no longer the one it
    /// was, is left alone). One that cannot be made now goes to the panes to take over later,
    /// its PTY open and its program untouched. `recovered`: a terminal kept from a hand-off
    /// that could not be read.
    fn adopt_live(
        &mut self,
        live: LivePane,
        name: Option<String>,
        scrollback: Option<String>,
        recovered: Option<Recovered>,
    ) -> Outcome {
        let id = live.id;
        // SAFETY: plain syscalls on a descriptor the hand-off names.
        if unsafe { libc::isatty(live.fd) } != 1 {
            // Whatever it is, it is no terminal: no program uses it as one.
            if unsafe { libc::fcntl(live.fd, libc::F_GETFD) } >= 0 {
                // SAFETY: the hand-off gives this process the descriptor, which nothing else owns.
                drop(unsafe { OwnedFd::from_raw_fd(live.fd) });
            }
            return Outcome::NoTerminal;
        }
        #[cfg(debug_assertions)]
        if unadoptable(name.as_deref(), id) {
            return self.adopt_later(LeftOver { live, name, why: "a test says it cannot be".into(), recovered });
        }
        // SAFETY: plain syscall on a pid the hand-off names.
        let group = unsafe { libc::getpgid(live.pid as libc::pid_t) };
        let ours = live.pid > 0 && crate::exec::is_child(live.pid) && (group < 0 || group == live.pgid);
        // The hand-off's descriptor stays as it is until the pane is made: the pane gets a copy.
        // SAFETY: fcntl on a descriptor the hand-off names; the copy is a fresh one.
        let copy = unsafe { libc::fcntl(live.fd, libc::F_DUPFD_CLOEXEC, 0) };
        if copy < 0 {
            let why = format!("{}", std::io::Error::last_os_error());
            return self.adopt_later(LeftOver { live, name, why, recovered });
        }
        // SAFETY: a fresh descriptor nothing else owns.
        let copy = unsafe { OwnedFd::from_raw_fd(copy) };
        let mut pane = match Pane::adopt(&live, copy, ours, name.clone(), &self.env, &self.tx) {
            Ok(pane) => pane,
            Err(e) => return self.adopt_later(LeftOver { live, name, why: format!("{e:#}"), recovered }),
        };
        // SAFETY: the hand-off gives this process the descriptor; the pane has its copy.
        drop(unsafe { OwnedFd::from_raw_fd(live.fd) });
        pane.recovered = recovered;
        self.panes.insert(id, pane);
        if let Some(file) = scrollback {
            self.scrollback.insert(id, SavedScrollback { file: Some(file), taken: Instant::now() });
        }
        if let Some(recovered) = recovered {
            return Outcome::Salvaged { known: recovered == Recovered::Known };
        }
        if live.suspended && live.spec.pending_agent.is_some() {
            self.resolve_pending_agent(id);
        }
        if ours { Outcome::Kept } else { Outcome::TerminalOnly }
    }

    /// `left` is taken over later: its PTY stays open, its program runs.
    fn adopt_later(&mut self, left: LeftOver) -> Outcome {
        let why = left.why.clone();
        self.unadopted.push(left);
        Outcome::Waiting(why)
    }

    /// Keep the terminals a hand-off that could not be read kept open (see `salvage`), as they
    /// are: nothing else is made, no layout and no program; no state is written over the saved
    /// one. Each terminal shows in workspace "recovered" under the id its programs know, or,
    /// when that is unknown, read-only.
    pub(super) fn take_salvaged(&mut self, salvaged: Vec<Salvaged>) {
        self.recovering = true;
        self.live_restarts = 1;
        // Their ids first: nothing else may take them.
        let known: Vec<PaneId> = salvaged.iter().filter_map(|s| s.pane).collect();
        self.next_id = self.next_id.max(known.iter().max().map_or(0, |id| id + 1));
        let mut seen = std::collections::HashSet::new();
        for Salvaged { mut live, pane } in salvaged {
            let recovered = match pane.filter(|id| seen.insert(*id)) {
                Some(id) => {
                    live.id = id;
                    Recovered::Known
                }
                None => {
                    live.id = self.alloc_id();
                    Recovered::Unknown
                }
            };
            let why = "the hand-off could not be read".to_string();
            self.unadopted.push(LeftOver { live, name: None, why, recovered: Some(recovered) });
        }
        self.warnings.push(format!(
            "the previous server's state could not be read: what ran in its panes runs on, as it is, in workspace \
             {RECOVERED}; its saved layout is kept for a restart"
        ));
        self.adopt_left_over();
    }

    /// Try again to take over the panes a live restart could not take yet: each one taken shows
    /// in a tab of its own in workspace "recovered" (no hook runs for it: it opens nothing).
    pub(super) fn adopt_left_over(&mut self) {
        self.unadopted_tried = Instant::now();
        for left in std::mem::take(&mut self.unadopted) {
            let LeftOver { live, name, why, recovered } = left;
            let id = live.id;
            let outcome = match self.adopt_live(live, name, None, recovered) {
                Outcome::Kept | Outcome::TerminalOnly => Outcome::Recovered(why),
                other => other,
            };
            let placed = matches!(outcome, Outcome::Recovered(_) | Outcome::Salvaged { .. });
            self.set_outcome(id, outcome);
            if !placed {
                continue;
            }
            let hooks = std::mem::take(&mut self.hooks.list);
            let ws = match self.workspaces.iter().find(|w| w.name == RECOVERED) {
                Some(w) => w.id,
                None => self.new_workspace(RECOVERED.into(), None),
            };
            self.hooks.list = hooks;
            let Some(wi) = self.ws_index(ws) else { continue };
            let mut tab = super::Tab::new(self.alloc_id(), String::new());
            tab.root = Some(crate::layout::Node::Leaf(id));
            tab.set_focus(id);
            self.workspaces[wi].tabs.push(tab);
            self.relayout();
            self.mark_dirty();
        }
    }
}

/// What became of a pane a live restart handed over: one each (see `Server::outcomes`), what
/// the report, its counts and the warnings are told from.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Outcome {
    /// Taken over in its place, with its program.
    Kept,
    /// Taken over in its place; its program is not this server's child, to wait for.
    TerminalOnly,
    /// What was handed over for it was no terminal: there was nothing to take over.
    NoTerminal,
    /// Not taken over yet (why): its terminal stays open and its program runs, not read until
    /// it is taken over.
    Waiting(String),
    /// Taken over after it could not be at first (why), in workspace recovered.
    Recovered(String),
    /// Kept from a hand-off that could not be read, in workspace recovered: under its own id
    /// (`known`), or read-only.
    Salvaged { known: bool },
}

impl Outcome {
    /// What the report says of pane `id`, unless it was taken over as it was.
    fn line(&self, id: PaneId) -> Option<String> {
        Some(match self {
            Outcome::Kept => return None,
            Outcome::TerminalOnly => {
                format!("pane {id}: its terminal is kept, but its program is not this server's to wait for")
            }
            Outcome::NoTerminal => format!("pane {id}: no terminal was handed over for it; it is gone"),
            Outcome::Waiting(why) => format!(
                "pane {id}: not taken over yet ({why}); its program runs on, and it shows in workspace {RECOVERED} once it is"
            ),
            Outcome::Recovered(why) => format!("pane {id}: taken over late ({why}); it shows in workspace {RECOVERED}"),
            Outcome::Salvaged { known: true } => {
                format!("pane {id}: kept as it is (the hand-off could not be read), in workspace {RECOVERED}")
            }
            Outcome::Salvaged { known: false } => format!(
                "pane {id}: a terminal kept as it is (the hand-off could not be read), whose pane is unknown: \
                 read-only, in workspace {RECOVERED}"
            ),
        })
    }
}

/// A terminal kept from a hand-off that could not be read (see `salvage`), with the pane its
/// programs know, when that can be read from them.
pub struct Salvaged {
    live: LivePane,
    pane: Option<PaneId>,
}

/// Where panes a live restart took over late show.
const RECOVERED: &str = "recovered";

/// A pane a live restart did not take over yet (see `Server::adopt_left_over`).
pub(super) struct LeftOver {
    live: LivePane,
    name: Option<String>,
    why: String,
    recovered: Option<Recovered>,
}

/// Debug builds: whether a test asks that pane `name` (or `#ID`) cannot be taken over for now
/// (the file `KRAAL_TEST_UNADOPTABLE` names lists such panes, one a line; `NAME once`: only the
/// first time it is tried).
#[cfg(debug_assertions)]
fn unadoptable(name: Option<&str>, id: PaneId) -> bool {
    static TRIED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let Some(file) = std::env::var_os("KRAAL_TEST_UNADOPTABLE") else { return false };
    let listed = std::fs::read_to_string(file).unwrap_or_default();
    let by_id = format!("#{id}");
    let names: Vec<&str> = name.into_iter().chain([by_id.as_str()]).collect();
    listed.lines().any(|l| match l.split_once(' ') {
        Some((n, "once")) if names.contains(&n) => {
            let mut tried = TRIED.lock().unwrap_or_else(|e| e.into_inner());
            let first = !tried.iter().any(|t| t == n);
            tried.push(n.into());
            first
        }
        _ => names.contains(&l),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_new_binary_is_the_one_on_disk_and_says_what_it_reads() {
        assert_eq!(check_answer(HANDOFF_VERSION), format!("kraal hand-off {HANDOFF_VERSION}"));
        assert!(handoff_check(HANDOFF_VERSION + 1).is_err());
        let exe = binary_to_run(None).unwrap();
        assert!(exe.is_file());
        assert!(!exe.to_string_lossy().ends_with(" (deleted)"));
        assert!(binary_to_run(Some(PathBuf::from("kraal"))).is_err(), "a relative path");
        assert!(binary_to_run(Some(std::env::temp_dir().join("kraal-nothing-here"))).is_err());
    }

    #[test]
    fn the_binary_run_is_the_copy_tried_whatever_happens_to_the_file() {
        use std::io::{Read, Seek, Write};
        let dir = std::env::temp_dir().join(format!("kraal-binary-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("kraal");
        std::fs::write(&path, "one").unwrap();
        let mut opened = Binary::open(Some(path.clone())).unwrap();
        // Written to in place, then another file renamed over its path: the copy is still what
        // was tried.
        std::fs::OpenOptions::new().write(true).open(&path).unwrap().write_all(b"two").unwrap();
        std::fs::write(dir.join("other"), "three").unwrap();
        std::fs::rename(dir.join("other"), &path).unwrap();
        let mut read = String::new();
        opened.file.rewind().unwrap();
        opened.file.read_to_string(&mut read).unwrap();
        assert_eq!(read, "one");
        assert_eq!(opened.path, path, "where it is installed is kept");
        #[cfg(target_os = "linux")]
        assert!(opened.file.write_all(b"x").is_err(), "the copy cannot change");
        // Not something that runs: the exec fails, and says why; this process goes on.
        let err = exec(&opened, "s", Path::new("/nonexistent/handoff.json"));
        assert!(err.raw_os_error().is_some() || err.kind() == std::io::ErrorKind::Unsupported, "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn hand_offs_are_files_of_their_own_read_only_whole_and_of_this_format() {
        assert_ne!(handoff_path("s"), handoff_path("s"), "an attempt's file is its own");
        assert!(handoff_check(1).is_err(), "the format before this one is not read");
    }

    #[test]
    fn a_hand_off_is_read_however_deep_its_layout() {
        let mut node = persist::NodeSnap::Pane(Box::new(
            serde_json::from_str(r#"{"name":null,"spec":{},"agent":null,"agent_session":null}"#).unwrap(),
        ));
        for i in 0..200 {
            let axis = if i % 2 == 0 { crate::layout::Axis::Row } else { crate::layout::Axis::Column };
            let leaf = persist::NodeSnap::Pane(Box::new(
                serde_json::from_str(r#"{"name":null,"spec":{},"agent":null,"agent_session":null}"#).unwrap(),
            ));
            node = persist::NodeSnap::Split { axis, children: vec![(leaf, 0.5), (node, 0.5)] };
        }
        let state = SessionSnapshot {
            version: 1,
            revision: 1,
            workspaces: vec![persist::WorkspaceSnap {
                name: "w".into(),
                definition: None,
                cwd: None,
                active: 0,
                tabs: vec![persist::TabSnap {
                    name: String::new(),
                    root: Some(node),
                    floating: Vec::new(),
                    focus: None,
                    show_floating: false,
                    zoomed: false,
                    sync: false,
                }],
                sync: Default::default(),
            }],
            tunnels: Vec::new(),
        };
        let h = Handoff {
            version: HANDOFF_VERSION,
            from: "x".into(),
            session: "s".into(),
            listener: -1,
            binary: -1,
            installed: PathBuf::from("/kraal"),
            next_id: 1,
            layout_area: (0, 0, 80, 24),
            sidebar_was_useful: false,
            state,
            panes: Vec::new(),
            tunnels: Vec::new(),
            events: EventLog::default().save(),
            announced: Vec::new(),
            pane_states: Vec::new(),
            ports: Vec::new(),
            port_candidates: Vec::new(),
            connections: Vec::new(),
            reaping: Vec::new(),
            live_restarts: 0,
        };
        let data = serde_json::to_vec(&h).unwrap();
        assert!(serde_json::from_slice::<Handoff>(&data).is_err(), "too deep for the default reader");
        assert!(parse_handoff(&data).is_ok());
        // Not written when larger than the next server reads.
        assert!(encode_handoff(&h, data.len()).is_ok());
        assert!(encode_handoff(&h, data.len() - 1).unwrap_err().contains("too large"));
    }

    #[test]
    fn work_that_takes_too_long_is_left_to_clean_up_after_itself() {
        assert_eq!(within(Duration::from_secs(5), || 7, |_| {}), Some(7));
        let (tx, rx) = std::sync::mpsc::channel();
        let late = within(
            Duration::from_millis(50),
            || {
                std::thread::sleep(Duration::from_millis(300));
                8
            },
            move |v| {
                let _ = tx.send(v);
            },
        );
        assert_eq!(late, None);
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(8), "what it did once done is undone");
    }

    #[test]
    fn descriptors_stay_open_over_exec_only_when_asked() {
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        let fd = std::os::fd::AsRawFd::as_raw_fd(&a);
        // SAFETY: fcntl on a descriptor this test owns.
        let cloexec = || unsafe { libc::fcntl(fd, libc::F_GETFD) } & libc::FD_CLOEXEC != 0;
        assert!(cloexec());
        set_inherited(fd, true).unwrap();
        assert!(!cloexec());
        set_inherited(fd, false).unwrap();
        assert!(cloexec());
    }
}
