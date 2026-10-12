//! The attaching client and the one-shot control connection used by CLI commands.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::protocol::{self, ClientMsg, Request, Response, ServerMsg};
use crate::{paths, process};

/// Make sure a server runs for `session`, starting one in the background if needed.
pub fn ensure_server(session: &str) -> Result<PathBuf> {
    paths::private_runtime_dir().context("checking the runtime directory")?;
    let socket = paths::socket_path(session);
    if UnixStream::connect(&socket).is_ok() {
        return Ok(socket);
    }
    let exe = installed_exe()?;
    let log = paths::log_path(session);
    // The state directory holds what the server logs and saves: only ours.
    if let Some(dir) = log.parent() {
        paths::ensure_private_dir(dir)?;
    }
    let out = crate::runbooks::open_private_append(&log)?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["server", "--session", session])
        .stdin(std::process::Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out)
        .current_dir(dirs::home_dir().unwrap_or_else(|| "/".into()));
    // Detach from the terminal so closing it does not take the server down.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("starting the kraal server")?;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if UnixStream::connect(&socket).is_ok() {
            return Ok(socket);
        }
        if let Ok(Some(status)) = child.try_wait() {
            bail!("the server exited ({status}); see {}", log.display());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    bail!("the server did not start in time; see {}", log.display())
}

/// Socket for control commands: `$KRAAL_SOCKET` inside a pane unless a session is named.
pub fn control_socket(session: Option<&str>) -> PathBuf {
    match session {
        Some(s) => paths::socket_path(s),
        None => std::env::var_os("KRAAL_SOCKET").map(PathBuf::from).unwrap_or_else(|| paths::socket_path("main")),
    }
}

/// Send one request and wait for its answer.
pub fn request(socket: &Path, req: Request) -> Result<Response> {
    if socket.starts_with(paths::runtime_dir()) {
        paths::private_runtime_dir().context("checking the runtime directory")?;
    }
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("no kraal server at {} (is it running?)", socket.display()))?;
    exchange(&mut stream, req)
}

fn exchange(stream: &mut UnixStream, req: Request) -> Result<Response> {
    protocol::write_json(stream, &ClientMsg::Request { version: protocol::PROTOCOL_VERSION, request: req })?;
    loop {
        match protocol::read_frame(stream)? {
            None => bail!("the server closed the connection"),
            Some((protocol::JSON, payload)) => {
                let msg: ServerMsg = serde_json::from_slice(&payload)?;
                match msg {
                    ServerMsg::Response { result } => return result.map_err(|e| anyhow::anyhow!(e)),
                    ServerMsg::Exit { reason, .. } => bail!("{reason}"),
                    ServerMsg::Open { .. }
                    | ServerMsg::Event { .. }
                    | ServerMsg::Events { .. }
                    | ServerMsg::Attached { .. }
                    | ServerMsg::Hold { .. } => continue,
                }
            }
            Some(_) => continue,
        }
    }
}

/// Ask the server behind `socket` to restart live on `binary` (see [`Request::Upgrade`]), or
/// with `check` whether it can now. `Ok` once it said yes, or once the connection closed as the
/// server replaced itself; why not otherwise.
pub fn upgrade(socket: &Path, check: bool, binary: Option<PathBuf>) -> Result<(), String> {
    let mut stream = UnixStream::connect(socket).map_err(|e| format!("its server does not answer ({e})"))?;
    let request =
        ClientMsg::Request { version: protocol::PROTOCOL_VERSION, request: Request::Upgrade { check, binary } };
    protocol::write_json(&mut stream, &request).map_err(|e| format!("its server does not answer ({e})"))?;
    loop {
        let payload = match protocol::read_frame(&mut stream) {
            Ok(None) | Err(_) if check => return Err("its server closed the connection".into()),
            Ok(None) | Err(_) => return Ok(()),
            Ok(Some((protocol::JSON, payload))) => payload,
            Ok(Some(_)) => continue,
        };
        match serde_json::from_slice::<ServerMsg>(&payload) {
            // A server from before live restarts does not know the request.
            Ok(ServerMsg::Response { result: Err(e) }) if e.starts_with("bad message") => {
                return Err("its server is a kraal from before live restarts".into());
            }
            Ok(ServerMsg::Response { result: Err(e) }) => return Err(e),
            Ok(ServerMsg::Response { result: Ok(_) }) => return Ok(()),
            Ok(ServerMsg::Exit { reason, .. }) => return Err(reason),
            _ => continue,
        }
    }
}

/// Whether a stopped server saved its layout.
#[derive(Debug, PartialEq)]
pub enum Saved {
    Yes,
    No(String),
    /// A server from another version, stopped by a signal, does not say.
    Unknown,
}

/// Stop the server behind `socket` and wait until it has exited. A server from another kraal
/// version refuses every request, so when asking fails it gets SIGTERM, which every version
/// handles the same way: save the layout, then exit.
pub fn kill_server(socket: &Path) -> Result<Saved> {
    stop_server(socket, false)
}

/// Restart the session's server on this kraal: the server saves the session and stops, telling
/// its attached clients to wait for the next one, which starts from this binary and restores
/// the session. Whether the layout was saved, as for `kraal kill`.
pub fn restart_server(session: &str) -> Result<Saved> {
    let saved = stop_server(&paths::socket_path(session), true)?;
    ensure_server(session)?;
    Ok(saved)
}

/// The pid and kraal release of the server behind `socket`, given up on quickly when it does
/// not answer.
pub fn ping(socket: &Path) -> Result<(u32, String)> {
    identify(socket).map(|i| (i.pid, i.version))
}

/// What a server says it is (see `Response::Pong`).
pub struct Identity {
    pub pid: u32,
    pub version: String,
    /// When the session's server started, when it says.
    pub boot: Option<u64>,
    /// What its last live restart took over, when it says.
    pub live: Option<protocol::LiveRestart>,
}

/// [`ping`], with what else the server says of itself.
pub fn identify(socket: &Path) -> Result<Identity> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    match exchange(&mut stream, Request::Ping)? {
        Response::Pong { pid, version, boot, live, .. } => Ok(Identity { pid, version, boot, live }),
        _ => bail!("unexpected answer from the server"),
    }
}

