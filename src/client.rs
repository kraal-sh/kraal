//! The attaching client and the one-shot control connection used by CLI commands.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::paths;
use crate::protocol::{self, ClientMsg, Request, Response, ServerMsg};

/// Make sure a server runs for `session`, starting one in the background if needed.
pub fn ensure_server(session: &str) -> Result<PathBuf> {
    paths::private_runtime_dir().context("checking the runtime directory")?;
    let socket = paths::socket_path(session);
    if UnixStream::connect(&socket).is_ok() {
        return Ok(socket);
    }
    let exe = std::env::current_exe().context("locating the kraal binary")?;
    let log = paths::log_path(session);
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let out = std::fs::OpenOptions::new().create(true).append(true).open(&log)?;
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
    protocol::write_json(&mut stream, &ClientMsg::Request { version: protocol::PROTOCOL_VERSION, request: req })?;
    loop {
        match protocol::read_frame(&mut stream)? {
            None => bail!("the server closed the connection"),
            Some((protocol::JSON, payload)) => {
                let msg: ServerMsg = serde_json::from_slice(&payload)?;
                match msg {
                    ServerMsg::Response { result } => return result.map_err(|e| anyhow::anyhow!(e)),
                    ServerMsg::Exit { reason } => bail!("{reason}"),
                    ServerMsg::Open { .. } => continue,
                }
            }
            Some(_) => continue,
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

// `CSI > 1 u` asks the terminal for unambiguous keys (kitty keyboard protocol), so Shift+Enter,
// Ctrl+Enter or Esc arrive distinct; terminals without the protocol ignore it.
const SETUP_NO_MOUSE: &str = "\x1b[?1049h\x1b[?2004h\x1b[?1004h\x1b[?25l\x1b[>1u";
const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const RESTORE: &str =
    "\x1b[<u\x1b[?1006l\x1b[?1002l\x1b[?1000l\x1b[?1004l\x1b[?2004l\x1b[0m\x1b[0 q\x1b[?25h\x1b[?1049l";

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut out = std::io::stdout();
        let _ = out.write_all(RESTORE.as_bytes());
        let _ = out.flush();
        let _ = crossterm::terminal::disable_raw_mode();
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
            Ok(Conn { reader: Box::new(s.try_clone()?), writer: Box::new(s), child: None })
        }
        Target::Remote { ssh, session, .. } => {
            // The session name is validated (letters, digits, - _ .), so it needs no quoting.
            let remote =
                format!("k=$(command -v kraal || echo \"$HOME/.local/bin/kraal\"); exec \"$k\" proxy -s {session}");
            let mut child = std::process::Command::new("ssh")
                .args(ssh)
                .arg(remote)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()
                .context("starting ssh")?;
            let reader = Box::new(child.stdout.take().context("ssh stdout")?);
            let writer = Box::new(child.stdin.take().context("ssh stdin")?);
            Ok(Conn { reader, writer, child: Some(child) })
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

/// Attach the current terminal to a session. Remote sessions reconnect by themselves when the
/// connection drops; their programs keep running on the server meanwhile.
pub fn attach(target: Target, workspace: Option<String>, mouse: bool) -> Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    if std::env::var_os("KRAAL_PANE").is_some() {
        bail!("already inside kraal (unset KRAAL_PANE to nest anyway)");
    }
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal() {
        bail!("kraal needs a terminal");
    }
    // Fail before touching the terminal when the first connection is impossible.
    let first = connect(&target)?;

    crossterm::terminal::enable_raw_mode().context("switching the terminal to raw mode")?;
    let guard = TerminalGuard;
    {
        let mut out = std::io::stdout();
        out.write_all(SETUP_NO_MOUSE.as_bytes())?;
        if mouse {
            out.write_all(MOUSE_ON.as_bytes())?;
        }
        out.flush()?;
    }

    // The current connection's writer; None while reconnecting.
    let writer: Arc<Mutex<Option<Box<dyn Write + Send>>>> = Arc::new(Mutex::new(None));
    let quit = Arc::new(AtomicBool::new(false));

    // Keyboard → server. While disconnected, ctrl+c gives up.
    {
        let writer = writer.clone();
        let quit = quit.clone();
        std::thread::Builder::new().name("stdin".into()).spawn(move || {
            let mut stdin = std::io::stdin().lock();
            let mut buf = [0u8; 16 * 1024];
            loop {
                match stdin.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let Ok(mut w) = writer.lock() else { break };
                        match w.as_mut() {
                            Some(w) => {
                                let _ = protocol::write_frame(w, protocol::BYTES, &buf[..n]);
                            }
                            None if buf[..n].contains(&0x03) => quit.store(true, Ordering::SeqCst),
                            None => {}
                        }
                    }
                }
            }
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

    let mut workspace = workspace;
    let mut conn = Some(first);
    let mut delay = Duration::from_secs(1);
    let reason = 'session: loop {
        let c = match conn.take() {
            Some(c) => Ok(c),
            None => connect(&target),
        };
        let lost = match c {
            Ok(mut c) => {
                let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
                let hello =
                    ClientMsg::Attach { version: protocol::PROTOCOL_VERSION, cols, rows, workspace: workspace.take() };
                if protocol::write_json(&mut c.writer, &hello).is_err() {
                    "could not talk to the server".to_string()
                } else {
                    *writer.lock().unwrap() = Some(std::mem::replace(&mut c.writer, Box::new(std::io::sink())));
                    delay = Duration::from_secs(1);
                    let mut reader = std::io::BufReader::with_capacity(
                        256 * 1024,
                        std::mem::replace(&mut c.reader, Box::new(std::io::empty())),
                    );
                    let mut out = std::io::stdout().lock();
                    let lost = loop {
                        match protocol::read_frame(&mut reader) {
                            Ok(Some((protocol::BYTES, bytes))) => {
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
                            Ok(None) => break "the connection closed".to_string(),
                            Err(e) => break format!("connection error: {e}"),
                        }
                    };
                    *writer.lock().unwrap() = None;
                    lost
                }
            }
            Err(e) => format!("{e:#}"),
        };
        let Target::Remote { label, .. } = &target else {
            break 'session format!("the server went away ({lost})");
        };
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
        status_line(&format!("kraal: reconnecting to {label}…"));
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
