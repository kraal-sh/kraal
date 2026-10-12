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
    let exe = std::env::current_exe().context("locating the kraal binary")?;
    let log = paths::log_path(session);
    // The state directory holds what the server logs and saves: only ours.
    if let Some(dir) = log.parent() {
        paths::ensure_private_dir(dir)?;
    }
    let out = paths::open_private_append(&log)?;
    // The server writes a byte to this pipe once it listens, instead of being asked again and
    // again whether it does.
    let (ready, ready_end) = std::io::pipe().context("creating a pipe")?;
    let ready_fd = std::os::fd::AsRawFd::as_raw_fd(&ready_end);
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["server", "--session", session])
        .env(READY_FD_VAR, ready_fd.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(out.try_clone()?)
        .stderr(out)
        .current_dir(dirs::home_dir().unwrap_or_else(|| "/".into()));
    // Detach from the terminal so closing it does not take the server down, and keep the end
    // of the pipe the server writes to open across exec (in the child only).
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(move || {
            libc::setsid();
            if libc::fcntl(ready_fd, libc::F_SETFD, 0) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().context("starting the kraal server")?;
    drop(ready_end);
    let start = Instant::now();
    if wait_ready(&ready, Duration::from_secs(5)) {
        return Ok(socket);
    }
    // It closed the pipe without a word: it failed, or another server took the session first.
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

/// The environment variable that gives a new server the descriptor of the pipe it says on that
/// it listens (see [`crate::server::run`]).
pub const READY_FD_VAR: &str = "KRAAL_READY_FD";

/// Wait until the server writes its byte to `ready`: `true` then; `false` when it closed its end
/// without writing (it exited) or `limit` passed.
fn wait_ready(ready: &std::io::PipeReader, limit: Duration) -> bool {
    use std::os::fd::AsRawFd;
    let start = Instant::now();
    loop {
        let left = limit.saturating_sub(start.elapsed());
        if left.is_zero() {
            return false;
        }
        let mut pfd = libc::pollfd { fd: ready.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd.
        match unsafe { libc::poll(&mut pfd, 1, left.as_millis().min(i32::MAX as u128) as i32) } {
            0 => return false,
            n if n < 0 => {
                if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                    return false;
                }
            }
            _ => {
                let mut byte = [0u8; 1];
                return matches!((&*ready).read(&mut byte), Ok(1));
            }
        }
    }
}

/// Socket for control commands: `$KRAAL_SOCKET` inside a pane unless a session is named.
pub fn control_socket(session: Option<&str>) -> PathBuf {
    match session {
        Some(s) => paths::socket_path(s),
        None => std::env::var_os("KRAAL_SOCKET").map(PathBuf::from).unwrap_or_else(|| paths::socket_path("main")),
    }
}

/// Send one request and wait for its answer. When the server runs a kraal that cannot do what
/// is asked, the error says so, and how to update it.
pub fn request(socket: &Path, req: Request) -> Result<Response> {
    if socket.starts_with(paths::runtime_dir()) {
        paths::private_runtime_dir().context("checking the runtime directory")?;
    }
    let no_server = || format!("no kraal server at {} (is it running?)", socket.display());
    let needs = req.needs();
    // A server without this feature would not refuse the request: it would quietly do less.
    if let Some(feature) = needs.filter(|f| f.check_first) {
        let server = ping(socket).with_context(no_server)?;
        if !server.has(feature) {
            bail!(server.cannot(feature.does));
        }
    }
    let mut stream = UnixStream::connect(socket).with_context(no_server)?;
    let msg = ClientMsg::request(req);
    match exchange(&mut stream, &msg)? {
        Ok(answer) => Ok(answer),
        Err(e) if protocol::refused_as_unknown(&e) => {
            let name = match &msg {
                ClientMsg::Request { request, .. } => request.name(),
                _ => String::new(),
            };
            bail!(refusal(socket, needs, &name, e))
        }
        Err(e) => bail!(e),
    }
}

/// What to say when the server behind `socket` could not read the request `name`: the
/// server's own words, unless it is a kraal that lacks what the request needs.
fn refusal(socket: &Path, needs: Option<protocol::Feature>, name: &str, said: String) -> String {
    let Ok(server) = ping(socket) else { return said };
    if needs.is_some_and(|f| server.has(f)) {
        return said;
    }
    if protocol::newer(&server.version, protocol::KRAAL_VERSION) {
        let (session, version) = (&server.session, &server.version);
        return format!("{said} (session {session} runs kraal {version}, newer than this one: `kraal update`)");
    }
    match needs {
        Some(feature) => server.cannot(feature.does),
        None => server.cannot(&format!("answer `{name}`")),
    }
}

/// One request to a server that may not answer, given up on after `limit`; a refusal is said
/// as the server said it.
pub fn request_within(socket: &Path, req: Request, limit: Duration) -> Result<Response> {
    let deadline = Instant::now() + limit;
    let mut stream = UnixStream::connect(socket)?;
    exchange_until(&mut stream, &ClientMsg::request(req), deadline)?.map_err(anyhow::Error::msg)
}

/// [`exchange`], given up on at `deadline`: the server sends one answer.
fn exchange_until(stream: &mut UnixStream, msg: &ClientMsg, deadline: Instant) -> Result<Result<Response, String>> {
    // A socket refuses a zero timeout.
    let left = deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
    stream.set_read_timeout(Some(left))?;
    stream.set_write_timeout(Some(left))?;
    exchange(stream, msg).map_err(|e| {
        let timed_out = e
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut));
        if timed_out { anyhow::anyhow!("the server did not answer in time") } else { e }
    })
}

/// Send `msg` and wait for the server's answer, which is `Err` when it refused.
fn exchange(stream: &mut UnixStream, msg: &ClientMsg) -> Result<Result<Response, String>> {
    protocol::write_json(stream, msg)?;
    loop {
        match protocol::read_frame(stream)? {
            None => bail!("the server closed the connection"),
            Some((protocol::JSON, payload)) => match serde_json::from_slice(&payload)? {
                ServerMsg::Response { result } => return Ok(result),
                ServerMsg::Exit { reason, .. } => bail!("{reason}"),
                ServerMsg::Open { .. }
                | ServerMsg::Event { .. }
                | ServerMsg::Events { .. }
                | ServerMsg::Attached { .. }
                | ServerMsg::Unknown(_) => continue,
            },
            Some(_) => continue,
        }
    }
}

/// What a server says it is, in answer to a ping.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub pid: u32,
    pub session: String,
    /// Its kraal release.
    pub version: String,
    /// Its [`protocol::Feature`]s; `None` from a server older than that list.
    pub features: Option<Vec<String>>,
}