/// Where this kraal is installed, when it runs from elsewhere (a live restart runs a copy).
static INSTALLED: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Say where this kraal is installed (see [`installed_exe`]).
pub fn set_installed(path: PathBuf) {
    let _ = INSTALLED.set(path);
}

/// The kraal binary installed where this one was started from. Once an update has replaced the
/// file, Linux names the running one `<path> (deleted)`: the file now at `<path>` is meant.
pub fn installed_exe() -> Result<PathBuf> {
    if let Some(path) = INSTALLED.get() {
        if path.exists() {
            return Ok(path.clone());
        }
        bail!("{} is not there any more", path.display());
    }
    let exe = std::env::current_exe().context("locating the kraal binary")?;
    if exe.exists() {
        return Ok(exe);
    }
    let name = exe.to_string_lossy();
    match name.strip_suffix(" (deleted)").map(PathBuf::from) {
        Some(path) if path.exists() => Ok(path),
        _ => bail!("{name} is not there any more"),
    }
}

/// The kraal a job runs (a transfer, a command on servers, an attachment): the one installed
/// now, which an update may have put in place of the file this process started from; else the
/// `kraal` on PATH.
pub fn kraal_program() -> String {
    installed_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "kraal".into())
}

/// Start `kraal -s <session> restart --yes` from `exe` on its own, out of any terminal, pane and
/// session: it outlives the pane or the server that starts it. What it says goes to the
/// session's log. `--cold` only when `cold`: a kraal from before live restarts does not know
/// it (its restart is never live).
pub fn restart_detached(exe: &Path, session: &str, cold: bool) -> Result<std::process::Child> {
    let log = paths::log_path(session);
    if let Some(dir) = log.parent() {
        paths::ensure_private_dir(dir)?;
    }
    let out = crate::runbooks::open_private_append(&log)?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["-s", session, "restart", "--yes"])
        .args(cold.then_some("--cold"))
        .stdin(std::process::Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out)
        .current_dir(dirs::home_dir().unwrap_or_else(|| "/".into()));
    for var in ["KRAAL_PANE", "KRAAL_SOCKET", "KRAAL_SESSION", "KRAAL_SESSION_NAME"] {
        cmd.env_remove(var);
    }
    // SAFETY: setsid is async-signal-safe.
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().with_context(|| format!("starting {}", exe.display()))
}

/// Whether the restart [`restart_detached`] started is under way: it did not fail at once
/// (a kraal that does not know how), within `limit`. It is reaped once it ends.
pub fn restart_under_way(mut child: std::process::Child, session: &str, limit: Duration) -> Result<(), String> {
    let start = Instant::now();
    while start.elapsed() < limit {
        match child.try_wait() {
            Ok(Some(status)) if !status.success() => {
                let log = paths::log_path(session);
                return Err(format!("the restart did not happen ({status}); see {}", log.display()));
            }
            Ok(Some(_)) => return Ok(()),
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(_) => break,
        }
    }
    crate::exec::reap_later(child);
    Ok(())
}

fn stop_server(socket: &Path, restart: bool) -> Result<Saved> {
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("no kraal server at {} (is it running?)", socket.display()))?;
    let (pid, uid) = process::socket_peer(&stream).context("finding the server process")?;
    // SAFETY: getuid cannot fail.
    if uid != unsafe { libc::getuid() } {
        bail!("{} belongs to another user", socket.display());
    }
    let saved = match exchange(&mut stream, Request::KillServer { restart }) {
        Ok(Response::Text(why)) => Saved::No(why),
        Ok(_) => Saved::Yes,
        Err(_) => {
            // SAFETY: plain signal to the process listening on our own private socket.
            if unsafe { libc::kill(pid, libc::SIGTERM) } != 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() != Some(libc::ESRCH) {
                    return Err(err).context(format!("stopping the server (pid {pid})"));
                }
            }
            Saved::Unknown
        }
    };
    drop(stream);
    // The server waits up to 10 s for its layout to reach the disk.
    let start = Instant::now();
    while process::running(pid) {
        if start.elapsed() > Duration::from_secs(15) {
            bail!("the server (pid {pid}) is still running after 15 s");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(saved)
}

/// Stream events matching `filters` to `on_event` until the server goes away or it returns false.
/// `on_ready` runs once the server has started streaming.
pub fn subscribe(
    socket: &Path,
    filters: Vec<String>,
    since: Option<(u64, Option<u64>)>,
    on_ready: impl FnOnce(),
    mut on_event: impl FnMut(&serde_json::Value) -> bool,
) -> Result<()> {
    let mut on_ready = Some(on_ready);
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("no kraal server at {} (is it running?)", socket.display()))?;
    let request = Request::Subscribe { events: filters, since: since.map(|s| s.0), boot: since.and_then(|s| s.1) };
    protocol::write_json(&mut stream, &ClientMsg::Request { version: protocol::PROTOCOL_VERSION, request })?;
    loop {
        let Some((kind, payload)) = protocol::read_frame(&mut stream)? else { return Ok(()) };
        if kind != protocol::JSON {
            continue;
        }
        match serde_json::from_slice::<ServerMsg>(&payload)? {
            ServerMsg::Response { result: Err(e) } => bail!(e),
            ServerMsg::Response { result: Ok(_) } => {
                if let Some(f) = on_ready.take() {
                    f();
                }
            }
            ServerMsg::Event { event } => {
                if !on_event(&event) {
                    return Ok(());
                }
            }
            ServerMsg::Events { events } => {
                for event in &events {
                    if !on_event(event) {
                        return Ok(());
                    }
                }
            }
            ServerMsg::Exit { reason, .. } => bail!("{reason}"),
            _ => {}
        }
    }
}

/// Like [`request`] but gives up quickly; used by hooks, which must never slow the agent down.
pub fn request_quick(socket: &Path, req: Request) -> Result<()> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
    stream.set_read_timeout(Some(Duration::from_millis(1000)))?;
    protocol::write_json(&mut stream, &ClientMsg::Request { version: protocol::PROTOCOL_VERSION, request: req })?;
    let _ = protocol::read_frame(&mut stream);
    Ok(())
}

struct TerminalGuard;

