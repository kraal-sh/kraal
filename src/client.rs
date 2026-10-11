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
                    ServerMsg::Exit { reason } => bail!("{reason}"),
                    ServerMsg::Open { .. } | ServerMsg::Event { .. } | ServerMsg::Events { .. } => continue,
                }
            }
            Some(_) => continue,
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
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("no kraal server at {} (is it running?)", socket.display()))?;
    let (pid, uid) = process::socket_peer(&stream).context("finding the server process")?;
    // SAFETY: getuid cannot fail.
    if uid != unsafe { libc::getuid() } {
        bail!("{} belongs to another user", socket.display());
    }
    let saved = match exchange(&mut stream, Request::KillServer) {
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
            ServerMsg::Exit { reason } => bail!("{reason}"),
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
const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const MOUSE_OFF: &str = "\x1b[?1006l\x1b[?1003l\x1b[?1002l\x1b[?1000l";

/// Turn the terminal's input modes on (kitty keys, bracketed paste, focus reports, the mouse
/// when asked) while a server answers, and off while kraal waits for one.
fn set_modes(on: bool, mouse: bool) {
    let mut out = std::io::stdout();
    let _ = out.write_all(if on { KEYS_ON } else { KEYS_OFF }.as_bytes());
    if mouse {
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
    },
}

struct Conn {
    reader: Box<dyn Read + Send>,
    writer: Box<dyn Write + Send>,
    child: Option<std::process::Child>,
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
            Ok(Conn {
                reader: Box::new(s.try_clone()?),
                writer: Box::new(s),
                child: None,
                ssh_said: Default::default(),
            })
        }
        Target::Remote { ssh, session, .. } => {
            // The session name is validated (letters, digits, - _ .), so it needs no quoting. The
            // script goes through sh: the login shell on the server may not be a POSIX one.
            let remote = crate::shell::sh_c(&format!(
                "k=$(command -v kraal || echo \"$HOME/.local/bin/kraal\"); exec \"$k\" proxy -s {session}"
            ));
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
            Ok(Conn { reader, writer, child: Some(child), ssh_said })
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

/// Read `fd` (the keyboard) into `forward` until it ends. Nothing is read while paused: the
/// reader parks before it would read, and it reads only the bytes already waiting, so a read
/// never blocks it past a pause.
fn read_keys(fd: std::os::unix::io::RawFd, pause: &KeysPause, mut forward: impl FnMut(&[u8])) {
    let mut buf = [0u8; 16 * 1024];
    loop {
        if pause.paused.load(Ordering::SeqCst) {
            pause.parked.store(pause.epoch.load(Ordering::SeqCst), Ordering::SeqCst);
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
/// connection drops; their programs keep running on the server meanwhile.
pub fn attach(target: Target, workspace: Option<String>, mouse: bool) -> Result<()> {
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
    let writer: Arc<Mutex<Option<Box<dyn Write + Send>>>> = Arc::new(Mutex::new(None));
    let quit = Arc::new(AtomicBool::new(false));
    let pause = Arc::new(KeysPause::default());

    // Keyboard → server. While disconnected, ctrl+c gives up.
    {
        let writer = writer.clone();
        let quit = quit.clone();
        let pause = pause.clone();
        std::thread::Builder::new().name("stdin".into()).spawn(move || {
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
                *writer.lock().unwrap() = Some(std::mem::replace(&mut c.writer, Box::new(std::io::sink())));
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
                            Ok(ServerMsg::Exit { reason }) => break 'session reason,
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
    Ok(())
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
        drop(write);
        drop(typing);
        reader.join().unwrap();
        drop(keyboard_end);
    }
}