impl ServerInfo {
    pub fn has(&self, feature: protocol::Feature) -> bool {
        self.features.as_ref().is_some_and(|list| list.iter().any(|f| f == feature.name))
    }

    /// That this server's kraal cannot do `what`, and that a restart updates it.
    pub fn cannot(&self, what: &str) -> String {
        let (session, version) = (&self.session, &self.version);
        format!(
            "session {session} runs kraal {version}, which cannot {what}: `kraal -s {session} restart` updates it (its layout and scrollback are kept)"
        )
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

/// Stop the server behind `socket` and wait until it has exited ([`STOP_TIMEOUT`] at most). A
/// server from another kraal version refuses every request, so when asking fails it gets
/// SIGTERM, which every version handles the same way: save the layout, then exit.
pub fn kill_server(socket: &Path) -> Result<Saved> {
    stop_server(socket, false)
}

/// Restart the session's server on this kraal: the server saves the session and stops, telling
/// its attached clients to wait for the next one, which starts from this binary and restores
/// the session. Whether the layout was saved, as for `kraal kill`; a server of this protocol
/// that cannot save it first refuses, and keeps running.
pub fn restart_server(session: &str) -> Result<Saved> {
    let saved = stop_server(&paths::socket_path(session), true)?;
    ensure_server(session)?;
    Ok(saved)
}

/// What the server behind `socket` is, given up on quickly when it does not answer.
pub fn ping(socket: &Path) -> Result<ServerInfo> {
    match request_within(socket, Request::Ping, Duration::from_secs(2))? {
        Response::Pong { pid, session, version, features } => Ok(ServerInfo { pid, session, version, features }),
        _ => bail!("unexpected answer from the server"),
    }
}

/// The kraal binary installed where this one was started from. Once an update has replaced the
/// file, Linux names the running one `<path> (deleted)`: the file now at `<path>` is meant.
pub fn installed_exe() -> Result<PathBuf> {
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

/// Start `kraal -s <session> restart --yes` from `exe` on its own, out of any terminal, pane and
/// session: it outlives the pane or the server that starts it. What it says goes to the
/// session's log.
pub fn restart_detached(exe: &Path, session: &str) -> Result<std::process::Child> {
    let log = paths::log_path(session);
    if let Some(dir) = log.parent() {
        paths::ensure_private_dir(dir)?;
    }
    let out = paths::open_private_append(&log)?;
    let mut cmd = std::process::Command::new(exe);
    cmd.args(["-s", session, "restart", "--yes"])
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

/// How long stopping a server may take, from asking it to its exit: a restart waits up to 10 s
/// for the session to be saved, then the server as long for its state, and as long again for
/// its runs' logs, to reach the disk.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);

fn stop_server(socket: &Path, restart: bool) -> Result<Saved> {
    let deadline = Instant::now() + STOP_TIMEOUT;
    // A server that answers speaks this protocol: its refusal to restart stands (it could not
    // save the session first). One of another protocol refuses every request.
    let refusal_stands = restart && ping(socket).is_ok();
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("no kraal server at {} (is it running?)", socket.display()))?;
    let (pid, uid) = process::socket_peer(&stream).context("finding the server process")?;
    // SAFETY: getuid cannot fail.
    if uid != unsafe { libc::getuid() } {
        bail!("{} belongs to another user", socket.display());
    }
    let ask = ClientMsg::request(Request::KillServer { restart });
    let answer = exchange_until(&mut stream, &ask, deadline).and_then(|a| a.map_err(anyhow::Error::msg));
    let saved = match answer {
        Ok(Response::Text(why)) => Saved::No(why),
        Ok(_) => Saved::Yes,
        Err(e) if refusal_stands => return Err(e),
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
    while process::running(pid) {
        if Instant::now() > deadline {
            bail!("the server (pid {pid}) is still running after {} s", STOP_TIMEOUT.as_secs());
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
    protocol::write_json(&mut stream, &ClientMsg::request(request))?;
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
    protocol::write_json(&mut stream, &ClientMsg::request(req))?;
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
    let text = crate::text::truncate(msg, usize::from(cols).saturating_sub(2));
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
fn greet(target: &Target, workspace: &Option<String>) -> Result<Greeted, String> {
    let mut conn = connect(target).map_err(|e| format!("{e:#}"))?;
    let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
    // The workspace asked for is asked again after a reconnection.
    let hello = ClientMsg::Attach {
        version: protocol::PROTOCOL_VERSION,
        cols,
        rows,
        workspace: workspace.clone(),
        remote: matches!(target, Target::Remote { .. }),
        pane: match target {
            Target::Remote { pane, .. } => pane.clone(),
            Target::Local { .. } => None,
        },
        // A local client is the kraal installed on the server's machine; a remote one may not be.
        installed: matches!(target, Target::Local { .. }).then(|| protocol::KRAAL_VERSION.to_string()),
        features: protocol::client_features(),
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
/// reader still busy with an earlier read is never taken for a stopped one. A byte written to
/// `wake` after each change wakes the reader to look, so it never wakes up by itself.
struct KeysPause {
    paused: AtomicBool,
    epoch: AtomicU64,
    parked: AtomicU64,
    wake: (std::io::PipeReader, std::io::PipeWriter),
}

impl KeysPause {
    fn new() -> std::io::Result<KeysPause> {
        let (r, w) = std::io::pipe()?;
        for fd in [std::os::fd::AsRawFd::as_raw_fd(&r), std::os::fd::AsRawFd::as_raw_fd(&w)] {
            // Neither end ever blocks: the reader empties it, a full pipe already wakes it.
            // SAFETY: plain fcntl on a descriptor this owns.
            unsafe { libc::fcntl(fd, libc::F_SETFL, libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK) };
        }
        Ok(KeysPause {
            paused: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            parked: AtomicU64::new(0),
            wake: (r, w),
        })
    }

    /// Stop reading the keyboard; `true` once the reader has stopped, so what is typed next (a
    /// password for ssh) is never read as keys for kraal. The reader only reads what is waiting
    /// and never blocks, so it stops as soon as it is woken; `false` means it did not, and the
    /// terminal must not be handed over.
    fn pause(&self) -> bool {
        // Paused before the number changes: a reader that sees the new number sees the pause.
        self.paused.store(true, Ordering::SeqCst);
        let epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        self.wake();
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
        self.wake();
    }

    fn wake(&self) {
        let _ = (&self.wake.1).write(b"w");
    }

    /// Forget the wake-ups seen: the flags they were for are read next.
    fn drain(&self) {
        let mut buf = [0u8; 64];
        while matches!((&self.wake.0).read(&mut buf), Ok(n) if n > 0) {}
    }
}

/// Read `fd` (the keyboard) into `forward` until it ends. Nothing is read while paused: the
/// reader parks before it would read, and it reads only the bytes already waiting, so a read
/// never blocks it past a pause. It sleeps until a key comes or the pause changes.
fn read_keys(fd: std::os::unix::io::RawFd, pause: &KeysPause, mut forward: impl FnMut(&[u8])) {
    use std::os::fd::AsRawFd;
    let mut buf = [0u8; 16 * 1024];
    let wake = pause.wake.0.as_raw_fd();
    loop {
        if pause.paused.load(Ordering::SeqCst) {
            pause.parked.store(pause.epoch.load(Ordering::SeqCst), Ordering::SeqCst);
            // Until resumed, only the wake-up is listened to.
            let mut pfd = libc::pollfd { fd: wake, events: libc::POLLIN, revents: 0 };
            // SAFETY: one valid pollfd.
            unsafe { libc::poll(&mut pfd, 1, -1) };
            pause.drain();
            continue;
        }
        let mut pfd = [
            libc::pollfd { fd, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: wake, events: libc::POLLIN, revents: 0 },
        ];
        // SAFETY: two valid pollfds.
        let ready = unsafe { libc::poll(pfd.as_mut_ptr(), 2, -1) };
        if ready < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return;
        }
        if pfd[1].revents != 0 {
            pause.drain();
        }
        if ready <= 0 || pfd[0].revents == 0 || pause.paused.load(Ordering::SeqCst) {
            continue;
        }
        let mut waiting: libc::c_int = 0;
        // SAFETY: FIONREAD writes the number of bytes waiting into `waiting`.
        if unsafe { libc::ioctl(fd, libc::FIONREAD, &mut waiting) } != 0 {
            return;
        }
        if waiting <= 0 {
            // Ready with nothing to read: the end (a closed pipe, a hung-up terminal).
            if pfd[0].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
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
    let first = greet(&target, &workspace).map_err(anyhow::Error::msg)?;
    // So does a session that refuses at once (a kraal of another generation, a name that is no
    // workspace of its own): nothing was attached.
    if let (protocol::JSON, Ok(ServerMsg::Exit { reason, reattach: None })) =
        (first.first.0, serde_json::from_slice(&first.first.1))
    {
        bail!(reason);
    }
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
        crate::threads::named("signals").spawn(move || {
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
    let writer: Arc<Mutex<Option<Box<dyn Write + Send>>>> = Arc::new(Mutex::new(None));
    let quit = Arc::new(AtomicBool::new(false));
    let pause = Arc::new(KeysPause::new().context("creating a pipe")?);

    // Keyboard → server. While disconnected, ctrl+c gives up.
    {
        let writer = writer.clone();
        let quit = quit.clone();
        let pause = pause.clone();
        crate::threads::named("stdin").spawn(move || {
            read_keys(0, &pause, |keys| {
                let Ok(mut w) = writer.lock() else { return };
                match w.as_mut() {
                    Some(w) => {
                        let _ = protocol::write_frame(w, protocol::BYTES, keys);
                    }
                    None if is_ctrl_c(keys) => quit.store(true, Ordering::SeqCst),
                    None => {}
                }
            });
        })?;
    }
    // Window size changes → server.
    {
        let writer = writer.clone();
        let mut signals = signal_hook::iterator::Signals::new([signal_hook::consts::SIGWINCH])?;
        crate::threads::named("winch").spawn(move || {
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

    // In a pane of another kraal, attached to a server's session: that kraal is told which
    // session this is, and that session which of its panes this shows, so that the kraal outside
    // knows what is on screen here and can ask for another pane (OSC 7701).
    let nested = match &target {
        Target::Remote { label, session, .. } if std::env::var_os("KRAAL_SOCKET").is_some() => {
            Some(format!("\x1b]7701;attach;{label};{session}\x07"))
        }
        _ => None,
    };
    let mut greeted = Some(first);
    let mut delay = Duration::from_secs(1);
    // What the server said it runs, and the server that said it is restarting.
    let mut server_version = None;
    let mut restarting = None;
    let reason = 'session: loop {
        let session = match greeted.take() {
            Some(s) => Ok(s),
            None => {
                // ssh has the terminal until the server answers (a password, a host key): kraal
                // reads nothing, and ctrl+c (a signal again) ends it. The keyboard modes are off
                // already (see below).
                if pause.pause() {
                    let _ = crossterm::terminal::disable_raw_mode();
                    let greeted = greet(&target, &workspace);
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
                let mut to_server = std::mem::replace(&mut c.writer, Box::new(std::io::sink()));
                let mut out = std::io::stdout().lock();
                if let Some(said) = &nested {
                    let _ = protocol::write_frame(&mut to_server, protocol::BYTES, b"\x1b]7701;report\x07");
                    let _ = out.write_all(said.as_bytes()).and_then(|_| out.flush());
                }
                *writer.lock().unwrap() = Some(to_server);
                delay = Duration::from_secs(1);
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
                            Ok(ServerMsg::Exit { reason, reattach: None }) => break 'session reason,
                            // The next server shows the same workspace.
                            Ok(ServerMsg::Exit { reattach: Some(ws), .. }) => {
                                workspace = Some(ws).filter(|w| !w.is_empty());
                                restarting = Some(c.server);
                                break "the session restarts".to_string();
                            }
                            Ok(ServerMsg::Attached { version, .. }) => server_version = Some(version),
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
                if nested.is_some() {
                    let _ = out.write_all(NOT_ATTACHED).and_then(|_| out.flush());
                }
                lost
            }
            Err(lost) => lost,
        };
        if let (Some(old), Target::Local { session }) = (restarting.take(), &target) {
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
    if nested.is_some() {
        let mut out = std::io::stdout();
        let _ = out.write_all(NOT_ATTACHED).and_then(|_| out.flush());
    }
    drop(guard);
    eprintln!("[kraal: {reason}]");
    if let Target::Local { session } = &target {
        // A session left running on an older kraal is said once more, here.
        let socket = paths::socket_path(session);
        let running = server_version
            .filter(|_| UnixStream::connect(&socket).is_ok())
            .or_else(|| ping(&socket).ok().map(|s| s.version));
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

/// What a client attached to a server's session from a pane of another kraal tells that kraal
/// once it is not (the connection dropped, or it ends).
const NOT_ATTACHED: &[u8] = b"\x1b]7701;attach\x07";

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
    fn nothing_typed_after_a_pause_is_read() {
        use std::os::unix::io::AsRawFd;
        // A socket pair stands in for the keyboard (closed on exec, unlike a bare pipe: a child
        // another test starts would keep it open).
        let (keyboard_end, mut typing) = std::os::unix::net::UnixStream::pair().unwrap();
        let keyboard = keyboard_end.as_raw_fd();
        let mut write = |bytes: &[u8]| typing.write_all(bytes).unwrap();
        let pause = Arc::new(KeysPause::new().unwrap());
        let got = Arc::new(Mutex::new(Vec::new()));
        let reader = {
            let (pause, got) = (pause.clone(), got.clone());
            std::thread::spawn(move || read_keys(keyboard, &pause, |k| got.lock().unwrap().extend_from_slice(k)))
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

    #[test]
    #[cfg(target_os = "linux")]
    fn the_keyboard_reader_sleeps_until_a_key_or_a_pause() {
        use std::os::unix::io::AsRawFd;
        let (keyboard_end, mut typing) = std::os::unix::net::UnixStream::pair().unwrap();
        let keyboard = keyboard_end.as_raw_fd();
        let pause = Arc::new(KeysPause::new().unwrap());
        let (got_tx, got_rx) = crossbeam_channel::unbounded::<Vec<u8>>();
        let (tid_tx, tid_rx) = crossbeam_channel::bounded(1);
        let reader = {
            let pause = pause.clone();
            std::thread::spawn(move || {
                // SAFETY: gettid has no preconditions.
                let _ = tid_tx.send(unsafe { libc::gettid() });
                read_keys(keyboard, &pause, |k| {
                    let _ = got_tx.send(k.to_vec());
                })
            })
        };
        let tid = tid_rx.recv().unwrap();
        let switches = crate::testing::thread_switches(tid);
        // Once it sleeps (it was just started).
        crate::testing::wait_asleep(tid);
        let before = switches();
        std::thread::sleep(Duration::from_millis(700));
        assert_eq!(switches(), before, "the reader woke up while nothing was typed");
        typing.write_all(b"a").unwrap();
        assert_eq!(got_rx.recv_timeout(Duration::from_secs(10)).unwrap(), b"a");
        // A pause is seen at once, without a key to wake the reader; so is the end of it. The time
        // the reader, or this thread asking it, waited for a processor is the machine's, not the
        // reader's.
        // SAFETY: gettid has no preconditions.
        let here = unsafe { libc::gettid() };
        let waited = || crate::testing::waited_for_a_processor(tid) + crate::testing::waited_for_a_processor(here);
        let (start, waited_before) = (Instant::now(), waited());
        assert!(pause.pause());
        let took = start.elapsed().saturating_sub(waited().saturating_sub(waited_before));
        assert!(took < Duration::from_millis(500), "{took:?} of {:?}", start.elapsed());
        // Once it went to sleep (it says it parked just before).
        crate::testing::wait_asleep(tid);
        let parked = switches();
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(switches(), parked, "a paused reader sleeps too");
        pause.resume();
        typing.write_all(b"b").unwrap();
        assert_eq!(got_rx.recv_timeout(Duration::from_secs(10)).unwrap(), b"b");
        drop(typing);
        reader.join().unwrap();
        drop(keyboard_end);
    }

    /// A server of a release before feature lists: what it lacks is said with the way out, and
    /// what it would quietly do less of is not even sent.
    #[test]
    fn what_an_older_server_cannot_do_is_said_with_the_way_out() {
        let dir = std::env::temp_dir().join(format!("kraal-older-server-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("old.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let asked = Arc::new(Mutex::new(Vec::<String>::new()));
        {
            let asked = asked.clone();
            std::thread::spawn(move || {
                for mut stream in listener.incoming().map_while(Result::ok) {
                    let Ok(Some((_, payload))) = protocol::read_frame(&mut stream) else { continue };
                    let msg: serde_json::Value = serde_json::from_slice(&payload).unwrap();
                    let cmd = msg["request"]["cmd"].as_str().unwrap_or_default().to_string();
                    let answer = match cmd.as_str() {
                        "ping" => {
                            serde_json::json!({"Ok": {"kind": "pong", "data": {"pid": 1, "session": "old", "version": "0.1.9"}}})
                        }
                        "read_pane" => serde_json::json!({"Err": "no such pane: 9"}),
                        _ => {
                            serde_json::json!({"Err": format!("bad message: unknown variant `{cmd}`, expected `ping`")})
                        }
                    };
                    asked.lock().unwrap().push(cmd);
                    let _ =
                        protocol::write_json(&mut stream, &serde_json::json!({"type": "response", "result": answer}));
                }
            });
        }
        let err = request(&socket, Request::BufferList).unwrap_err().to_string();
        assert_eq!(
            err,
            "session old runs kraal 0.1.9, which cannot keep paste buffers: `kraal -s old restart` updates it (its layout and scrollback are kept)"
        );
        let edit =
            |sudo| Request::EditRemote { target: "web:/a".into(), line: None, near: None, production: false, sudo };
        let err = request(&socket, edit(true)).unwrap_err().to_string();
        assert!(err.contains("runs kraal 0.1.9, which cannot edit a server's files as root"), "{err}");
        assert!(!asked.lock().unwrap().iter().any(|c| c == "edit_remote"), "asked about before it is sent");
        let err = request(&socket, edit(false)).unwrap_err().to_string();
        assert!(err.contains("which cannot answer `edit_remote`"), "{err}");
        // A refusal for another reason is said as the server said it.
        let err = request(&socket, Request::ReadPane { pane: "9".into(), lines: None }).unwrap_err().to_string();
        assert_eq!(err, "no such pane: 9");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_new_server_is_waited_for_until_it_says_it_listens() {
        let (ready, mut end) = std::io::pipe().unwrap();
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            end.write_all(b"1").unwrap();
        });
        let start = Instant::now();
        assert!(wait_ready(&ready, Duration::from_secs(5)));
        assert!(start.elapsed() < Duration::from_secs(2));
        writer.join().unwrap();
        // A server that goes without a word is not waited for.
        let (ready, end) = std::io::pipe().unwrap();
        drop(end);
        let start = Instant::now();
        assert!(!wait_ready(&ready, Duration::from_secs(5)));
        assert!(start.elapsed() < Duration::from_secs(1));
        // Nor one that says nothing in time.
        let (ready, _end) = std::io::pipe().unwrap();
        assert!(!wait_ready(&ready, Duration::from_millis(50)));
    }
}