const SETUP_NO_MOUSE: &str = "\x1b[?1049h\x1b[?25l";
// `CSI > 5 u` asks the terminal for unambiguous keys (kitty keyboard protocol), so Shift+Enter,
// Ctrl+Enter or Esc arrive distinct, and for their alternates, so a key with Ctrl or Alt also
// says where it is on the keyboard; typed text still comes as text. Terminals without the
// protocol ignore it. The server raises it for the leader menu by setting this same entry of the
// terminal's stack, which the single pop when the client goes takes away.
const KEYS_ON: &str = "\x1b[?2004h\x1b[?1004h\x1b[>5u";
const KEYS_OFF: &str = "\x1b[<u\x1b[?1004l\x1b[?2004l";
pub(crate) const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
pub(crate) const MOUSE_OFF: &str = "\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l";

/// Turn the terminal's input modes on (kitty keys, bracketed paste, focus reports, the mouse
/// when asked) while a server answers, and off while kraal waits for one.
fn set_modes(on: bool, mouse: bool) {
    let mut out = std::io::stdout();
    let _ = out.write_all(if on { KEYS_ON } else { KEYS_OFF }.as_bytes());
    // Off whatever the setting was when attaching: the server turns the mouse on or off when its
    // configuration is read again.
    if mouse || !on {
        let _ = out.write_all(if on { MOUSE_ON } else { MOUSE_OFF }.as_bytes());
    }
    let _ = out.flush();
}

/// Whether `keys` hold ctrl+c, as a terminal sends it with or without the kitty protocol, and on
/// a non-Latin layout (the key at the place of `c`).
fn is_ctrl_c(keys: &[u8]) -> bool {
    let mut parser = crate::input::InputParser::new();
    let mut events = parser.feed(keys);
    events.extend(parser.flush());
    let c = crate::input::KeyCode::Char('c');
    events.iter().any(|e| {
        matches!(e, crate::input::InputEvent::Key(k)
            if k.mods.ctrl && !k.mods.alt && (k.code == c || k.by_place().is_some_and(|p| p.code == c)))
    })
}
const RESTORE: &str = "\x1b[?2026l\x1b[<u\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l\x1b[?1004l\x1b[?2004l\x1b[0m\x1b[0 q\x1b[?25h\x1b[?1049l";

/// What is held of what is typed during a live restart, about at most: past that, the keyboard
/// is not read until it is sent (the terminal keeps the rest).
const MAX_HELD_KEYS: usize = 16 << 20;
/// How long an escape alone waits for the rest of a sequence, as the server waits for it.
const ESCAPE_ALONE: Duration = Duration::from_millis(50);

/// What is typed while a live restart is prepared: kept from a point where the server has every
/// key and paste whole, and sent once it says (to the same server, or the next one).
#[derive(Default)]
struct Holding {
    /// The server asked: what is typed is kept from the next point between two keys.
    asked: bool,
    /// Which time it asked, said in the answer.
    attempt: u32,
    /// The pane what is kept is typed for (the server says which): it goes there or nowhere.
    target: Option<u32>,
    /// What is kept, since the server was told it is.
    kept: Option<Vec<u8>>,
    /// What was sent, read as the server reads it: whether it ends between two keys.
    sent: crate::input::InputParser,
    /// When keys last came.
    last: Option<Instant>,
}

impl Holding {
    /// `keys` were typed (none: nothing came for a moment). What goes to the server now, and
    /// whether to tell it that what is typed is held from there on.
    fn typed(&mut self, keys: &[u8], now: Instant) -> (Vec<u8>, bool) {
        if let Some(kept) = &mut self.kept {
            kept.extend_from_slice(keys);
            return (Vec::new(), false);
        }
        if !keys.is_empty() {
            self.last = Some(now);
        }
        if !self.asked {
            let _ = self.sent.feed(keys);
            return (keys.to_vec(), false);
        }
        let mut at = self.sent.idle().then_some(0);
        if at.is_none() {
            for (i, b) in keys.iter().enumerate() {
                let _ = self.sent.feed(std::slice::from_ref(b));
                if self.sent.idle() {
                    at = Some(i + 1);
                    break;
                }
            }
        }
        // An escape alone for a moment is the Escape key (a paste goes on until its end).
        if at.is_none() && self.last.is_none_or(|t| now >= t + ESCAPE_ALONE) {
            let _ = self.sent.flush();
            at = self.sent.idle().then_some(keys.len());
        }
        let Some(at) = at else { return (keys.to_vec(), false) };
        self.kept = Some(keys[at..].to_vec());
        (keys[..at].to_vec(), true)
    }

    /// The same server needs nothing held any more: what was kept, to send.
    fn release(&mut self) -> Vec<u8> {
        self.asked = false;
        self.target = None;
        let kept = self.kept.take().unwrap_or_default();
        let _ = self.sent.feed(&kept);
        kept
    }

    /// Connecting (again): the server reads afresh. What to tell it: the pane what is kept was
    /// typed for, and how much there is.
    fn reattach(&mut self) -> Option<(u32, u64)> {
        self.sent = crate::input::InputParser::new();
        self.asked = false;
        let kept = self.kept.as_ref().map_or(0, |k| k.len() as u64);
        self.target.map(|pane| (pane, kept))
    }

    /// The server answered a connection (`shown`: the pane it shows of the one asked for, ready
    /// to take what is typed). What was kept goes now if that is its pane; otherwise it is
    /// dropped (the server says so), never sent to another pane.
    fn settle(&mut self, shown: Option<u32>) -> Vec<u8> {
        let fits = self.target.is_some() && shown == self.target;
        match fits {
            true => self.release(),
            false => {
                self.kept = None;
                self.release()
            }
        }
    }

    /// Whether more may be read from the keyboard: once full, nothing is until what is kept
    /// is sent.
    fn has_room(&self) -> bool {
        self.kept.as_ref().is_none_or(|k| k.len() < MAX_HELD_KEYS)
    }
}

type Writer = Mutex<Option<Box<dyn Write + Send>>>;

