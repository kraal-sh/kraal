//! Live restart: the server replaces itself with the kraal binary now installed, and no program
//! in its panes stops.
//!
//! `exec` keeps the process, so the panes' programs stay its children, and their PTYs stay open
//! when their descriptors are not closed on exec. First the server checks that the new binary
//! runs and reads this hand-off's format, stops reading the PTYs (what was read is drawn first),
//! and writes a private hand-off file: the session's state with each pane's id, and for each
//! terminal pane its PTY's descriptor, its program, and what its emulator shows (text with its
//! attributes, cursors, modes). The new server reads it (and deletes it), keeps listening on
//! the same socket, rebuilds every pane on its PTY and program, and has full-screen programs
//! draw themselves again. Attached clients are told, and come back once it answers.
//!
//! What cannot be carried over is refused, with the reason: a runbook run, a transfer, a file
//! open in the local editor, an account switch, a `kraal wait`, and a hook or background work
//! still running after a few seconds. Tunnels keep their ssh; those that list remote sessions'
//! agents are started again.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ratatui::layout::Rect;
use serde::{Deserialize, Serialize};

use super::events::{EventLog, SavedLog};
use super::net::{LiveTunnel, TunnelState};
use super::ui::{ConfirmAction, Mode};
use super::{ClientId, Server};
use crate::layout::PaneId;
use crate::pane::{Drained, LivePane, Pane};
use crate::persist::{self, PaneSnap, SavedScrollback, SessionSnapshot};
use crate::protocol::{Response, ServerMsg};

/// The format of the hand-off file. A kraal takes over only from one whose format it reads.
pub const HANDOFF_VERSION: u32 = 1;

/// How long a live restart waits for background work to end before it gives up.
const IDLE_WAIT: Duration = Duration::from_secs(5);

/// What a server hands over to the one that replaces it.
#[derive(Serialize, Deserialize)]
pub struct Handoff {
    version: u32,
    /// The version of the kraal that wrote it.
    from: String,
    session: String,
    /// The listening socket, kept open: a client connecting meanwhile waits for the new server.
    listener: RawFd,
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
}