/// Send what was typed (`keys`, from the keyboard) to the server, or keep it (see [`Holding`]).
/// While disconnected, ctrl+c gives up (`quit`).
fn forward_keys(keys: &[u8], writer: &Writer, holding: &Mutex<Holding>, quit: &AtomicBool) {
    let Ok(mut w) = writer.lock() else { return };
    if w.is_none() && is_ctrl_c(keys) {
        quit.store(true, Ordering::SeqCst);
        return;
    }
    let Ok(mut h) = holding.lock() else { return };
    let (send, held) = h.typed(keys, Instant::now());
    let attempt = h.attempt;
    drop(h);
    if let Some(w) = w.as_mut() {
        if !send.is_empty() {
            let _ = protocol::write_frame(w, protocol::BYTES, &send);
        }
        if held {
            let _ = protocol::write_json(w, &ClientMsg::Held { attempt });
        }
    }
}

/// The server asks to hold what is typed (`attempt`, for `pane`): from the next point between
/// two keys, said once reached (now, or as keys come).
fn hold_keys(writer: &Writer, holding: &Mutex<Holding>, attempt: u32, pane: Option<u32>) {
    let Ok(mut w) = writer.lock() else { return };
    let Ok(mut h) = holding.lock() else { return };
    h.asked = true;
    h.attempt = attempt;
    h.target = pane;
    // Held already: said again.
    let (send, held) = match h.kept {
        Some(_) => (Vec::new(), true),
        None => h.typed(&[], Instant::now()),
    };
    drop(h);
    if let Some(w) = w.as_mut() {
        if !send.is_empty() {
            let _ = protocol::write_frame(w, protocol::BYTES, &send);
        }
        if held {
            let _ = protocol::write_json(w, &ClientMsg::Held { attempt });
        }
    }
}

/// Send what was held to the server: the same one, told it holds nothing any more, or one just
/// connected to, which said what it shows (`shown`, see [`Holding::settle`]).
fn release_keys(writer: &Writer, holding: &Mutex<Holding>, shown: Option<Option<u32>>) {
    let Ok(mut w) = writer.lock() else { return };
    let Ok(mut h) = holding.lock() else { return };
    let kept = match shown {
        Some(shown) => h.settle(shown),
        None => h.release(),
    };
    drop(h);
    if let Some(w) = w.as_mut()
        && !kept.is_empty()
    {
        let _ = protocol::write_frame(w, protocol::BYTES, &kept);
    }
}

/// Set when the client is going: the output loop writes nothing more.
static LEAVING: AtomicBool = AtomicBool::new(false);
/// The ssh process of a remote attachment, to stop with the client.
static SSH_PID: AtomicU32 = AtomicU32::new(0);

/// Give the terminal back as it was: line mode first (that cannot block), then the reset
/// sequences, written to the descriptor directly (on a signal, the output loop may hold the
/// stdout lock).
fn restore_terminal() {
    let _ = crossterm::terminal::disable_raw_mode();
    let mut out: &[u8] = RESTORE.as_bytes();
    while !out.is_empty() {
        // SAFETY: writing a valid buffer to stdout's descriptor.
        let n = unsafe { libc::write(1, out.as_ptr().cast(), out.len()) };
        if n <= 0 {
            break;
        }
        out = &out[n as usize..];
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = std::io::stdout().flush();
        restore_terminal();
    }
}

/// Where to attach: this machine's server, or a server running on another machine over SSH.
#[derive(Debug, Clone)]
pub enum Target {
    Local {
        session: String,
    },
    /// `ssh` arguments up to and including the destination.
    Remote {
        ssh: Vec<String>,
        session: String,
        label: String,
        /// A pane of that session to show on the first connection (an id or a name).
        pane: Option<String>,
    },
}

/// The command line that runs `kraal ARGS` on a server: the `kraal` in its `PATH`, else the one
/// `kraal remote-install` puts in `~/.local/bin`. It goes through sh: the login shell on the
/// server may not be a POSIX one. `args` must be quoted already.
pub fn remote_kraal(args: &str) -> String {
    crate::shell::sh_c(&format!("k=$(command -v kraal || echo \"$HOME/.local/bin/kraal\"); exec \"$k\" {args}"))
}

struct Conn {
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    child: Option<std::process::Child>,
    /// The server's process, when it runs on this machine.
    server: Option<libc::pid_t>,
    /// The last thing ssh said on stderr (host key, authentication, remote errors).
    ssh_said: Arc<Mutex<String>>,
}

impl Conn {
    /// `what` went wrong, with ssh's last words if it said something: ssh is given a moment to
    /// end and say them (a refused key, an unknown host).
    fn why_lost(&mut self, what: &str) -> String {
        if let Some(child) = &mut self.child {
            let start = Instant::now();
            while matches!(child.try_wait(), Ok(None)) && start.elapsed() < Duration::from_secs(1) {
                std::thread::sleep(Duration::from_millis(20));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let said = self.ssh_said.lock().map(|s| s.trim().to_string()).unwrap_or_default();
        if said.is_empty() { what.to_string() } else { format!("{what}: {said}") }
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        if let Some(c) = &mut self.child {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn connect(target: &Target) -> Result<Conn> {
    match target {
        Target::Local { session } => {
            let socket = ensure_server(session)?;
            let s = UnixStream::connect(&socket)?;
            let server = process::socket_peer(&s).ok().map(|(pid, _)| pid);
            Ok(Conn {
                reader: Box::new(s.try_clone()?),
                writer: Box::new(s),
                child: None,
                server,
                ssh_said: Default::default(),
            })
        }
        Target::Remote { ssh, session, .. } => {
            // The session name is validated (letters, digits, - _ .), so it needs no quoting.
            let remote = remote_kraal(&format!("proxy -s {session}"));
            let mut child = std::process::Command::new("ssh")
                .args(ssh)
                .arg(remote)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .context("starting ssh")?;
            SSH_PID.store(child.id(), Ordering::SeqCst);
            let reader = Box::new(child.stdout.take().context("ssh stdout")?);
            let writer = Box::new(child.stdin.take().context("ssh stdin")?);
            // Keep ssh's last error line to explain a lost connection (it would garble the
            // screen if it went to the terminal).
            let ssh_said = match child.stderr.take() {
                Some(stderr) => crate::exec::last_line(stderr),
                None => Default::default(),
            };
            Ok(Conn { reader, writer, child: Some(child), server: None, ssh_said })
        }
    }
}

/// Write a one-line status over the bottom of the screen (used while reconnecting).
fn status_line(msg: &str) {
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    // Leave the last column empty: writing there would scroll the screen.
    let text: String = msg.chars().take((cols as usize).saturating_sub(2)).collect();
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b7\x1b[?7l\x1b[{rows};1H\x1b[0m\x1b[41;97m {text}\x1b[K\x1b[0m\x1b[?7h\x1b8");
    let _ = out.flush();
}

/// A connection the server answered.
struct Greeted {
    conn: Conn,
    frames: std::io::BufReader<Box<dyn Read + Send>>,
    /// The server's first frame, still to be shown.
    first: (u8, Vec<u8>),
}

/// Connect, say hello, and wait for the server's first answer (returned, still to be shown).
/// Until then, over ssh, ssh has the terminal: it may ask for a password or about a host key,
/// and the terminal is in its usual mode, where ctrl+c ends kraal. `Err` says why it failed.
/// `typed_for`: after a live restart, the pane what is held was typed for, and how much there
/// is (see [`Holding::reattach`]).
fn greet(target: &Target, workspace: &Option<String>, typed_for: Option<(u32, u64)>) -> Result<Greeted, String> {
    let mut conn = connect(target).map_err(|e| format!("{e:#}"))?;
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    // The workspace asked for is asked again after a reconnection; after a live restart, the
    // pane shows instead.
    let hello = ClientMsg::Attach {
        version: protocol::PROTOCOL_VERSION,
        cols,
        rows,
        workspace: workspace.clone().filter(|_| typed_for.is_none()),
        remote: matches!(target, Target::Remote { .. }),
        pane: match target {
            Target::Remote { pane, .. } => pane.clone(),
            Target::Local { .. } => None,
        },
        // A local client is the kraal installed on the server's machine; a remote one may not be.
        installed: matches!(target, Target::Local { .. }).then(|| protocol::KRAAL_VERSION.to_string()),
        follows_live: true,
        typed_for: typed_for.map(|(pane, _)| pane),
        typed: typed_for.map_or(0, |(_, typed)| typed),
    };
    if protocol::write_json(&mut conn.writer, &hello).is_err() {
        return Err(conn.why_lost("could not talk to the server"));
    }
    let mut frames =
        std::io::BufReader::with_capacity(256 * 1024, std::mem::replace(&mut conn.reader, Box::new(std::io::empty())));
    match protocol::read_frame(&mut frames) {
        Ok(Some(first)) => Ok(Greeted { conn, frames, first }),
        Ok(None) => Err(conn.why_lost("the connection closed")),
        Err(e) => Err(conn.why_lost(&format!("connection error: {e}"))),
    }
}

/// Whether keyboard reading is paused (ssh is asking something in the terminal). Each pause has
/// a number, and the reader says which pause it has seen: `pause` waits for that very one, so a
/// reader still busy with an earlier read is never taken for a stopped one.
#[derive(Default)]
struct KeysPause {
    paused: AtomicBool,
    epoch: AtomicU64,
    parked: AtomicU64,
}

impl KeysPause {
    /// Stop reading the keyboard; `true` once the reader has stopped, so what is typed next (a
    /// password for ssh) is never read as keys for kraal. The reader only reads what is waiting
    /// and never blocks, so it stops within its poll interval; `false` means it did not, and the
    /// terminal must not be handed over.
    fn pause(&self) -> bool {
        // Paused before the number changes: a reader that sees the new number sees the pause.
        self.paused.store(true, Ordering::SeqCst);
        let epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        let start = Instant::now();
        while self.parked.load(Ordering::SeqCst) != epoch {
            if start.elapsed() > Duration::from_secs(5) {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        true
    }

    fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
    }
}

/// Read `fd` (the keyboard) into `forward` until it ends; `forward` gets nothing when nothing
/// came for a moment. Nothing is read while paused: the reader parks before it would read, and
/// it reads only the bytes already waiting, so a read never blocks it past a pause. Nothing is
/// read either while there is no `room` (the terminal keeps what is typed meanwhile).
fn read_keys(
    fd: std::os::unix::io::RawFd,
    pause: &KeysPause,
    mut room: impl FnMut() -> bool,
    mut forward: impl FnMut(&[u8]),
) {
    let mut buf = [0u8; 16 * 1024];
    loop {
        if pause.paused.load(Ordering::SeqCst) {
            pause.parked.store(pause.epoch.load(Ordering::SeqCst), Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        if !room() {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        }
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd.
        let ready = unsafe { libc::poll(&mut pfd, 1, 100) };
        if ready < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return;
        }
        if ready <= 0 || pause.paused.load(Ordering::SeqCst) {
            if ready == 0 && !pause.paused.load(Ordering::SeqCst) {
                forward(&[]);
            }
            continue;
        }
        let mut waiting: libc::c_int = 0;
        // SAFETY: FIONREAD writes the number of bytes waiting into `waiting`.
        if unsafe { libc::ioctl(fd, libc::FIONREAD, &mut waiting) } != 0 {
            return;
        }
        if waiting <= 0 {
            // Ready with nothing to read: the end (a closed pipe, a hung-up terminal).
            if pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
                return;
            }
            continue;
        }
        let want = (waiting as usize).min(buf.len());
        // SAFETY: reading into a buffer we own, at most its length.
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), want) };
        match n {
            0 => return,
            n if n < 0 => {
                if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                    return;
                }
            }
            n => forward(&buf[..n as usize]),
        }
    }
}

/// Attach the current terminal to a session. Remote sessions reconnect by themselves when the
/// connection drops; their programs keep running on the server meanwhile. `attached` runs once
/// the session first answered, before the terminal changes.
pub fn attach(mut target: Target, mut workspace: Option<String>, mouse: bool, attached: impl FnOnce()) -> Result<()> {
    if std::env::var_os("KRAAL_PANE").is_some() {
        bail!("already inside kraal (unset KRAAL_PANE to nest anyway)");
    }
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        bail!("kraal needs a terminal");
    }
    // Fail before touching the terminal when the first connection is impossible; ssh asks what
    // it has to ask in the terminal as it is.
    let first = greet(&target, &workspace, None).map_err(anyhow::Error::msg)?;
    // The pane asked for is shown once: a reconnection leaves the session where it is.
    if let Target::Remote { pane, .. } = &mut target {
        *pane = None;
    }
    attached();

    // Being told to go (terminal closed, kill): leave the terminal usable, as a normal exit
    // does (destructors do not run on a signal). Set up before the terminal changes.
    {
        use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
        let mut signals = signal_hook::iterator::Signals::new([SIGTERM, SIGHUP, SIGINT, SIGQUIT])?;
        std::thread::Builder::new().name("signals".into()).spawn(move || {
            if let Some(sig) = signals.forever().next() {
                LEAVING.store(true, Ordering::SeqCst);
                let pid = SSH_PID.load(Ordering::SeqCst);
                if pid != 0 {
                    // SAFETY: signalling the ssh child this client started.
                    unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
                }
                // CAN ends an escape sequence the output loop may have left half written.
                // SAFETY: one byte to stdout's descriptor.
                unsafe { libc::write(1, b"\x18".as_ptr().cast(), 1) };
                restore_terminal();
                std::process::exit(128 + sig);
            }
        })?;
    }

    crossterm::terminal::enable_raw_mode().context("switching the terminal to raw mode")?;
    let guard = TerminalGuard;
    {
        let mut out = std::io::stdout();
        out.write_all(SETUP_NO_MOUSE.as_bytes())?;
        out.flush()?;
    }
    set_modes(true, mouse);

    // The current connection's writer; None while reconnecting.
    let writer: Arc<Writer> = Arc::new(Mutex::new(None));
    let quit = Arc::new(AtomicBool::new(false));
    let pause = Arc::new(KeysPause::default());
    // What is typed while the server replaces itself (a live restart), for the next one.
    let holding: Arc<Mutex<Holding>> = Arc::default();

    // Keyboard → server. While disconnected, ctrl+c gives up.
    {
        let (writer, quit, pause, holding) = (writer.clone(), quit.clone(), pause.clone(), holding.clone());
        std::thread::Builder::new().name("stdin".into()).spawn(move || {
            let mut told = false;
            let room = || {
                let room = holding.lock().is_ok_and(|h| h.has_room());
                // Said once: nothing typed is lost.
                if !room && !std::mem::replace(&mut told, true) {
                    status_line("kraal: what you type is kept, and sent once the session is back");
                }
                told &= !room;
                room
            };
            read_keys(0, &pause, room, |keys| forward_keys(keys, &writer, &holding, &quit));
        })?;
    }
    // Window size changes → server.
    {
        let writer = writer.clone();
        let mut signals = signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH])?;
        std::thread::Builder::new().name("winch".into()).spawn(move || {
            for _ in signals.forever() {
                if let Ok((cols, rows)) = crossterm::terminal::size()
                    && let Ok(mut w) = writer.lock()
                    && let Some(w) = w.as_mut()
                {
                    let _ = protocol::write_json(w, &ClientMsg::Resize { cols, rows });
                }
            }
        })?;
    }

    let mut greeted = Some(first);
    let mut delay = Duration::from_secs(1);
    // What the server said it runs, the server that said it is restarting, and when it said it
    // replaces itself in place (a live restart: the connection goes on until it happens).
    let mut server_version = None;
    let mut restarting = None;
    let mut live_restart: Option<Instant> = None;
    let reason = 'session: loop {
        let session = match greeted.take() {
            Some(s) => Ok(s),
            None => {
                // ssh has the terminal until the server answers (a password, a host key): kraal
                // reads nothing, and ctrl+c (a signal again) ends it. The keyboard modes are off
                // already (see below).
                if pause.pause() {
                    let _ = crossterm::terminal::disable_raw_mode();
                    let typed_for = holding.lock().ok().and_then(|mut h| h.reattach());
                    let greeted = greet(&target, &workspace, typed_for);
                    let _ = crossterm::terminal::enable_raw_mode();
                    if greeted.is_ok() {
                        set_modes(true, mouse);
                    }
                    pause.resume();
                    greeted
                } else {
                    // ssh would compete with kraal for what is typed: not this time.
                    pause.resume();
                    Err("the keyboard could not be handed to ssh".to_string())
                }
            }
        };
        let lost = match session {
            Ok(Greeted { conn: mut c, mut frames, first }) => {
                *writer.lock().unwrap() = Some(std::mem::replace(&mut c.writer, Box::new(std::io::sink())));
                // What was held goes to the pane it was typed for, if this server shows it.
                let shown = match serde_json::from_slice::<ServerMsg>(&first.1) {
                    Ok(ServerMsg::Attached { shown, .. }) if first.0 == protocol::JSON => shown,
                    _ => None,
                };
                release_keys(&writer, &holding, Some(shown));
                delay = Duration::from_secs(1);
                let mut out = std::io::stdout().lock();
                let mut next = Some(Ok(Some(first)));
                let lost = loop {
                    match next.take().unwrap_or_else(|| protocol::read_frame(&mut frames)) {
                        Ok(Some((protocol::BYTES, bytes))) => {
                            if LEAVING.load(Ordering::SeqCst) {
                                break 'session "leaving".to_string();
                            }
                            if out.write_all(&bytes).and_then(|_| out.flush()).is_err() {
                                break 'session "output closed".to_string();
                            }
                        }
                        Ok(Some((_, payload))) => match serde_json::from_slice::<ServerMsg>(&payload) {
                            Ok(ServerMsg::Exit { reason, reattach: None, .. }) => break 'session reason,
                            // The next server shows the pane focused (see `Holding::target`): a
                            // workspace named would be opened again from its definition, should
                            // none of its panes have been taken over.
                            Ok(ServerMsg::Exit { live: true, .. }) => {
                                workspace = None;
                                live_restart = Some(Instant::now());
                            }
                            Ok(ServerMsg::Exit { reattach: Some(ws), .. }) => {
                                workspace = Some(ws).filter(|w| !w.is_empty());
                                restarting = Some(c.server);
                                break "the session restarts".to_string();
                            }
                            Ok(ServerMsg::Attached { version, .. }) => server_version = Some(version),
                            Ok(ServerMsg::Hold { on: true, attempt, pane }) => {
                                hold_keys(&writer, &holding, attempt, pane)
                            }
                            Ok(ServerMsg::Hold { on: false, .. }) => release_keys(&writer, &holding, None),
                            Ok(ServerMsg::Open { url }) => {
                                let _ = crate::pane::open_url(&url);
                            }
                            _ => {}
                        },
                        Ok(None) => break c.why_lost("the connection closed"),
                        Err(e) => break c.why_lost(&format!("connection error: {e}")),
                    }
                };
                *writer.lock().unwrap() = None;
                lost
            }
            Err(lost) => lost,
        };
        // After a live restart, the same server answers again: no other process to wait for.
        let live = live_restart.take().is_some_and(|at| at.elapsed() < Duration::from_secs(30));
        if let (Some(old), Target::Local { session }) = (restarting.take().or(live.then_some(None)), &target) {
            set_modes(false, mouse);
            match wait_restarted(session, old, &quit) {
                Ok(()) => continue 'session,
                Err(why) => break 'session why,
            }
        }
        let Target::Remote { label, .. } = &target else {
            break 'session format!("the server went away ({lost})");
        };
        // Until a server answers again, keys are plain (ctrl+c is ^C, and a signal while ssh
        // has the terminal), and neither the mouse nor focus changes type into a password.
        set_modes(false, mouse);
        // Remote: wait and try again, until it works or the user gives up.
        let until = Instant::now() + delay;
        while Instant::now() < until {
            if quit.load(Ordering::SeqCst) {
                break 'session format!("gave up reconnecting to {label}");
            }
            let left = until.saturating_duration_since(Instant::now()).as_secs() + 1;
            status_line(&format!(
                "kraal: connection to {label} lost ({lost}). Reconnecting in {left}s… ctrl+c to quit"
            ));
            std::thread::sleep(Duration::from_millis(200));
        }
        status_line(&format!("kraal: reconnecting to {label}… (ssh may ask for a password; ctrl+c to quit)"));
        delay = (delay * 2).min(Duration::from_secs(15));
    };
    drop(guard);
    eprintln!("[kraal: {reason}]");
    if let Target::Local { session } = &target {
        // A session left running on an older kraal is said once more, here.
        let socket = paths::socket_path(session);
        let running = server_version
            .filter(|_| UnixStream::connect(&socket).is_ok())
            .or_else(|| ping(&socket).ok().map(|(_, v)| v));
        if let Some(older) = running.filter(|v| protocol::newer(protocol::KRAAL_VERSION, v)) {
            eprintln!(
                "kraal {} is installed; session {session} runs {older}: `kraal -s {session} restart` restarts it",
                protocol::KRAAL_VERSION
            );
        }
    }
    Ok(())
}

/// Wait until the server that is restarting (`old`) has gone and the next one listens, saying
/// so on the bottom line; ctrl+c gives up.
fn wait_restarted(session: &str, old: Option<libc::pid_t>, quit: &AtomicBool) -> Result<(), String> {
    let start = Instant::now();
    loop {
        if quit.load(Ordering::SeqCst) {
            return Err("gave up waiting for the session to restart".into());
        }
        let gone = old.is_none_or(|pid| !process::running(pid));
        if gone && UnixStream::connect(paths::socket_path(session)).is_ok() {
            return Ok(());
        }
        if start.elapsed() > Duration::from_secs(60) {
            let log = paths::log_path(session);
            return Err(format!("the session did not come back after its restart; see {}", log.display()));
        }
        status_line("kraal: the session restarts… ctrl+c to quit");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `kraal proxy`: relay a client's frames between stdin/stdout and the local session socket.
/// Remote attach runs it on the server through ssh.
pub fn proxy(session: &str) -> Result<()> {
    let socket = ensure_server(session)?;
    let stream = UnixStream::connect(&socket)?;
    let mut to_server = stream.try_clone()?;
    let mut from_server = stream;
    let up = std::thread::spawn(move || {
        let _ = std::io::copy(&mut std::io::stdin().lock(), &mut to_server);
        let _ = to_server.shutdown(std::net::Shutdown::Write);
    });
    let mut out = std::io::stdout().lock();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match from_server.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if out.write_all(&buf[..n]).and_then(|_| out.flush()).is_err() {
                    break;
                }
            }
        }
    }
    drop(up);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_c_is_seen_with_or_without_the_kitty_protocol() {
        assert!(is_ctrl_c(b"\x03"));
        assert!(is_ctrl_c(b"\x1b[99;5u"));
        assert!(is_ctrl_c(b"ab\x1b[99;5:1u"));
        assert!(!is_ctrl_c(b"c"));
        assert!(!is_ctrl_c(b"\x1b[99;7u"), "ctrl+alt+c is something else");
        // A Russian layout: the key at the place of `c` types `с`.
        assert!(is_ctrl_c(b"\x1b[1089::99;5u"));
    }

    #[test]
    fn what_is_typed_is_held_from_a_point_between_two_keys() {
        let t0 = Instant::now();
        let later = |ms| t0 + Duration::from_millis(ms);
        let mut h = Holding::default();
        assert_eq!(h.typed(b"ab\x1b[", t0), (b"ab\x1b[".to_vec(), false));
        // Asked in the middle of a key: its end goes, and what follows is held.
        h.asked = true;
        assert_eq!(h.typed(b"Axyz", t0), (b"A".to_vec(), true));
        assert_eq!(h.typed(b"more", t0), (Vec::new(), false));
        assert_eq!(h.release(), b"xyzmore");
        assert_eq!(h.typed(b"q", t0), (b"q".to_vec(), false));
        // Asked between two keys: held at once.
        h.asked = true;
        assert_eq!(h.typed(&[], t0), (Vec::new(), true));
        assert_eq!(h.typed(b"k", t0), (Vec::new(), false));
        assert_eq!(h.release(), b"k");
        // A paste goes on until its end, however long it pauses.
        assert_eq!(h.typed(b"\x1b[200~line\r", t0), (b"\x1b[200~line\r".to_vec(), false));
        h.asked = true;
        assert_eq!(h.typed(b"more\r", later(1000)), (b"more\r".to_vec(), false));
        assert_eq!(h.typed(&[], later(2000)), (Vec::new(), false));
        assert_eq!(h.typed(b"end\x1b[201~after", later(2000)), (b"end\x1b[201~".to_vec(), true));
        assert_eq!(h.release(), b"after");
        // An escape alone is the Escape key once nothing follows for a moment.
        assert_eq!(h.typed(b"\x1b", t0), (b"\x1b".to_vec(), false));
        h.asked = true;
        assert_eq!(h.typed(&[], later(10)), (Vec::new(), false), "the rest of a sequence may come");
        assert_eq!(h.typed(&[], later(60)), (Vec::new(), true));
        assert!(h.release().is_empty());
    }

    /// A full hold stops reading the keyboard (the terminal keeps what is typed meanwhile), and
    /// the reader still parks when asked, so the connection can be made again: once what is
    /// held is sent, the rest is read, and nothing is lost.
    #[test]
    fn a_full_hold_stops_reading_the_keyboard_and_still_parks() {
        use std::os::unix::io::AsRawFd;
        #[derive(Clone, Default)]
        struct Sent(Arc<Mutex<Vec<u8>>>);
        impl Write for Sent {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(b);
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let sent = Sent::default();
        let writer: Arc<Writer> = Arc::new(Mutex::new(Some(Box::new(sent.clone()))));
        let holding: Arc<Mutex<Holding>> = Arc::default();
        {
            let mut h = holding.lock().unwrap();
            h.asked = true;
            h.kept = Some(vec![b'x'; MAX_HELD_KEYS]);
        }
        let (keyboard_end, mut typing) = std::os::unix::net::UnixStream::pair().unwrap();
        let keyboard = keyboard_end.as_raw_fd();
        let pause = Arc::new(KeysPause::default());
        let reader = {
            let (writer, holding, pause) = (writer.clone(), holding.clone(), pause.clone());
            std::thread::spawn(move || {
                let quit = AtomicBool::new(false);
                read_keys(
                    keyboard,
                    &pause,
                    || holding.lock().unwrap().has_room(),
                    |k| forward_keys(k, &writer, &holding, &quit),
                )
            })
        };
        typing.write_all(b"later").unwrap();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(holding.lock().unwrap().kept.as_ref().map(Vec::len), Some(MAX_HELD_KEYS));
        assert!(pause.pause(), "the reader did not park");
        pause.resume();
        release_keys(&writer, &holding, None);
        let start = Instant::now();
        while !sent.0.lock().unwrap().ends_with(b"later") {
            assert!(start.elapsed() < Duration::from_secs(5), "the rest was not read");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(sent.0.lock().unwrap().len() > MAX_HELD_KEYS);
        drop(typing);
        reader.join().unwrap();
        drop(keyboard_end);
    }

    /// After a live restart what was held goes to the pane it was typed for, once the server
    /// shows that pane; otherwise it is dropped, never sent elsewhere.
    #[test]
    fn what_was_held_goes_to_its_pane_or_nowhere() {
        let mut h = Holding { asked: true, target: Some(7), kept: Some(b"ls\r".to_vec()), ..Default::default() };
        assert_eq!(h.reattach(), Some((7, 3)));
        assert_eq!(h.settle(Some(7)), b"ls\r");
        let mut h = Holding { asked: true, target: Some(7), kept: Some(b"ls\r".to_vec()), ..Default::default() };
        assert_eq!(h.reattach(), Some((7, 3)));
        assert!(h.settle(Some(8)).is_empty(), "another pane");
        let mut h = Holding { asked: true, target: Some(7), kept: Some(b"ls\r".to_vec()), ..Default::default() };
        assert!(h.settle(None).is_empty(), "not there");
        assert!(h.kept.is_none() && h.target.is_none() && !h.asked);
    }

    #[test]
    fn nothing_typed_after_a_pause_is_read() {
        use std::os::unix::io::AsRawFd;
        // A socket pair stands in for the keyboard (closed on exec, unlike a bare pipe: a child
        // another test starts would keep it open).
        let (keyboard_end, mut typing) = std::os::unix::net::UnixStream::pair().unwrap();
        let keyboard = keyboard_end.as_raw_fd();
        let mut write = |bytes: &[u8]| typing.write_all(bytes).unwrap();
        let pause = Arc::new(KeysPause::default());
        let got = Arc::new(Mutex::new(Vec::new()));
        let reader = {
            let (pause, got) = (pause.clone(), got.clone());
            std::thread::spawn(move || {
                read_keys(keyboard, &pause, || true, |k| got.lock().unwrap().extend_from_slice(k))
            })
        };
        // Keys typed just before a pause are read or left for ssh; nothing typed after it is read.
        let mut left_for_ssh = 0;
        for round in 0..20 {
            write(b"k");
            assert!(pause.pause(), "round {round}: the reader did not stop");
            let before = got.lock().unwrap().len();
            write(b"secret");
            std::thread::sleep(Duration::from_millis(150));
            assert_eq!(got.lock().unwrap().len(), before, "round {round}: read while paused");
            let mut waiting = Vec::new();
            while !waiting.ends_with(b"secret") {
                let mut buf = [0u8; 64];
                // SAFETY: reading the pipe into a buffer we own.
                let n = unsafe { libc::read(keyboard, buf.as_mut_ptr().cast(), buf.len()) };
                assert!(n > 0);
                waiting.extend_from_slice(&buf[..n as usize]);
            }
            left_for_ssh += waiting.len() - b"secret".len();
            pause.resume();
        }
        write(b"after");
        let start = Instant::now();
        while !got.lock().unwrap().ends_with(b"after") {
            assert!(start.elapsed() < Duration::from_secs(5), "{:?}", got.lock().unwrap());
            std::thread::sleep(Duration::from_millis(5));
        }
        let got_keys = got.lock().unwrap().iter().filter(|b| **b == b'k').count();
        assert_eq!(got_keys + left_for_ssh, 20);
        assert!(!got.lock().unwrap().windows(6).any(|w| w == b"secret"));
        // Closing the typing end: the reader sees the end.
        drop(typing);
        reader.join().unwrap();
        drop(keyboard_end);
    }
}