impl Handoff {
    /// The descriptors it names: they stay open over the exec.
    fn fds(&self) -> Vec<RawFd> {
        let mut fds = vec![self.listener];
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

/// Whether `exe` runs here and takes over this hand-off's format.
fn check_binary(exe: &Path) -> Result<(), String> {
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["server", "--handoff-check", &HANDOFF_VERSION.to_string()]);
    let out = crate::exec::run(&mut cmd, Duration::from_secs(10), 4096)
        .map_err(|e| format!("cannot run {}: {e}", exe.display()))?;
    if out.success() && out.stdout_text().trim() == check_answer(HANDOFF_VERSION) {
        return Ok(());
    }
    Err(format!("the kraal installed cannot take over live ({}: an older kraal, or not one)", exe.display()))
}

fn handoff_path(session: &str) -> PathBuf {
    crate::paths::state_dir().join(format!("handoff-{session}-{}.json", std::process::id()))
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

/// Replace this process with `exe`, a server of `session` that takes over from `handoff`.
/// Returns only when that failed, with why.
fn exec(exe: &Path, session: &str, handoff: &Path) -> std::io::Error {
    use std::ffi::CString;
    let arg0 = std::env::args_os().next().unwrap_or_else(|| exe.as_os_str().to_owned());
    let args: [&OsStr; 6] = [
        &arg0,
        OsStr::new("--handoff"),
        handoff.as_os_str(),
        OsStr::new("server"),
        OsStr::new("--session"),
        OsStr::new(session),
    ];
    let c = |s: &OsStr| CString::new(s.as_bytes());
    let (Ok(program), Ok(argv)) = (c(exe.as_os_str()), args.iter().map(|a| c(a)).collect::<Result<Vec<_>, _>>()) else {
        return std::io::Error::new(std::io::ErrorKind::InvalidInput, "a path holds a NUL byte");
    };
    let mut ptrs: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    // SAFETY: NUL-terminated strings in a NULL-terminated array, alive across the call; execv
    // returns only when it failed.
    unsafe { libc::execv(program.as_ptr(), ptrs.as_ptr()) };
    std::io::Error::last_os_error()
}

/// Read the hand-off at `path` (deleted at once: it holds what the panes showed) for
/// `session`. The descriptors it names are not inherited by what this server starts.
pub fn read_handoff(path: &Path, session: &str) -> Result<Handoff> {
    let data = crate::paths::read_regular(path, 1 << 30);
    let _ = std::fs::remove_file(path);
    let data = data.with_context(|| format!("reading {}", path.display()))?;
    let h: Handoff = serde_json::from_slice(&data).with_context(|| format!("parsing {}", path.display()))?;
    if h.version != HANDOFF_VERSION {
        bail!("hand-off format {} (this kraal reads {HANDOFF_VERSION})", h.version);
    }
    if h.session != session {
        bail!("the hand-off is session {:?}'s", h.session);
    }
    for fd in h.fds() {
        let _ = set_inherited(fd, false);
    }
    Ok(h)
}

/// Close every descriptor but the standard ones: those of a hand-off that cannot be read (the
/// PTYs, whose programs then get a hangup as with a server that stops, and the socket).
pub fn close_inherited() {
    let dir = if cfg!(target_os = "linux") { "/proc/self/fd" } else { "/dev/fd" };
    let fds: Vec<RawFd> = std::fs::read_dir(dir)
        .map(|d| d.flatten().filter_map(|e| e.file_name().to_str()?.parse().ok()).filter(|fd| *fd > 2).collect())
        .unwrap_or_default();
    for fd in fds {
        // SAFETY: nothing in this process uses these yet (the directory's own is closed already).
        unsafe {
            libc::close(fd);
        }
    }
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
        let exe = match binary_to_run(binary) {
            Ok(exe) => exe,
            Err(e) => return Some(Err(e)),
        };
        if check {
            let started = self.background(
                move || check_binary(&exe),
                move |server, result| {
                    server.send_msg(client, &ServerMsg::Response { result: result.map(|()| Response::Ok) });
                    server.clients.remove(&client);
                },
            );
            return (!started).then(|| Err("cannot start trying the binary".into()));
        }
        self.start_upgrade(Asker::Request(client), exe);
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
            || binary_to_run(None).and_then(|exe| check_binary(&exe)),
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
        match binary_to_run(None) {
            Ok(exe) => self.start_upgrade(Asker::Menu(client), exe),
            Err(why) => self.toast(client, format!("no live restart: {why}")),
        }
    }

    /// Try `exe`, then hand over to it once nothing in the way is left (see `upgrade_now`).
    fn start_upgrade(&mut self, asker: Asker, exe: PathBuf) {
        self.upgrading = true;
        let checked = exe.clone();
        let started = self.background(
            move || check_binary(&checked),
            move |server, result| match result {
                Ok(()) => server.upgrade_when_idle(asker, exe, Instant::now() + IDLE_WAIT),
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
        None
    }

    /// What a live restart waits for, a few seconds at most: work that usually ends by itself
    /// in a moment.
    fn upgrade_busy(&self) -> Option<&'static str> {
        if self.hooks.running() > 0 {
            return Some("a hook is running");
        }
        if !self.jobs.is_empty() {
            return Some("background work is still running");
        }
        if !self.held_input.is_empty() {
            return Some("typed input waits for a pane's context to be read");
        }
        if self.tunnels.iter().any(|t| t.state == TunnelState::Connecting) {
            return Some("a tunnel is connecting");
        }
        None
    }

    fn upgrade_when_idle(&mut self, asker: Asker, exe: PathBuf, deadline: Instant) {
        // The command that asked gave up (ctrl+c): nothing happens.
        if let Asker::Request(client) = asker
            && !self.clients.contains_key(&client)
        {
            self.upgrading = false;
            return;
        }
        if let Some(why) = self.upgrade_refusal() {
            return self.upgrade_failed(asker, why);
        }
        if let Some(why) = self.upgrade_busy() {
            if Instant::now() >= deadline {
                return self.upgrade_failed(asker, format!("{why}: try again in a moment"));
            }
            let started = self.background(
                || std::thread::sleep(Duration::from_millis(50)),
                move |server, ()| server.upgrade_when_idle(asker, exe, deadline),
            );
            if !started {
                self.upgrade_failed(asker, why.to_string());
            }
            return;
        }
        let why = self.upgrade_now(&exe);
        self.upgrade_failed(asker, why);
    }

    fn upgrade_failed(&mut self, asker: Asker, why: String) {
        self.upgrading = false;
        match asker {
            Asker::Request(client) => {
                self.send_msg(client, &ServerMsg::Response { result: Err(why) });
                self.clients.remove(&client);
            }
            Asker::Menu(client) => self.toast(client, format!("no live restart: {why}")),
        }
    }

    /// Hand everything over to `exe` and become it. Returns only when that did not happen, with
    /// why; the server then goes on as before. From now on connections wait for the next server
    /// (in the socket's backlog), and those accepted but not handled yet go to it too.
    fn upgrade_now(&mut self, exe: &Path) -> String {
        let held = self.hold_connections();
        let fds: Vec<RawFd> = held.0.iter().map(std::os::fd::AsRawFd::as_raw_fd).collect();
        let why = self.hand_over(exe, fds);
        self.release_connections(held);
        why
    }

    /// Stop accepting connections, and take those waiting to be handled out of the events, with
    /// the other events read meanwhile.
    fn hold_connections(&mut self) -> (Vec<std::os::unix::net::UnixStream>, Vec<super::Event>) {
        use std::sync::atomic::Ordering;
        self.accepting.paused.store(true, Ordering::SeqCst);
        let start = Instant::now();
        while !self.accepting.parked.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(1) {
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut connections = Vec::new();
        let mut events = Vec::new();
        while let Ok(ev) = self.events_rx.try_recv() {
            match ev {
                super::Event::Connection(stream) => connections.push(stream),
                other => events.push(other),
            }
        }
        (connections, events)
    }

    /// The live restart did not happen: what was held is handled, and connections accepted again.
    fn release_connections(&mut self, (connections, events): (Vec<std::os::unix::net::UnixStream>, Vec<super::Event>)) {
        for ev in events {
            self.handle(ev);
        }
        for stream in connections {
            self.new_connection(stream);
        }
        self.accepting.paused.store(false, std::sync::atomic::Ordering::SeqCst);
    }

    fn hand_over(&mut self, exe: &Path, connections: Vec<RawFd>) -> String {
        // What the PTYs hold stays there from now on, and what was read is drawn: nothing read
        // is lost in the exec.
        let ids: Vec<PaneId> = self.panes.keys().copied().collect();
        for id in &ids {
            self.panes[id].stop_reading();
        }
        if let Err(why) = self.drain_panes(&ids) {
            self.resume_panes();
            return why;
        }
        // Drawing it may have started something (a hook).
        let started = (self.hooks.running() > 0).then(|| "a hook started: try again in a moment".to_string());
        if let Some(why) = self.upgrade_refusal().or(started) {
            self.resume_panes();
            return why;
        }
        for id in &ids {
            let signals = self.panes.get_mut(id).map(|p| p.end_sync()).unwrap_or_default();
            self.pane_signals(*id, signals);
        }
        // Input and answers on their way reach the programs.
        let start = Instant::now();
        while self.panes.values().any(|p| p.input_pending() > 0) && start.elapsed() < Duration::from_secs(1) {
            std::thread::sleep(Duration::from_millis(5));
        }
        // The state is saved as always: should the new server not start, `kraal` brings the
        // layout back.
        self.persist();
        self.sync_persister(Duration::from_secs(3));
        let (done, flushed) = crossbeam_channel::bounded(1);
        if self.run_logs.send(crate::runbooks::LogJob::Flush(done)).is_ok() {
            let _ = flushed.recv_timeout(Duration::from_secs(3));
        }
        let mut handoff = self.handoff();
        handoff.connections = connections;
        let fds = handoff.fds();
        let path = handoff_path(&self.session);
        let written = serde_json::to_vec(&handoff).map_err(std::io::Error::other).and_then(|data| {
            crate::paths::ensure_private_dir(&crate::paths::state_dir())?;
            crate::paths::write_private(&path, &data)
        });
        if let Err(e) = written {
            let _ = std::fs::remove_file(&path);
            self.resume_panes();
            return format!("cannot write {}: {e}", path.display());
        }
        // The ssh that list remote sessions' agents would lose their pipes: the next server
        // starts them again.
        self.stop_remotes();
        self.remotes.clear();
        self.tell_restarting();
        let why = match fds.iter().try_for_each(|fd| set_inherited(*fd, true)) {
            Ok(()) => format!("could not run {}: {}", exe.display(), exec(exe, &self.session, &path)),
            Err(e) => format!("cannot keep the panes' terminals open: {e}"),
        };
        for fd in &fds {
            let _ = set_inherited(*fd, false);
        }
        let _ = std::fs::remove_file(&path);
        self.resume_panes();
        why
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

    fn handoff(&mut self) -> Handoff {
        let mut state = persist::snapshot(self, true);
        // Tunnels go with their ssh, below.
        state.tunnels.clear();
        let mut ids: Vec<PaneId> = self.panes.keys().copied().collect();
        ids.sort();
        let panes = ids.iter().filter_map(|id| self.panes.get_mut(id)?.live()).collect();
        let a = self.layout_area;
        Handoff {
            version: HANDOFF_VERSION,
            from: env!("CARGO_PKG_VERSION").into(),
            session: self.session.clone(),
            listener: self.listener_fd,
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
        }
    }

    /// Tell the attached clients the server restarts, and which workspace each shows, so that
    /// they come back to it; what they were sent is on its way before the exec drops them.
    fn tell_restarting(&mut self) {
        let ids: Vec<ClientId> = self.clients.values().filter(|c| c.attached).map(|c| c.id).collect();
        for id in ids {
            let ws = self.clients.get(&id).and_then(|c| self.ws_index(c.workspace));
            let workspace = ws.map(|wi| self.workspaces[wi].name.clone()).unwrap_or_default();
            // A client from before live restarts leaves, and says how to come back.
            let reason = "kraal restarted live: run kraal to attach again".to_string();
            self.send_msg(id, &ServerMsg::Exit { reason, reattach: Some(workspace), live: true });
        }
        let start = Instant::now();
        while self.clients.values().any(|c| !c.out.is_empty()) && start.elapsed() < Duration::from_millis(500) {
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(20));
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
        for t in h.tunnels {
            // SAFETY: the hand-off gives this process the descriptor, which nothing else owns.
            let stderr = t.stderr.map(|fd| unsafe { OwnedFd::from_raw_fd(fd) });
            self.adopt_tunnel(t, stderr);
        }
        let mut live: HashMap<PaneId, LivePane> = h.panes.into_iter().map(|p| (p.id, p)).collect();
        let mut kept = 0;
        // Nothing opens: the workspaces are the same ones, their events said already, and the
        // hooks run for them already.
        let hooks = std::mem::take(&mut self.hooks.list);
        persist::apply_layout(self, h.state.workspaces, h.state.revision, &mut |server, p| {
            match p.id.and_then(|id| live.remove(&id)) {
                Some(lp) => {
                    let adopted = server.adopt_pane(p, lp);
                    kept += usize::from(adopted.as_ref().is_ok_and(|(_, ok)| *ok));
                    adopted.map(|(id, _)| id)
                }
                // A preview or a file manager, opened again where it was.
                None if p.spec.preview.is_some() || p.spec.files.is_some() => match p.id {
                    Some(id) => server.spawn_pane_as(id, p.name.clone(), p.spec.clone()),
                    None => server.spawn_pane(p.name.clone(), p.spec.clone()),
                },
                None => persist::spawn(server, p),
            }
        });
        self.hooks.list = hooks;
        self.events = EventLog::restore(h.events);
        // What has no place any more (a finished transfer's window) goes: its PTY is closed,
        // and its program reaped.
        for (_, lp) in live {
            // SAFETY: as above.
            drop(unsafe { OwnedFd::from_raw_fd(lp.fd) });
            let pid = lp.pid as libc::pid_t;
            std::thread::spawn(move || {
                let mut status = 0;
                // SAFETY: waitpid on a child of ours writes `status`.
                unsafe { libc::waitpid(pid, &mut status, 0) };
            });
        }
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
            self.new_connection(unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) });
        }
        let text = format!("restarted live from kraal {}: {kept} panes kept running", h.from);
        eprintln!("kraal: {text}");
        self.log_message(&text, false);
    }

    /// Pane `p` of the state on the PTY and program `live` describes; started again as after a
    /// restart when they are not this process's to take (whether they were taken).
    fn adopt_pane(&mut self, p: &PaneSnap, live: LivePane) -> Result<(PaneId, bool)> {
        let id = live.id;
        // SAFETY: the hand-off gives this process the descriptor, which nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(live.fd) };
        // SAFETY: plain syscalls on a descriptor and a pid the hand-off names.
        let (tty, group) = unsafe { (libc::isatty(live.fd) == 1, libc::getpgid(live.pid as libc::pid_t)) };
        if !tty || !crate::exec::is_child(live.pid) || (group >= 0 && group != live.pgid) {
            drop(fd);
            self.warnings.push(format!("pane {id} could not be taken over: it starts again"));
            return persist::spawn(self, p).map(|id| (id, false));
        }
        let pending = live.suspended && live.spec.pending_agent.is_some();
        let pane = Pane::adopt(live, fd, p.name.clone(), &self.env, &self.tx)?;
        self.panes.insert(id, pane);
        if let Some(file) = &p.scrollback {
            self.scrollback.insert(id, SavedScrollback { file: Some(file.clone()), taken: Instant::now() });
        }
        if pending {
            self.resolve_pending_agent(id);
        }
        Ok((id, true))
    }
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
    fn an_exec_that_fails_returns_why() {
        let missing = std::env::temp_dir().join(format!("kraal-no-such-binary-{}", std::process::id()));
        let err = exec(&missing, "s", Path::new("/nonexistent/handoff.json"));
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
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
