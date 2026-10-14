//! End-to-end tests: a real server driven through the CLI, in isolated directories.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use common::{Env, claude_hook, git_repo, wait_until, write_script};

/// How long a test waits for what it expects to show before it fails. Far longer than it takes:
/// a machine busy with other work (a CI runner, the suite run beside other suites) is slower by
/// tens of seconds where programs are started and talk to each other, and a wait that ends when
/// its condition holds costs nothing when it does.
const PATIENCE: Duration = Duration::from_secs(60);

impl Env {
    fn workspace(&self, name: &str, kdl: &str) {
        std::fs::write(self.root.join("config/kraal/workspaces").join(format!("{name}.kdl")), kdl).unwrap();
    }

    fn wait_for_text(&self, pane: &str, text: &str) -> String {
        self.wait_for(pane, text, |screen| screen)
    }

    /// Wait for `text` below the line marking what a restart restored, and return that part.
    fn wait_for_text_since_restore(&self, pane: &str, text: &str) -> String {
        self.wait_for(pane, text, since_restore)
    }

    fn wait_for(&self, pane: &str, text: &str, part: impl Fn(&str) -> &str) -> String {
        let start = Instant::now();
        loop {
            let out = self.try_run(&["read", "-p", pane]);
            let screen = if out.status.success() {
                String::from_utf8(out.stdout).unwrap()
            } else {
                // A pane that a command has just asked for may not exist yet.
                let err = String::from_utf8_lossy(&out.stderr);
                assert!(err.contains("no pane named"), "kraal read -p {pane:?} failed: {err}");
                String::new()
            };
            let seen = part(&screen);
            // A narrow pane wraps long lines: the text may run over two rows.
            if seen.contains(text) || seen.lines().collect::<String>().contains(text) {
                return seen.to_string();
            }
            assert!(start.elapsed() < PATIENCE, "timed out waiting for {text:?} in:\n{screen}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The pid of this test's server: a `server --session <name>` process whose environment has
    /// this test's runtime directory.
    #[cfg(not(target_os = "linux"))]
    fn server_pid(&self) -> Option<i32> {
        let want = format!("XDG_RUNTIME_DIR={}", self.root.join("run").display());
        let found = Command::new("pgrep").args(["-f", &format!("server --session {}$", self.session)]).output().ok()?;
        String::from_utf8_lossy(&found.stdout).lines().filter_map(|l| l.trim().parse::<i32>().ok()).find(|pid| {
            // `ps -E` shows the environment after the command.
            Command::new("ps")
                .args(["-E", "-ww", "-o", "command=", "-p", &pid.to_string()])
                .output()
                .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(&want))
        })
    }

    /// The pid of this test's server, from /proc.
    #[cfg(target_os = "linux")]
    fn server_pid(&self) -> Option<i32> {
        let want = format!("XDG_RUNTIME_DIR={}", self.root.join("run").display());
        std::fs::read_dir("/proc").ok()?.filter_map(|e| e.ok()).find_map(|e| {
            let pid: i32 = e.file_name().to_str()?.parse().ok()?;
            let cmdline = std::fs::read(e.path().join("cmdline")).ok()?;
            let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).filter(|a| !a.is_empty()).collect();
            let tail = [&b"server"[..], b"--session", self.session.as_bytes()];
            if args.len() < 3 || args[args.len() - 3..] != tail {
                return None;
            }
            let environ = std::fs::read(e.path().join("environ")).ok()?;
            environ.split(|b| *b == 0).any(|v| v == want.as_bytes()).then_some(pid)
        })
    }

    /// Start a long-running command; its output lines arrive on the receiver (read with a
    /// deadline, so a stalled stream fails the test instead of hanging it).
    fn follow(&self, args: &[&str]) -> (std::process::Child, std::sync::mpsc::Receiver<String>) {
        use std::io::BufRead;
        let mut child = self.cmd(args).stdout(std::process::Stdio::piped()).spawn().unwrap();
        let out = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        (child, rx)
    }

    /// A client attached in a terminal (`script`, from util-linux, provides one), and what is
    /// typed into it.
    #[cfg(target_os = "linux")]
    fn client(&self) -> std::process::Child {
        let attach = self.cmd(&["attach"]);
        let line = std::iter::once(attach.get_program())
            .chain(attach.get_args())
            .map(|a| format!("'{}'", a.to_string_lossy()))
            .collect::<Vec<_>>()
            .join(" ");
        let mut c = Command::new("script");
        // The terminal `script` makes has no size until it is given one.
        c.args(["-q", "-f", "-c", &format!("stty cols 100 rows 30; exec {line}"), "/dev/null"]);
        for (k, v) in attach.get_envs() {
            match v {
                Some(v) => c.env(k, v),
                None => c.env_remove(k),
            };
        }
        c.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        let child = c.spawn().expect("script (util-linux)");
        std::thread::sleep(Duration::from_millis(800));
        child
    }

    fn state_file(&self) -> PathBuf {
        self.root.join("state/kraal/sessions-v2").join(format!("{}.json", self.session))
    }

    fn legacy_state_file(&self) -> PathBuf {
        self.root.join("state/kraal/sessions").join(format!("{}.json", self.session))
    }

    /// Start the session by opening workspace `name`, with a server that notes how it composes
    /// each frame (this build does, where `KRAAL_TEST_FRAMES` says): what reads the notes so far,
    /// a frame each, `<client> output` for one made from what one pane printed alone and
    /// `<client> whole` for any other.
    fn open_noting_frames(&self, name: &str) -> impl Fn() -> Vec<String> {
        let notes = self.root.join("frames");
        let out = self.cmd(&["open", name]).env("KRAAL_TEST_FRAMES", &notes).output().unwrap();
        assert!(out.status.success(), "kraal open {name}: {}", String::from_utf8_lossy(&out.stderr));
        move || std::fs::read_to_string(&notes).unwrap_or_default().lines().map(String::from).collect()
    }
}

/// A client attached through the session's socket, as kraal's own client is: it sends what a
/// terminal would, and keeps what the server draws.
struct RawClient {
    stream: std::os::unix::net::UnixStream,
    screen: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    /// The messages the server sent, as JSON.
    messages: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    /// Its terminal's columns and rows.
    size: (usize, usize),
    trace: Option<std::cell::RefCell<Vec<ClientStep>>>,
}

struct ClientStep {
    at: Instant,
    drawn: usize,
    size: (usize, usize),
    action: ClientAction,
}

enum ClientAction {
    Input(Vec<u8>),
    Observed(String),
}

impl RawClient {
    /// Keep input and screen offsets cheaply; replay them only if the test fails.
    fn traced(mut self) -> Self {
        self.trace = Some(Default::default());
        self.trace_step(ClientAction::Observed("attached".into()));
        self
    }

    fn trace_step(&self, action: ClientAction) {
        let drawn = self.screen.lock().unwrap().len();
        self.trace_step_at(drawn, action);
    }

    fn trace_step_at(&self, drawn: usize, action: ClientAction) {
        if let Some(trace) = &self.trace {
            trace.borrow_mut().push(ClientStep { at: Instant::now(), drawn, size: self.size, action });
        }
    }

    fn print_trace(&self) {
        let Some(trace) = &self.trace else { return };
        let trace = trace.borrow();
        let bytes = self.screen.lock().unwrap().clone();
        let show = |drawn, size: (usize, usize)| {
            let rows = text_after(&bytes[..drawn], size.0, size.1);
            for (y, row) in rows.iter().enumerate() {
                eprintln!("{:2} {row}", y + 1);
            }
            rows
        };
        eprintln!("Client trace (mouse coordinates and screen rows are 1-based):");
        for (i, step) in trace.iter().enumerate() {
            eprintln!(
                "Step {i}, +{:?}, {}x{}, {} bytes drawn",
                step.at - trace[0].at,
                step.size.0,
                step.size.1,
                step.drawn
            );
            match &step.action {
                ClientAction::Observed(what) => {
                    eprintln!("Observed: {what}");
                    show(step.drawn, step.size);
                }
                ClientAction::Input(input) => {
                    eprintln!("Before input: {}", input.escape_ascii());
                    let rows = show(step.drawn, step.size);
                    // SGR mouse reports, including both the press and release of a click.
                    for report in input.split(|b| *b == 0x1b).filter_map(|s| s.strip_prefix(b"[<")) {
                        if let Some(end) = report.iter().position(|b| *b == b'M' || *b == b'm') {
                            let fields: Vec<usize> = String::from_utf8_lossy(&report[..end])
                                .split(';')
                                .filter_map(|s| s.parse().ok())
                                .collect();
                            if let [button, col, row] = fields.as_slice() {
                                let text = row.checked_sub(1).and_then(|y| rows.get(y));
                                eprintln!(
                                    "Mouse {} button={button} column={col} row={row}, row text: {text:?}",
                                    if report[end] == b'M' { "press" } else { "release" }
                                );
                            }
                        }
                    }
                    // The next observation/input marks how far the test let this input run.
                    let next = trace.get(i + 1);
                    eprintln!("After input (at next step, or failure):");
                    show(next.map_or(bytes.len(), |s| s.drawn), next.map_or(self.size, |s| s.size));
                }
            }
        }
        eprintln!("Screen at failure:");
        show(bytes.len(), self.size);
    }

    fn attach(env: &Env, workspace: &str) -> RawClient {
        RawClient::attach_installed(env, workspace, None)
    }

    /// Attach, saying which kraal is installed (a local client says its own).
    fn attach_installed(env: &Env, workspace: &str, installed: Option<&str>) -> RawClient {
        RawClient::attach_as(env, Some(workspace), (120, 40), installed)
    }

    /// Attach from a terminal of `size` (columns, rows), asking for a workspace or, as `kraal`
    /// alone does, for none.
    fn attach_sized(env: &Env, workspace: Option<&str>, size: (usize, usize)) -> RawClient {
        RawClient::attach_as(env, workspace, size, None)
    }

    fn attach_as(env: &Env, workspace: Option<&str>, size: (usize, usize), installed: Option<&str>) -> RawClient {
        let socket = env.root.join("run/kraal").join(format!("{}.sock", env.session));
        RawClient::attach_at(&socket, workspace, size, installed)
    }

    /// Attach to the server that listens at `socket`.
    fn attach_at(socket: &Path, workspace: Option<&str>, size: (usize, usize), installed: Option<&str>) -> RawClient {
        use std::io::Read;
        let mut stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
        let installed = installed.map(|v| format!(r#","installed":"{v}""#)).unwrap_or_default();
        let (cols, rows) = size;
        let workspace = serde_json::json!(workspace);
        let hello = format!(
            r#"{{"type":"attach","version":2,"cols":{cols},"rows":{rows},"workspace":{workspace},"remote":false{installed}}}"#
        );
        raw_frame(&mut stream, 0, hello.as_bytes());
        let screen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (mut reader, kept, said) = (stream.try_clone().unwrap(), screen.clone(), messages.clone());
        std::thread::spawn(move || {
            let mut header = [0u8; 5];
            while reader.read_exact(&mut header).is_ok() {
                let mut payload = vec![0u8; u32::from_le_bytes(header[..4].try_into().unwrap()) as usize];
                if reader.read_exact(&mut payload).is_err() {
                    break;
                }
                match header[4] {
                    1 => kept.lock().unwrap().extend_from_slice(&payload),
                    _ => said.lock().unwrap().push(String::from_utf8_lossy(&payload).into_owned()),
                }
            }
        });
        RawClient { stream, screen, messages, size, trace: None }
    }

    /// Its terminal takes another size: the server draws the whole screen again.
    fn resize(&mut self, size: (usize, usize)) {
        self.size = size;
        self.trace_step(ClientAction::Observed(format!("resize to {}x{}", size.0, size.1)));
        let (cols, rows) = size;
        raw_frame(&mut self.stream, 0, format!(r#"{{"type":"resize","cols":{cols},"rows":{rows}}}"#).as_bytes());
    }

    /// Wait until the server has sent a message containing `text`.
    fn wait_for_message(&self, text: &str) {
        let start = Instant::now();
        while !self.messages.lock().unwrap().iter().any(|m| m.contains(text)) {
            assert!(start.elapsed() < PATIENCE, "{text:?} never sent: {:?}", self.messages.lock().unwrap());
            std::thread::sleep(Duration::from_millis(50));
        }
        self.trace_step(ClientAction::Observed(format!("message {text:?}")));
    }

    /// Bytes as the terminal sends them, one read each.
    fn send(&mut self, bytes: &[u8]) {
        if self.trace.is_some() {
            self.trace_step(ClientAction::Input(bytes.to_vec()));
        }
        raw_frame(&mut self.stream, 1, bytes);
    }

    /// A bracketed paste, sent as a terminal does: in reads of 64 KiB.
    fn paste(&mut self, data: &[u8]) {
        let all = [&b"\x1b[200~"[..], data, b"\x1b[201~"].concat();
        for chunk in all.chunks(64 << 10) {
            self.send(chunk);
        }
    }

    /// Wait until the server has drawn `text` on the client's screen.
    fn wait_for_screen(&self, text: &str) {
        let start = Instant::now();
        while !String::from_utf8_lossy(&self.screen.lock().unwrap()).contains(text) {
            assert!(start.elapsed() < PATIENCE, "{text:?} never shown");
            std::thread::sleep(Duration::from_millis(50));
        }
        self.trace_step(ClientAction::Observed(format!("screen contains {text:?}")));
    }
}

impl Drop for RawClient {
    fn drop(&mut self) {
        // The thread that keeps what the server sends holds the connection too: closed for both,
        // so that the server sees this client go when the test lets go of it.
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        if std::thread::panicking() {
            self.print_trace();
        }
    }
}

fn raw_frame(w: &mut impl std::io::Write, kind: u8, payload: &[u8]) {
    let mut frame = (payload.len() as u32).to_le_bytes().to_vec();
    frame.push(kind);
    frame.extend_from_slice(payload);
    w.write_all(&frame).unwrap();
}

/// A client attached in a terminal the test holds (a pty): what the client writes to it is kept
/// byte for byte, and the terminal's line mode can be looked at.
#[cfg(unix)]
struct TerminalClient {
    master: Box<dyn portable_pty::MasterPty + Send>,
    keys: Box<dyn std::io::Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
    written: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    reader: std::thread::JoinHandle<()>,
}

#[cfg(unix)]
impl TerminalClient {
    /// Attach to `env`'s session, and wait until it is drawn.
    #[cfg(target_os = "linux")]
    fn attach(env: &Env) -> TerminalClient {
        Self::start(env.cmd(&["attach"]))
    }

    fn start(attach: Command) -> TerminalClient {
        let size = portable_pty::PtySize { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 };
        let pair = portable_pty::native_pty_system().openpty(size).unwrap();
        let mut cmd = portable_pty::CommandBuilder::new(attach.get_program());
        cmd.args(attach.get_args());
        for (k, v) in attach.get_envs() {
            match v {
                Some(v) => cmd.env(k, v),
                None => cmd.env_remove(k),
            }
        }
        let child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let written = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (mut from_client, kept) = (pair.master.try_clone_reader().unwrap(), written.clone());
        // Until the client has gone: the terminal then has no other end.
        let reader = std::thread::spawn(move || {
            use std::io::Read;
            let mut buf = [0u8; 64 * 1024];
            while let Ok(n @ 1..) = from_client.read(&mut buf) {
                kept.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
        let client =
            TerminalClient { keys: pair.master.take_writer().unwrap(), master: pair.master, child, written, reader };
        client.wait_for_frames(1);
        client
    }

    /// Wait until the client has written `frames` frames to its terminal (each ends a
    /// synchronized update).
    fn wait_for_frames(&self, frames: usize) {
        let start = Instant::now();
        loop {
            let written = self.written.lock().unwrap().clone();
            if written.windows(8).filter(|w| *w == b"\x1b[?2026l").count() >= frames {
                return;
            }
            let shown = String::from_utf8_lossy(&written).replace('\x1b', "^[");
            assert!(start.elapsed() < Duration::from_secs(10), "{frames} frame(s) never written: {shown}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Wait until the client has written `text` to its terminal.
    fn wait_for(&self, text: &str) {
        let start = Instant::now();
        loop {
            let written = String::from_utf8_lossy(&self.written.lock().unwrap()).into_owned();
            if written.contains(text) {
                return;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "{text:?} never written: {written:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn type_keys(&mut self, keys: &[u8]) {
        self.keys.write_all(keys).unwrap();
        self.keys.flush().unwrap();
    }

    #[cfg(target_os = "linux")]
    fn signal(&self, signal: libc::c_int) {
        // SAFETY: plain syscall to the client this test started (ended already, it is still ours
        // to wait for).
        unsafe { libc::kill(self.child.process_id().unwrap() as libc::pid_t, signal) };
    }

    /// Whether the terminal is in line mode, as a shell has it.
    fn line_mode(&self) -> bool {
        // SAFETY: tcgetattr fills the zeroed struct, for a descriptor the test holds.
        let mode = unsafe {
            let mut mode: libc::termios = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(self.master.as_raw_fd().unwrap(), &mut mode), 0);
            mode
        };
        mode.c_lflag & (libc::ICANON | libc::ECHO) == libc::ICANON | libc::ECHO
    }

    /// Wait for the client to end, and check that it gave its terminal back, `what` saying how
    /// it left: its exit status, and what it said once the terminal was the shell's again.
    ///
    /// Given back means: line mode; the keyboard stack popped as often as the client pushed it;
    /// the alternate screen left once; in order (the keyboard, the mouse, the screen, the
    /// cursor); and nothing but plain text after that.
    fn left(mut self, what: &str) -> (u32, String) {
        let start = Instant::now();
        let status = loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                break status;
            }
            assert!(start.elapsed() < Duration::from_secs(15), "{what}: the client is still there");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(self.line_mode(), "{what}: the terminal is not in line mode");
        // The terminal has no other end once the client has gone: all it wrote is read. Only
        // then may the test's own end go, which says so in the terminal (a new line it echoes).
        while !self.reader.is_finished() {
            assert!(start.elapsed() < Duration::from_secs(15), "{what}: the terminal is still held");
            std::thread::sleep(Duration::from_millis(1));
        }
        let written = self.written.lock().unwrap().clone();
        let shown = String::from_utf8_lossy(&written).replace('\x1b', "^[");
        let count = |sequence: &str| written.windows(sequence.len()).filter(|w| *w == sequence.as_bytes()).count();
        let last = |sequence: &str| {
            let at = written.windows(sequence.len()).rposition(|w| w == sequence.as_bytes());
            at.unwrap_or_else(|| panic!("{what}: {sequence:?} never written: {shown}"))
        };
        let (pushed, popped, screens) = (count("\x1b[>5u"), count("\x1b[<u"), count("\x1b[?1049l"));
        assert_eq!((popped, screens), (pushed, 1), "{what}: pushed {pushed}, popped {popped}: {shown}");
        let order = [last("\x1b[<u"), last("\x1b[?1000l"), last("\x1b[?1049l"), last("\x1b[?25h")];
        assert!(order.is_sorted(), "{what}: given back out of order {order:?}: {shown}");
        let said = String::from_utf8_lossy(&written[order[3] + "\x1b[?25h".len()..]).into_owned();
        assert!(!said.contains('\x1b'), "{what}: written after the terminal was given back: {shown}");
        (status.exit_code(), said)
    }
}

#[cfg(unix)]
impl Drop for TerminalClient {
    fn drop(&mut self) {
        if self.child.try_wait().is_ok_and(|status| status.is_none()) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

/// Wait until the pane named `name` is seen as production.
fn wait_production(env: &Env, name: &str) {
    let start = Instant::now();
    loop {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let pane = panes.as_array().unwrap().iter().find(|p| p["name"] == name).cloned().unwrap();
        if pane["production"].is_string() {
            return;
        }
        assert!(start.elapsed() < PATIENCE, "never seen as production: {pane}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// What a pane shows below the line marking the text a restart restored (all of it without one):
/// what runs in the pane now wrote that.
fn since_restore(screen: &str) -> &str {
    match screen.rfind("kraal: restored from") {
        Some(at) => screen[at..].split_once('\n').map_or("", |(_, rest)| rest),
        None => screen,
    }
}

fn wait_gone(path: &Path) {
    let start = Instant::now();
    while path.exists() && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// How `child` ended and what it printed, once it has: still running after `limit`, it is
/// killed and the test fails.
fn finish(mut child: std::process::Child, limit: Duration) -> Output {
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if start.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            panic!("still running after {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

/// A process this test stopped (SIGSTOP), continued when the test ends, however it ends.
struct Stopped(libc::pid_t);

impl Stopped {
    fn stop(pid: libc::pid_t) -> Stopped {
        // SAFETY: plain syscall to a process this test started.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
        Stopped(pid)
    }
}

impl Drop for Stopped {
    fn drop(&mut self) {
        // SAFETY: plain syscall to a process this test started.
        unsafe { libc::kill(self.0, libc::SIGCONT) };
    }
}

#[test]
fn run_send_read_and_close() {
    let env = Env::new("basic");
    env.workspace("w", "layout {\n    pane name=\"first\"\n}\n");
    env.run(&["open", "w"]);
    let id =
        env.run(&["run", "--name", "worker", "--target", "first", "--close-on-exit", "--", "sh"]).trim().to_string();
    env.run(&["send", "-p", &id, "--enter", "echo", "hello-$((40+2))"]);
    env.wait_for_text(&id, "hello-42");
    let panes = env.run(&["panes"]);
    assert!(panes.contains("worker") && panes.contains("first"), "{panes}");
    env.run(&["keys", "-p", &id, "ctrl+d"]);
    let start = Instant::now();
    while env.run(&["panes"]).contains("worker") {
        assert!(start.elapsed() < Duration::from_secs(5), "shell pane did not close on exit");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn config_reload_applies_new_keys_and_keeps_the_old_ones_when_the_file_is_broken() {
    let env = Env::new("reload");
    env.workspace("w", "layout {\n    pane name=\"first\"\n}\n");
    env.run(&["open", "w"]);
    let mut client = env.client();
    let mut typed = |bytes: &[u8]| std::io::Write::write_all(client.stdin.as_mut().unwrap(), bytes).unwrap();
    let panes = || {
        let v: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        v.as_array().map_or(0, Vec::len)
    };
    let wait_for = |n: usize| {
        let start = Instant::now();
        while panes() != n {
            assert!(start.elapsed() < Duration::from_secs(5), "expected {n} panes");
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    // A key the preset leaves free opens a tab once the configuration says so.
    let path = env.root.join("config/kraal/config.toml");
    let base = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("{base}[keys.leader]\nnew_tab = [\"y\"]\n")).unwrap();
    let out = env.try_run(&["config", "reload"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    typed(b"\x02y");
    wait_for(2);
    // A file that cannot be read is an error, and the keys in use stay.
    std::fs::write(&path, "keys = [").unwrap();
    let out = env.try_run(&["config", "reload"]);
    assert!(!out.status.success(), "a broken file was accepted");
    typed(b"\x02y");
    wait_for(3);
    let _ = client.kill();
    let _ = client.wait();
}

#[test]
fn command_panes_stay_after_exit_and_wait_reports_it() {
    let env = Env::new("exit");
    env.workspace("w", "layout {\n    pane name=\"job\" command=\"sh\" {\n        args \"-c\" \"echo working; sleep 0.3; exit 7\"\n    }\n}\n");
    env.run(&["open", "w"]);
    let out = env.run(&["wait", "-p", "job", "--until", "exit", "--timeout", "10"]);
    assert!(!out.is_empty());
    let json = env.run(&["panes", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v[0]["exit_code"], 7, "{json}");
    assert!(env.run(&["read", "-p", "job"]).contains("working"));
}

#[test]
fn a_silent_panes_final_burst_is_drawn_and_its_empty_end_is_seen() {
    let env = Env::new("finalburst");
    env.workspace("w", "layout { pane name=\"main\"; }\n");
    env.run(&["open", "w"]);
    let mut client = RawClient::attach(&env, "w");
    env.run(&["run", "--name", "burst", "--", "sh", "-c", "stty -echo; printf 'FINAL-BURST\\n'; read line"]);
    client.wait_for_row("FINAL-BURST");
    client.send(b"\r");
    env.run(&["wait", "-p", "burst", "--until", "exit", "--timeout", "10"]);
    let text = env.run(&["read", "-p", "burst"]);
    assert_eq!(text.matches("FINAL-BURST").count(), 1, "{text}");
}

#[test]
fn a_wait_for_exit_includes_every_line_of_the_final_output_once() {
    let env = Env::new("finaloutput");
    env.workspace("w", "layout { pane name=\"main\"; }\n");
    env.run(&["open", "w"]);
    let script =
        r#"awk 'BEGIN { for (i = 1; i <= 9000; i++) printf "%06d %023d\n", i, 0 }'; printf 'LAST-LINE\n'; exit 7"#;
    env.run(&["run", "--name", "burst", "--", "sh", "-c", script]);
    env.run(&["wait", "-p", "burst", "--until", "exit", "--timeout", "15"]);
    let text = env.run(&["read", "-p", "burst", "--lines", "9500"]);
    let numbers: Vec<u32> = text.lines().filter_map(|l| l.split_whitespace().next()?.parse().ok()).collect();
    assert_eq!(numbers, (1..=9000).collect::<Vec<_>>());
    assert_eq!(text.matches("LAST-LINE").count(), 1);
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let burst = panes.as_array().unwrap().iter().find(|p| p["name"] == "burst").unwrap();
    assert_eq!(burst["exit_code"], 7);
}

#[test]
fn hooks_drive_agent_state() {
    let env = Env::new("hooks");
    env.workspace("w", "layout {\n    pane name=\"agent\"\n}\n");
    env.run(&["open", "w"]);
    let socket = env.root.join("run/kraal/hooks.sock");
    let hook = |payload: &str| {
        let mut c = env.cmd(&["hook", "claude"]);
        c.env("KRAAL_PANE", "agent").env("KRAAL_SOCKET", &socket).stdin(std::process::Stdio::piped());
        let mut child = c.spawn().unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        assert!(out.stdout.is_empty(), "hooks must not print");
    };
    hook(r#"{"hook_event_name":"SessionStart","session_id":"s-1"}"#);
    hook(r#"{"hook_event_name":"UserPromptSubmit","session_id":"s-1"}"#);
    assert!(env.run(&["agents"]).contains("working"));
    hook(r#"{"hook_event_name":"Notification","message":"Claude needs your permission to use Bash"}"#);
    assert_eq!(env.run(&["wait", "-p", "agent", "--until", "blocked", "--timeout", "2"]).trim(), "blocked");
    hook(r#"{"hook_event_name":"Stop","session_id":"s-1"}"#);
    assert_eq!(env.run(&["wait", "-p", "agent", "--until", "done", "--timeout", "2"]).trim(), "done");
    let timeout = env.try_run(&["wait", "-p", "agent", "--until", "working", "--timeout", "0.2"]);
    assert!(!timeout.status.success());
}

/// Other agents' hooks, as each one calls `kraal hook`; and a hook for another agent than the
/// one a pane runs (Cursor runs Claude Code's hooks) changes nothing there.
#[test]
fn other_agents_hooks_drive_their_state() {
    let env = Env::new("hooks-other");
    env.workspace("w", "layout {\n    pane name=\"gemini\"\n    pane name=\"copilot\"\n    pane name=\"cursor\"\n}\n");
    env.run(&["open", "w"]);
    let socket = env.root.join("run/kraal/hooks-other.sock");
    let hook = |pane: &str, args: &[&str], payload: &str| {
        let mut c = env.cmd(&[&["hook"], args].concat());
        c.env("KRAAL_PANE", pane).env("KRAAL_SOCKET", &socket).stdin(std::process::Stdio::piped());
        let mut child = c.spawn().unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success() && out.stdout.is_empty() && out.stderr.is_empty(), "hooks must stay silent");
    };
    let wait = |pane: &str, state: &str| {
        assert_eq!(env.run(&["wait", "-p", pane, "--until", state, "--timeout", "3"]).trim(), state, "{pane}");
    };
    // Gemini CLI's hooks leave a declined question to the screen: it runs in its pane, busy.
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let gemini = bin.join("gemini");
    write_script(&gemini, "#!/bin/sh\necho started\nwhile :; do sleep 0.3; printf .; done\n");
    env.run(&["send", "-p", "gemini", "--enter", &gemini.display().to_string()]);
    env.wait_for_text("gemini", "started");
    hook("gemini", &["gemini"], r#"{"hook_event_name":"BeforeAgent","session_id":"g-1","prompt":"fix it"}"#);
    wait("gemini", "working");
    hook(
        "gemini",
        &["gemini"],
        r#"{"hook_event_name":"Notification","session_id":"g-1","notification_type":"ToolPermission","message":"Tool Shell requires execution","details":{"type":"exec"}}"#,
    );
    wait("gemini", "blocked");
    hook("gemini", &["gemini"], r#"{"hook_event_name":"AfterAgent","session_id":"g-1"}"#);
    wait("gemini", "done");
    // Copilot's own format names the event on the command line only.
    hook("copilot", &["copilot", "userPromptSubmitted"], r#"{"sessionId":"c-1","timestamp":1,"cwd":"/","prompt":"x"}"#);
    wait("copilot", "working");
    hook(
        "copilot",
        &["copilot", "notification"],
        r#"{"sessionId":"c-1","hook_event_name":"Notification","notification_type":"permission_prompt","message":"Copilot needs permission"}"#,
    );
    wait("copilot", "blocked");
    hook("copilot", &["copilot", "agentStop"], r#"{"sessionId":"c-1","stopReason":"end_turn"}"#);
    wait("copilot", "done");
    // A pane running Cursor's CLI.
    let cursor = bin.join("cursor-agent");
    write_script(&cursor, "#!/bin/sh\necho started\nsleep 30\n");
    env.run(&["send", "-p", "cursor", "--enter", &cursor.display().to_string()]);
    env.wait_for_text("cursor", "started");
    hook(
        "cursor",
        &["claude"],
        r#"{"hook_event_name":"UserPromptSubmit","session_id":"x","cursor_version":"2026.10.01"}"#,
    );
    hook("cursor", &["claude"], r#"{"hook_event_name":"UserPromptSubmit","session_id":"x"}"#);
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let cursor_pane = panes.as_array().unwrap().iter().find(|p| p["name"] == "cursor").unwrap();
    assert_ne!(cursor_pane["agent"], "claude", "{cursor_pane}");
    hook(
        "cursor",
        &["cursor"],
        r#"{"hook_event_name":"stop","conversation_id":"c","status":"completed","loop_count":0}"#,
    );
    wait("cursor", "done");
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let cursor_pane = panes.as_array().unwrap().iter().find(|p| p["name"] == "cursor").unwrap();
    assert_eq!(cursor_pane["agent"], "cursor", "{cursor_pane}");
    env.run(&["keys", "-p", "cursor", "ctrl+c"]);
    env.run(&["keys", "-p", "gemini", "ctrl+c"]);
}

/// droid tells a cancelled turn by waiting for a prompt, without a stop: the pane is idle again.
/// It says the same after a turn that finished, which stays done until it is seen.
#[test]
fn a_cancelled_turn_ends_idle_and_a_finished_one_stays_done() {
    let env = Env::new("hooks-idle");
    env.workspace("w", "layout {\n    pane name=\"droid\"\n}\n");
    env.run(&["open", "w"]);
    let socket = env.root.join("run/kraal/hooks-idle.sock");
    let hook = |payload: &str| {
        let mut c = env.cmd(&["hook", "droid"]);
        c.env("KRAAL_PANE", "droid").env("KRAAL_SOCKET", &socket).stdin(std::process::Stdio::piped());
        let mut child = c.spawn().unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
        assert!(child.wait_with_output().unwrap().status.success());
    };
    let wait = |state: &str| {
        assert_eq!(env.run(&["wait", "-p", "droid", "--until", state, "--timeout", "3"]).trim(), state);
    };
    let prompt = r#"{"hook_event_name":"UserPromptSubmit","session_id":"d-1","prompt":"fix it"}"#;
    let waiting = r#"{"hook_event_name":"Notification","session_id":"d-1","notification_type":"idle_prompt","message":"Droid is waiting for input"}"#;
    hook(prompt);
    wait("working");
    hook(waiting);
    wait("idle");
    hook(prompt);
    hook(r#"{"hook_event_name":"Stop","session_id":"d-1"}"#);
    wait("done");
    hook(waiting);
    let still = env.try_run(&["wait", "-p", "droid", "--until", "idle", "--timeout", "0.5"]);
    assert!(!still.status.success(), "a finished turn nobody looked at went idle");
    wait("done");
}

/// OpenCode's plugin reports its changes only, never its end: run under a wrapper kraal does not
/// recognise (`sh -c 'opencode; …'`), the agent stays while the process its plugin reported
/// from runs, and goes when that process ends.
#[test]
fn a_hooked_agent_under_a_wrapper_stays_while_its_process_runs() {
    let env = Env::new("hooks-wrapped");
    env.workspace("w", "layout {\n    pane name=\"oc\"\n}\n");
    env.run(&["open", "w"]);
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let opencode = bin.join("opencode");
    let go = env.root.join("go");
    // Stands for OpenCode: its plugin runs `kraal hook opencode <state>` as a child of its own.
    write_script(
        &opencode,
        &format!(
            "#!/bin/sh\n'{}' hook opencode working\necho reported\nwhile [ ! -e '{}' ]; do sleep 0.1; done\n",
            env!("CARGO_BIN_EXE_kraal"),
            go.display()
        ),
    );
    let wrapped = format!("sh -c '{}; echo finished; sleep 30'", opencode.display());
    env.run(&["send", "-p", "oc", "--enter", &wrapped]);
    env.wait_for_text("oc", "reported");
    let agent = || {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let pane = panes.as_array().unwrap().iter().find(|p| p["name"] == "oc").cloned().unwrap();
        (pane["agent"].as_str().map(String::from), pane["state"].as_str().map(String::from))
    };
    // A few scans of the foreground (the wrapper, no agent) later, still there.
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(agent(), (Some("opencode".into()), Some("working".into())));
    std::fs::write(&go, "").unwrap();
    env.wait_for_text("oc", "finished");
    let start = Instant::now();
    while agent().0.is_some() {
        assert!(start.elapsed() < Duration::from_secs(5), "the agent outlived its process");
        std::thread::sleep(Duration::from_millis(100));
    }
    env.run(&["keys", "-p", "oc", "ctrl+c"]);
}

#[test]
fn agents_resume_after_a_restart_but_never_replay_a_one_shot_run() {
    let env = Env::new("resume");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let claude = bin.join("claude");
    write_script(&claude, "#!/bin/sh\necho started\nsleep 0.5\n");
    let claude = claude.display().to_string();
    let socket = env.root.join("run/kraal/resume.sock");
    let hook = |pane: &str, session: &str| {
        let mut c = env.cmd(&["hook", "claude"]);
        c.env("KRAAL_PANE", pane).env("KRAAL_SOCKET", &socket).stdin(std::process::Stdio::piped());
        let mut child = c.spawn().unwrap();
        use std::io::Write;
        let payload = format!(r#"{{"hook_event_name":"SessionStart","session_id":"{session}"}}"#);
        child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
        assert!(child.wait_with_output().unwrap().status.success());
    };
    let codex = bin.join("codex");
    write_script(&codex, "#!/bin/sh\necho started\nsleep 0.5\n");
    let codex = codex.display().to_string();
    // An interactive run and a one-shot one, each with a known conversation, both exited; one
    // whose words kraal cannot tell from a prompt; a one-shot run hidden behind an option kraal
    // cannot read; and a Codex fork, whose conversation is the fork's own.
    env.run(&["run", "-n", "chat", "--", &claude, "--model", "opus"]);
    env.run(&["run", "-n", "oneshot", "--", &claude, "-p", "perform deployment"]);
    env.run(&["run", "-n", "odd", "--", &claude, "--brand-new-flag", "Deploy now"]);
    env.run(&["run", "-n", "printed", "--", &claude, "--debug", "api", "--print", "Deploy release"]);
    env.run(&["run", "-n", "fork", "--", &codex, "fork", "parent-1", "Deploy release"]);
    hook("chat", "s-1");
    hook("oneshot", "s-2");
    hook("odd", "s-3");
    hook("printed", "s-5");
    let mut c = env.cmd(&["hook", "codex", r#"{"type":"agent-turn-complete","thread-id":"child-1"}"#]);
    c.env("KRAAL_PANE", "fork").env("KRAAL_SOCKET", &socket);
    assert!(c.status().unwrap().success());
    for name in ["chat", "oneshot", "odd", "printed", "fork"] {
        env.run(&["wait", "-p", name, "--until", "exit", "--timeout", "10"]);
    }
    env.run(&["kill"]);
    env.run(&["open", "w"]);
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let command = |name: &str| -> Vec<String> {
        let p = panes.as_array().unwrap().iter().find(|p| p["name"] == name).unwrap();
        p["command"].as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_string()).collect()
    };
    assert_eq!(command("chat"), [claude.as_str(), "--resume", "s-1", "--model", "opus"], "it resumes");
    assert_eq!(command("oneshot"), [claude.as_str(), "-p", "perform deployment"], "never run again as a resume");
    // Kept whole, and waiting: Enter runs it as shown.
    assert_eq!(command("odd"), [claude.as_str(), "--resume", "s-3", "--brand-new-flag", "Deploy now"]);
    let odd = panes.as_array().unwrap().iter().find(|p| p["name"] == "odd").unwrap();
    assert_eq!(odd["suspended"], true, "{odd}");
    assert_eq!(command("printed"), [claude.as_str(), "--debug", "api", "--print", "Deploy release"]);
    // The fork's own conversation, never the fork again with its prompt; waiting for Enter.
    assert_eq!(command("fork"), [codex.as_str(), "resume", "child-1"]);
    let fork = panes.as_array().unwrap().iter().find(|p| p["name"] == "fork").unwrap();
    assert_eq!(fork["suspended"], true, "{fork}");
}

#[test]
#[cfg(target_os = "linux")]
fn supervise_ends_what_its_program_left_however_it_left() {
    let env = Env::new("supervise");
    let gone = |pidfile: &Path| {
        let pid = std::fs::read_to_string(pidfile).unwrap().trim().to_string();
        let start = Instant::now();
        while Path::new(&format!("/proc/{pid}")).exists() {
            assert!(start.elapsed() < Duration::from_secs(5), "{pid} still runs");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    // Nothing holds the test's own output if a helper survives (the test then fails, not hangs).
    let quiet = |mut c: std::process::Command| {
        c.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        c
    };
    // A helper started with a cleared environment, in a session of its own, by a program that
    // then ends: an orphan nothing tags, and outside the program's group.
    let pidfile = env.root.join("helper.pid");
    let helper = format!("env -i setsid sh -c 'sleep 600 & echo $! > {}'", pidfile.display());
    let status = quiet(env.cmd(&["supervise", "sh", "-c", &format!("{helper}; sleep 0.3; exit 3")])).status().unwrap();
    assert_eq!(status.code(), Some(3), "the program's own status");
    gone(&pidfile);
    // Stopped while the program runs: the program and the helper both go.
    let pidfile = env.root.join("helper2.pid");
    let helper = format!("env -i setsid sh -c 'sleep 600 & echo $! > {}'", pidfile.display());
    let mut supervisor =
        quiet(env.cmd(&["supervise", "sh", "-c", &format!("{helper}; exec sleep 600")])).spawn().unwrap();
    let start = Instant::now();
    while std::fs::read_to_string(&pidfile).map_or(true, |p| p.trim().is_empty()) {
        assert!(start.elapsed() < Duration::from_secs(5), "the helper never started");
        std::thread::sleep(Duration::from_millis(20));
    }
    // SAFETY: plain syscall on our own child.
    unsafe {
        libc::kill(supervisor.id() as libc::pid_t, libc::SIGTERM);
    }
    let start = Instant::now();
    while supervisor.try_wait().unwrap().is_none() {
        assert!(start.elapsed() < Duration::from_secs(5), "the supervisor did not end");
        std::thread::sleep(Duration::from_millis(20));
    }
    gone(&pidfile);
}

#[test]
#[cfg(target_os = "linux")]
fn codex_usage_leaves_nothing_behind() {
    let env = Env::new("usage");
    let home = env.root.join("home");
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    std::fs::write(
        env.root.join("config/kraal/accounts.toml"),
        "[[account]]\nname = \"work\"\nagent = \"codex\"\nhome = \"~/.codex\"\n",
    )
    .unwrap();
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let pidfile = env.root.join("helper.pid");
    // A `codex` that leaves a helper behind (no environment, a session of its own), then answers.
    write_script(
        &bin.join("codex"),
        &format!(
            r#"#!/bin/sh
env -i setsid sh -c 'sleep 600 >/dev/null 2>&1 & echo $! > {}'
IFS= read -r line
printf '%s\n' '{{"id":1,"result":{{}}}}'
IFS= read -r line
IFS= read -r line
printf '%s\n' '{{"id":2,"result":{{"rateLimits":{{"primary":{{"usedPercent":42,"windowDurationMins":300}}}}}}}}'
sleep 600
"#,
            pidfile.display()
        ),
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let mut c = env.cmd(&["accounts", "refresh"]);
    c.env("PATH", path).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    let out = c.output().unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("42%"), "{text}");
    let pid = std::fs::read_to_string(&pidfile).unwrap().trim().to_string();
    let start = Instant::now();
    while Path::new(&format!("/proc/{pid}")).exists() {
        assert!(start.elapsed() < Duration::from_secs(5), "the helper Codex left still runs");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn an_agent_that_may_send_its_prompt_again_is_shown_before_it_restarts() {
    let env = Env::new("showfirst");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let claude = bin.join("claude");
    write_script(&claude, "#!/bin/sh\necho \"ran: $*\"\nsleep 0.3\n");
    let claude = claude.display().to_string();
    let socket = env.root.join("run/kraal/showfirst.sock");
    let mut client: Option<RawClient> = None;
    // An option kraal does not know; and `--worktree update`, where `update` names a worktree
    // (it is not Claude's `update` subcommand, and the conversation is resumed).
    for (pane, args, session) in [
        ("odd", ["--brand-new-flag", "Deploy now"].as_slice(), "s-9"),
        ("wt", ["--worktree", "update", "Deploy release"].as_slice(), "s-10"),
    ] {
        let mut run = vec!["run", "-n", pane, "--", claude.as_str()];
        run.extend_from_slice(args);
        env.run(&run);
        let mut c = env.cmd(&["hook", "claude"]);
        c.env("KRAAL_PANE", pane).env("KRAAL_SOCKET", &socket).stdin(std::process::Stdio::piped());
        let mut child = c.spawn().unwrap();
        use std::io::Write;
        let start = format!(r#"{{"hook_event_name":"SessionStart","session_id":"{session}"}}"#);
        child.stdin.take().unwrap().write_all(start.as_bytes()).unwrap();
        assert!(child.wait_with_output().unwrap().status.success());
        env.run(&["wait", "-p", pane, "--until", "exit", "--timeout", "10"]);
        env.run(&["focus", pane]);
        let client = client.get_or_insert_with(|| RawClient::attach(&env, "w"));
        env.wait_for_text(pane, "ran:");
        // The first Enter shows what would run; nothing runs.
        client.send(b"\r");
        let screen = env.wait_for_text(pane, "cannot tell");
        assert_eq!(screen.matches("ran:").count(), 1, "{pane}: {screen}");
        // The second runs it, exactly as shown (once the note is up: Enter before that goes to it).
        std::thread::sleep(Duration::from_millis(500));
        client.send(b"\r");
        // Lines may wrap in the pane (a space at a line end is lost): compare without spaces.
        let flat = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
        let expected = flat(&format!("ran: --resume {session} {}", args.join(" ")));
        let start = Instant::now();
        let screen = loop {
            let screen = flat(&env.run(&["read", "-p", pane]));
            if screen.contains(&expected) {
                break screen;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "{pane} never ran: {screen}");
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(screen.matches("ran:").count(), 2, "{pane}: {screen}");
    }
}

#[test]
fn layout_survives_a_server_restart() {
    let env = Env::new("restore");
    env.workspace(
        "fleet",
        r#"layout {
    tab name="servers" {
        pane split_direction="vertical" {
            pane name="a" command="sh" { args "-c" "echo node-a; exec sleep 1000"; }
            pane name="b" command="sh" { args "-c" "echo node-b; exec sleep 1000"; }
        }
    }
    tab name="tab 2" {
        pane name="shell"
    }
}
"#,
    );
    env.run(&["open", "fleet"]);
    env.wait_for_text("b", "node-b");
    env.run(&["rename", "-p", "shell", "renamed"]);
    let pid = env.server_pid().expect("the server runs");
    env.run(&["kill"]);
    // `kill` returns once the server has saved the layout and exited.
    assert!(env.server_pid().is_none(), "server {pid} still runs");
    assert!(env.state_file().exists());
    // Any command starting the server restores the saved layout.
    env.run(&["open", "fleet"]);
    env.wait_for_text("a", "node-a");
    let panes = env.run(&["panes"]);
    assert!(panes.contains("renamed"), "{panes}");
    assert_eq!(panes.lines().count(), 3, "{panes}");
    // A tab named "tab 2" keeps its name; older versions saved a tab without a name that way,
    // and in their states it shows what its pane runs.
    assert!(panes.contains("tab 2"), "{panes}");
    env.run(&["kill"]);
    let state = std::fs::read_to_string(env.state_file()).unwrap();
    let mut old: serde_json::Value = serde_json::from_str(&state).unwrap();
    old["version"] = serde_json::json!(1);
    std::fs::create_dir_all(env.legacy_state_file().parent().unwrap()).unwrap();
    std::fs::write(env.legacy_state_file(), serde_json::to_vec(&old).unwrap()).unwrap();
    std::fs::remove_file(env.state_file()).unwrap();
    env.run(&["open", "fleet"]);
    env.wait_for_text("a", "node-a");
    let panes = env.run(&["panes"]);
    assert!(!panes.contains("tab 2") && panes.contains("renamed"), "{panes}");
}

#[test]
fn legacy_workspace_state_migrates_before_opening_and_keeps_its_own_layout() {
    let env = Env::new("wsidentity");
    let source = "// Keep this source\nlayout { pane name=\"source-pane\"; }\n";
    env.workspace("w", source);
    env.run(&["open", "w"]);
    env.run(&["rename", "-p", "source-pane", "personal-pane"]);
    env.run(&["kill"]);
    let path = env.state_file();
    let mut old: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    old["version"] = serde_json::json!(1);
    old["revision"] = serde_json::json!(1);
    old["workspaces"][0].as_object_mut().unwrap().remove("id");
    let bytes = serde_json::to_vec_pretty(&old).unwrap();
    let path = env.legacy_state_file();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    std::fs::remove_file(env.state_file()).unwrap();
    let stopped = env.run(&["ls"]);
    assert_eq!(stopped.lines().filter(|line| line.starts_with(&env.session)).count(), 1, "{stopped}");
    env.run(&["open", "w"]);
    let list = || serde_json::from_str::<serde_json::Value>(&env.run(&["workspaces", "--json"])).unwrap();
    let id = list()[0]["id"].as_str().unwrap().to_owned();
    assert!(!id.is_empty());
    assert!(env.run(&["panes"]).contains("personal-pane"), "the saved layout wins over the KDL");
    let backups = || {
        std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("wsidentity.json.before-0.4-"))
            .collect::<Vec<_>>()
    };
    assert_eq!(backups().len(), 1);
    assert_eq!(std::fs::read(backups()[0].join("state.json")).unwrap(), bytes);
    env.run(&["kill"]);
    env.run(&["open", "w"]);
    assert_eq!(list()[0]["id"], id);
    assert_eq!(backups().len(), 1);
    assert_eq!(std::fs::read_to_string(env.root.join("config/kraal/workspaces/w.kdl")).unwrap(), source);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    env.run(&["kill"]);
    let stopped = env.run(&["ls"]);
    assert_eq!(stopped.lines().filter(|line| line.starts_with(&env.session)).count(), 1, "{stopped}");
    env.run(&["forget"]);
    assert!(!path.exists() && !env.state_file().exists());
    assert_eq!(std::fs::read(backups()[0].join("state.json")).unwrap(), bytes);
    assert!(!env.run(&["ls"]).contains(&env.session));
}

#[test]
fn a_failed_state_migration_never_starts_an_empty_session_or_replaces_the_source() {
    let env = Env::new("wsbadstate");
    env.workspace("w", "layout { pane; }\n");
    std::fs::create_dir_all(env.legacy_state_file().parent().unwrap()).unwrap();
    for bytes in ["{", r#"{"version":2,"workspaces":[]}"#, r#"{"version":1,"revision":3,"workspaces":[]}"#] {
        std::fs::write(env.legacy_state_file(), bytes).unwrap();
        let out = env.try_run(&["open", "w"]);
        let said = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && said.contains("the saved session is unchanged"), "{said}");
        assert_eq!(std::fs::read_to_string(env.legacy_state_file()).unwrap(), bytes);
        assert!(env.server_pid().is_none());
    }
    // Repairing the original is sufficient; no hidden replacement needs to be removed first.
    std::fs::write(env.legacy_state_file(), r#"{"version":1,"workspaces":[]}"#).unwrap();
    env.run(&["open", "w"]);
    assert!(env.server_pid().is_some());
}

#[test]
fn account_migration_refuses_unreadable_sessions_before_changing_the_account_file() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new("unreadable-account-state");
    let accounts = env.root.join("config/kraal/accounts.toml");
    let source = "[[account]]\nname = 'work'\nagent = 'claude'\nhome = '/work'\n";
    std::fs::write(&accounts, source).unwrap();
    for path in [env.state_file(), env.legacy_state_file()] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"version":1,"workspaces":[]}"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let out = env.try_run(&["accounts", "rename", "claude:work", "renamed"]);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && error.contains("accounts.toml is unchanged"), "{error}");
        assert_eq!(std::fs::read_to_string(&accounts).unwrap(), source);
        assert!(!accounts.with_extension("toml.kraal-bak").exists());
        std::fs::remove_file(path).unwrap();
    }
}

#[test]
fn newer_state_is_refused_without_falling_back_or_overwriting_either_format() {
    let env = Env::new("future-state");
    env.workspace("w", "layout { pane; }\n");
    let current = env.state_file();
    let legacy = env.legacy_state_file();
    let future = br#"{"version":3,"future_layout":[]}"#;
    let original = br#"{"version":1,"workspaces":[]}"#;
    for (path, bytes) in [(&current, future.as_slice()), (&legacy, original.as_slice())] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    for args in [&["workspaces", "--json"][..], &["open", "w"]] {
        let out = env.try_run(args);
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && error.contains("use a newer kraal"), "{error}");
        assert_eq!(std::fs::read(&current).unwrap(), future);
        assert_eq!(std::fs::read(&legacy).unwrap(), original);
        assert!(env.server_pid().is_none());
    }
}

#[test]
fn save_writes_a_definition_that_reopens() {
    let env = Env::new("save");
    env.workspace("w", "layout {\n    pane split_direction=\"vertical\" {\n        pane name=\"left\"\n        pane name=\"right\"\n    }\n}\n");
    env.run(&["open", "w"]);
    let out = env.run(&["save", "copy", "--workspace", "w"]);
    assert!(out.contains("copy.kdl"), "{out}");
    let list = env.run(&["workspaces"]);
    assert!(list.contains("copy"), "{list}");
    env.run(&["open", "copy"]);
    let json = env.run(&["panes", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let lefts = v.as_array().unwrap().iter().filter(|p| p["name"] == "left").count();
    assert_eq!(lefts, 2, "one in each workspace: {json}");
    // What has not run yet still waits, and a tab of floating panes alone is kept.
    env.workspace(
        "w2",
        "layout {\n    tab {\n        pane name=\"tiled\"\n        floating_panes {\n            pane name=\"float\" command=\"sleep\" start_suspended=true { args \"100\"; }\n        }\n    }\n}\n",
    );
    env.run(&["open", "w2"]);
    env.run(&["close", "w2:tiled"]);
    env.run(&["save", "copy2", "--workspace", "w2"]);
    let saved = std::fs::read_to_string(env.root.join("config/kraal/workspaces/copy2.kdl")).unwrap();
    assert!(saved.contains("name=\"float\"") && saved.contains("start_suspended=true"), "{saved}");
    // A name is a file name in the definitions directory, nothing else.
    for name in ["../escape", "/tmp/escape", ".hidden"] {
        let out = env.try_run(&["save", name, "--workspace", "w"]);
        assert!(!out.status.success(), "{name} was accepted");
    }
    assert!(!env.root.join("config/kraal/escape.kdl").exists());
}

/// Saves never take another workspace's source. Other existing files need a confirmation
/// which preserves the input on Cancel and expires when the file changes again.
#[test]
fn workspace_creation_failures_keep_input_and_retry_from_both_entries_at_both_sizes() {
    let env = Env::new("wscreateerrors");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    let shell = env.root.join("sh");
    let held = env.root.join("sh-held");
    for size in [(80, 24), (124, 36)] {
        for picker in [false, true] {
            let name = format!("scratch-{}-{picker}", size.0);
            let before = listed_workspaces(&env);
            let panes = env.run(&["panes"]);
            let mut client = RawClient::attach_sized(&env, Some("w"), size);
            client.wait_for_message("\"attached\"");
            std::fs::rename(&shell, &held).unwrap();
            if picker {
                client.send(b"\x02w");
                client.wait_for_row("Workspaces");
            } else {
                client.send(b"\x02Wn");
                client.wait_for_row("Name    ");
            }
            if picker {
                client.send(name.as_bytes());
                let rows = client.wait_for_row("Enter Add");
                assert!(!rows.iter().any(|r| r.contains("Tab Actions")));
                client.send(b"\r");
                client.wait_for_row("Name    ");
                assert_eq!(listed_workspaces(&env), before);
            }
            client.send(format!("\x01\x0b {name} ").as_bytes());
            client.send(b"\r");
            let rows = client.wait_for_row("Enter Retry");
            for text in [&name, "Add workspace", "Couldn't start the shell.", "No workspace was added.", "Esc Cancel"] {
                assert!(rows.iter().any(|r| r.contains(text)), "{size:?}: {}", rows.join("\n"));
            }
            assert_eq!(listed_workspaces(&env), before);
            assert_eq!(env.run(&["panes"]), panes);
            // Cursor movement keeps the error; an edit clears it and keeps the cursor's place.
            client.send(b"\x1b[D");
            client.settle();
            assert!(client.rows().iter().any(|r| r.contains("Enter Retry")));
            client.send(b"-fixed");
            client.wait_for_row("Enter Add");
            let fixed = format!("{name}-fixed");
            client.send(b"\r");
            client.wait_for_row("Enter Retry");
            assert_eq!(listed_workspaces(&env), before);
            std::fs::rename(&held, &shell).unwrap();
            client.send(b"\r");
            client.wait_for_rows("the creation prompt closed", |rows| !rows.iter().any(|r| r.contains("Name    ")));
            let workspaces = listed_workspaces(&env);
            assert_eq!(workspaces.len(), before.len() + 1);
            assert_eq!(workspaces.iter().filter(|w| *w == &format!("{fixed}:  (open)")).count(), 1);
            assert_eq!(env.run(&["panes"]).lines().count(), panes.lines().count() + 1);
        }
    }
}

#[test]
fn workspace_search_add_and_closed_rename_work_at_both_sizes() {
    for size in [(80, 24), (124, 36)] {
        let env = Env::new(&format!("ws-list-{}", size.0));
        let folder = env.root.join("project");
        std::fs::create_dir(&folder).unwrap();
        let source = format!("layout {{ cwd {:?}; pane name=\"shell\"; }}\n", folder.display().to_string());
        env.workspace("source", &source);
        env.run(&["open", "source"]);
        let path = env.root.join("config/kraal/workspaces/source.kdl");
        let mut client = RawClient::attach_sized(&env, Some("source"), size);
        client.wait_for_message("\"attached\"");
        client.wait_for_row("$ ");
        if size.0 == 80 {
            click_account_text(&mut client, "source");
        } else {
            click_account_text(&mut client, "WORKSPACES");
        }
        client.wait_for_row("Search");
        client.send(b"billing");
        client.wait_for_row("No matching workspaces.");
        assert_eq!(listed_workspaces(&env), ["source: source.kdl (open)"]);
        click_account_text(&mut client, "+ Add workspace");
        client.wait_for_row("Name    billing");
        client.wait_for_row("Starts in");
        assert_eq!(listed_workspaces(&env), ["source: source.kdl (open)"]);
        client.send(b"\x1b");
        client.wait_for_row("Search  billing");
        client.send(b"\r");
        client.wait_for_row("Name    billing");
        click_account_text(&mut client, " Add ");
        client.wait_for_row("Workspace added");
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let added = panes.as_array().unwrap().iter().find(|p| p["workspace"] == "billing").unwrap();
        assert_eq!(Path::new(added["cwd"].as_str().unwrap()).canonicalize().unwrap(), folder.canonicalize().unwrap());
        client.wait_for_row("$ ");
        client.send(b"\x02Wx");
        client.wait_for_row("Closed · 1 tab · 1 pane");
        let before = env.run(&["panes", "--json"]);
        let list: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
        let id = list.as_array().unwrap().iter().find(|w| w["name"] == "billing").unwrap()["id"].clone();
        client.send(b"\t");
        client.wait_for_row("Rename");
        client.send(b"\x1b[B\r");
        client.wait_for_row("Rename workspace");
        client.send(b"\x01\x0bbilling/review\r");
        client.wait_for_row("Workspace renamed");
        assert_eq!(env.run(&["panes", "--json"]), before, "renaming a closed workspace starts no programs");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        assert!(!path.with_extension("kdl.bak").exists());
        env.run(&["kill"]);
        env.run(&["open", "source"]);
        let list: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
        let renamed = list.as_array().unwrap().iter().find(|w| w["name"] == "billing/review").unwrap();
        assert_eq!(renamed["id"], id);
        assert_eq!(renamed["open"], false);
        assert_eq!(std::fs::read_to_string(path).unwrap(), source);
    }
}

#[test]
fn workspace_duplicate_is_saved_closed_and_starts_commands_only_on_enter() {
    for size in [(80, 24), (124, 36)] {
        let env = Env::new(&format!("ws-duplicate-{}", size.0));
        let bin = env.root.join("bin");
        let account = env.root.join("agent-home");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&account).unwrap();
        write_script(
            &bin.join("claude"),
            r#"#!/bin/sh
[ "$2" = status ] && exit 0
mkdir -p "$CLAUDE_CONFIG_DIR/projects/work"
printf '{"message":1}\n' >> "$CLAUDE_CONFIG_DIR/projects/work/$2.jsonl"
printf '%s\n' "$*" >> "$CLAUDE_CONFIG_DIR/launches"
echo original-agent-output
while IFS= read -r line; do :; done
"#,
        );
        std::fs::write(
            env.root.join("config/kraal/accounts.toml"),
            format!(
                "[[account]]\nid = \"test-claude-work\"\nname = \"work\"\nagent = \"claude\"\nhome = {:?}\n",
                account.display().to_string()
            ),
        )
        .unwrap();
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        let job = format!("echo job >> {}/jobs; echo original-job-output; exec sleep 100", env.root.display());
        let source = format!(
            r#"// Keep the recipe unchanged.
layout {{
    cwd {:?}
    tab name="work" focus=true {{
        pane name="shell" focus=true
        pane name="job" command="sh" {{ args "-c" {:?}; }}
    }}
    tab name="agent" {{ pane name="agent" agent="claude:work" account_id="test-claude-work" {{ args "--model" "opus"; }}; }}
}}
"#,
            env.root.display().to_string(),
            job
        );
        env.workspace("api", &source);
        env.workspace("api-copy", "layout { pane; }\n");
        let opened = env.cmd(&["open", "api"]).env("PATH", &path).output().unwrap();
        assert!(opened.status.success(), "{}", String::from_utf8_lossy(&opened.stderr));
        env.wait_for_text("job", "original-job-output");
        env.wait_for_text("agent", "original-agent-output");
        let original_launch = std::fs::read_to_string(account.join("launches")).unwrap();
        assert!(original_launch.contains("--session-id"));
        let original_id =
            serde_json::from_str::<serde_json::Value>(&env.run(&["workspaces", "--json"])).unwrap()[0]["id"].clone();
        let mut client = RawClient::attach_sized(&env, Some("api"), size);
        client.wait_for_row("$ ");
        // Output can arrive before the periodic scan recognises the agent.
        wait_until(PATIENCE, "the original agent was not detected as idle", || {
            let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
            panes
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["name"] == "agent" && p["agent"] == "claude" && p["state"] == "idle")
        });
        let original_panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        client.send(b"\x02wapi\t");
        click_account_text(&mut client, "Duplicate");
        client.wait_for_row("Name    api-copy-2");
        client.wait_for_row("Programs wait for you to start them.");
        client.send(b"\x1b");
        client.wait_for_row("Search  api");
        client.send(b"\t");
        click_account_text(&mut client, "Duplicate");
        client.wait_for_row("Name    api-copy-2");
        client.send(b"\r");
        client.wait_for_row("Workspace duplicated");
        let list: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
        let copy = list.as_array().unwrap().iter().find(|w| w["name"] == "api-copy-2").unwrap();
        assert_eq!(copy["open"], false);
        assert_eq!(copy["tabs"], 2);
        assert_eq!(copy["panes"], 3);
        assert_ne!(copy["id"], original_id);
        assert!(copy["definition"].is_null());
        assert_eq!(serde_json::from_str::<serde_json::Value>(&env.run(&["panes", "--json"])).unwrap(), original_panes);
        let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(env.state_file()).unwrap()).unwrap();
        assert!(saved["workspaces"].as_array().unwrap().iter().any(|w| w["id"] == copy["id"] && w["closed"] == true));
        env.run(&["kill"]);
        let opened = env.cmd(&["open", "api"]).env("PATH", &path).output().unwrap();
        assert!(opened.status.success());
        env.wait_for_text_since_restore("agent", "original-agent-output");
        let launches = std::fs::read_to_string(account.join("launches")).unwrap();
        let jobs = std::fs::read_to_string(env.root.join("jobs")).unwrap();
        let list: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
        let restarted_copy = list.as_array().unwrap().iter().find(|w| w["name"] == "api-copy-2").unwrap();
        assert_eq!(restarted_copy["id"], copy["id"]);
        assert_eq!(restarted_copy["open"], false);
        env.run(&["open", "api-copy-2"]);
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let current_source: Vec<_> = panes.as_array().unwrap().iter().filter(|p| p["workspace"] == "api").collect();
        let copied: Vec<_> = panes.as_array().unwrap().iter().filter(|p| p["workspace"] == "api-copy-2").collect();
        assert_eq!(copied.len(), 3);
        for p in &copied {
            assert!(current_source.iter().all(|original| original["id"] != p["id"]));
            if p["name"] != "shell" {
                assert_eq!(p["suspended"], true);
                let output = env.run(&["read", "-p", &p["id"].to_string()]);
                assert!(!output.contains("original-agent-output"));
            }
        }
        assert_eq!(std::fs::read_to_string(account.join("launches")).unwrap(), launches);
        assert_eq!(std::fs::read_to_string(env.root.join("jobs")).unwrap(), jobs);
        let shell = copied.iter().find(|p| p["name"] == "shell").unwrap();
        env.run(&["send", "-p", &shell["id"].to_string(), "--enter", "printf 'copy-shell-%s\\n' ready"]);
        env.wait_for_text(&shell["id"].to_string(), "copy-shell-ready");
        let agent = copied.iter().find(|p| p["name"] == "agent").unwrap();
        assert_eq!(agent["account_id"], current_source.iter().find(|p| p["name"] == "agent").unwrap()["account_id"]);
        let mut client = RawClient::attach_sized(&env, Some("api-copy-2"), size);
        client.wait_for_message("\"attached\"");
        client.send(format!("\x1b]7701;show;{}\x07", agent["id"]).as_bytes());
        client.wait_for_row("--session-id");
        client.wait_for_row("enter run");
        client.send(b"\r");
        wait_until(PATIENCE, "the copied agent did not start", || {
            std::fs::read_to_string(account.join("launches")).unwrap().lines().count() == launches.lines().count() + 1
        });
        let all_launches = std::fs::read_to_string(account.join("launches")).unwrap();
        let copied_launch = all_launches.lines().last().unwrap();
        let copied_session = copied_launch.split_whitespace().nth(1).unwrap();
        assert_eq!(copied_launch, format!("--session-id {copied_session} --model opus"));
        assert_ne!(Some(copied_session), original_launch.split_whitespace().nth(1));
        assert_eq!(std::fs::read_to_string(env.root.join("jobs")).unwrap(), jobs);
        let job = copied.iter().find(|p| p["name"] == "job").unwrap();
        client.send(format!("\x1b]7701;show;{}\x07", job["id"]).as_bytes());
        client.wait_for_row("echo job");
        client.wait_for_row("enter run");
        client.send(b"\r");
        wait_until(PATIENCE, "the copied command did not start", || {
            std::fs::read_to_string(env.root.join("jobs")).unwrap().lines().count() == jobs.lines().count() + 1
        });
        let source_path = env.root.join("config/kraal/workspaces/api.kdl");
        assert_eq!(std::fs::read_to_string(&source_path).unwrap(), source);
        assert!(!source_path.with_extension("kdl.bak").exists());
        assert!(!env.root.join("config/kraal/workspaces/api-copy-2.kdl").exists());

        // The fake agent writes a transcript but never sends hooks. Close/Open and a server
        // restart must both recover the copy's conversation, without borrowing the source's.
        // The question lists the programs as the periodic scan knows them, and asks again when
        // that list changed under it: answer once the scan has recognised the copied agent.
        wait_until(PATIENCE, "the copied agent was not detected as idle", || {
            let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
            panes
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["id"] == agent["id"] && p["agent"] == "claude" && p["state"] == "idle")
        });
        client.send(b"\x02Wx");
        client.wait_for_row("The layout will be kept.");
        client.wait_for_row("claude: idle");
        client.send(b"y");
        client.wait_for_row("Closed · 2 tabs · 3 panes");
        env.run(&["open", "api-copy-2"]);
        let resumed = format!("--resume {copied_session} --model opus");
        wait_until(PATIENCE, "Close/Open did not resume the copied conversation without hooks", || {
            std::fs::read_to_string(account.join("launches")).unwrap().lines().filter(|l| *l == resumed).count() == 1
        });
        env.run(&["kill"]);
        let opened = env.cmd(&["open", "api-copy-2"]).env("PATH", &path).output().unwrap();
        assert!(opened.status.success());
        wait_until(PATIENCE, "server restart did not resume the copied conversation without hooks", || {
            std::fs::read_to_string(account.join("launches")).unwrap().lines().filter(|l| *l == resumed).count() == 2
        });
    }
}

#[test]
fn workspace_moves_keep_focus_share_the_list_and_survive_restart() {
    for size in [(80, 24), (124, 36)] {
        let env = Env::new(&format!("ws-order-{}", size.0));
        let docs = "// Keep this recipe.\nlayout { pane command=\"true\" start_suspended=true; }\n";
        env.workspace("api", "layout { pane name=\"api-shell\"; }\n");
        env.workspace("docs", docs);
        env.workspace("web", "layout { pane name=\"web-shell\"; }\n");
        for name in ["api", "docs", "web"] {
            env.run(&["open", name]);
        }
        let mut client = RawClient::attach_sized(&env, Some("docs"), size);
        client.wait_for_message("\"attached\"");
        client.send(b"\x02Wx");
        client.wait_for_row("Closed · 1 tab · 1 pane");
        let mut other = RawClient::attach_sized(&env, Some("web"), size);
        other.wait_for_row("$ ");
        other.send(b"\x02w");
        other.wait_for_row("Search");
        let panes = env.run(&["panes", "--json"]);
        client.send(b"docs\t");
        click_account_text(&mut client, "Clear search to reorder");
        assert_eq!(listed_workspaces(&env), ["api: api.kdl (open)", "docs: docs.kdl", "web: web.kdl (open)"]);
        client.send(b"\t");
        click_account_text(&mut client, "Move up");
        let reordered = ["docs: docs.kdl", "api: api.kdl (open)", "web: web.kdl (open)"];
        wait_until(PATIENCE, "workspace order was not saved", || listed_workspaces(&env) == reordered);
        other.wait_for_rows("the other client's list has the same order", |rows| {
            let names: Vec<_> = rows
                .iter()
                .filter_map(|row| {
                    let row = row.split('│').nth(2)?.trim();
                    ["docs", "api", "web"].into_iter().find(|name| row.ends_with(name))
                })
                .collect();
            names.windows(3).any(|rows| rows == ["docs", "api", "web"])
        });
        assert_eq!(env.run(&["panes", "--json"]), panes, "moving a closed workspace starts no programs");
        client.close_overlay("Search");
        other.close_overlay("Search");
        client.send(b"printf 'focus-api\\n'\r");
        other.send(b"printf 'focus-web\\n'\r");
        env.wait_for_text("api-shell", "focus-api");
        env.wait_for_text("web-shell", "focus-web");
        assert!(!env.run(&["read", "-p", "api-shell"]).contains("focus-web"));
        assert!(!env.run(&["read", "-p", "web-shell"]).contains("focus-api"));
        let before: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
        env.run(&["kill"]);
        assert_eq!(listed_workspaces(&env), ["docs: docs.kdl", "api: api.kdl", "web: web.kdl"]);
        env.run(&["open", "api"]);
        assert_eq!(listed_workspaces(&env), reordered);
        let after: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
        for i in 0..3 {
            assert_eq!(after[i]["id"], before[i]["id"]);
        }
        assert_eq!(std::fs::read_to_string(env.root.join("config/kraal/workspaces/docs.kdl")).unwrap(), docs);
        assert!(!env.root.join("config/kraal/workspaces/docs.kdl.bak").exists());
    }
}

#[test]
fn workspace_delete_undo_and_restore_after_restart_keep_both_reused_names() {
    for size in [(80, 24), (124, 36)] {
        let env = Env::new(&format!("ws-delete-{}", size.0));
        let source = "// shared layout\nlayout { pane name=\"original\"; }\n";
        env.workspace("api", source);
        env.run(&["open", "api"]);
        let original: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
        let identity = original[0]["id"].clone();
        let path = env.root.join("config/kraal/workspaces/api.kdl");
        let mut client = RawClient::attach_sized(&env, Some("api"), size);
        client.wait_for_row("$ ");
        client.send(b"\x02w\t");
        click_account_text(&mut client, "Delete");
        client.wait_for_row("Delete api?");
        client.wait_for_row("Project files will be kept.");
        client.send(b"\r");
        client.wait_for_row("Search");
        assert_eq!(env.run(&["workspaces", "--json"]), serde_json::to_string_pretty(&original).unwrap() + "\n");
        client.send(b"\t");
        click_account_text(&mut client, "Delete");
        client.wait_for_row("Delete api?");
        client.send(b"y");
        client.wait_for_row("Ctrl+Z Undo");
        assert!(listed_workspaces(&env).is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        client.send(b"\x1a");
        client.wait_for_row("Workspace restored");
        let restored: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
        assert_eq!(restored[0]["id"], identity);
        assert_eq!(restored[0]["open"], false);
        assert_eq!(env.run(&["panes", "--json"]).trim(), "[]");
        client.send(b"\t");
        click_account_text(&mut client, "Delete");
        client.wait_for_row("Delete api?");
        client.send(b"y");
        client.wait_for_row("Workspace deleted");
        env.run(&["kill"]);
        assert!(listed_workspaces(&env).is_empty(), "the stopped-session scan must also exclude the source");
        env.run(&["tunnel", "list"]);
        let mut client = RawClient::attach_sized(&env, None, size);
        client.wait_for_row("Deleted workspaces (1)");
        assert_eq!(env.run(&["panes", "--json"]).trim(), "[]", "attaching must not add a replacement workspace");
        click_account_text(&mut client, "+ Add workspace");
        client.wait_for_row("Add workspace");
        client.send(b"\x01\x0bapi\r");
        client.wait_for_row("Workspace added");
        let replacement: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
        assert_ne!(replacement[0]["id"], identity);
        let panes = env.run(&["panes", "--json"]);
        client.send(b"\x02w");
        click_account_text(&mut client, "Deleted workspaces (1)");
        client.wait_for_row("Deleted today");
        client.send(b"\r");
        client.wait_for_row("Name    api-restored");
        client.send(b"\x1b");
        client.wait_for_row("Deleted today");
        client.send(b"\r");
        client.wait_for_row("Restore workspace");
        client.send(b"\r");
        client.wait_for_row("Workspace restored");
        let restored: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
        let restored = restored.as_array().unwrap();
        let old = restored.iter().find(|w| w["name"] == "api-restored").unwrap();
        let new = restored.iter().find(|w| w["name"] == "api").unwrap();
        assert_eq!(old["id"], identity);
        assert_eq!(old["open"], false);
        assert_eq!(new["id"], replacement[0]["id"]);
        assert_eq!(env.run(&["panes", "--json"]), panes, "Restore starts no programs");
        assert_eq!(std::fs::read_to_string(path).unwrap(), source);
    }
}

#[test]
fn deleting_a_scanned_source_never_runs_it_and_restores_its_saved_contents() {
    let env = Env::new("ws-delete-source");
    let marker = env.root.join("ran");
    let command = format!("echo started > {}", marker.display());
    let source = format!("layout {{ pane command=\"sh\" {{ args \"-c\" {command:?}; }}; }}\n");
    env.workspace("recipe", &source);
    env.run(&["tunnel", "list"]);
    let mut client = RawClient::attach_sized(&env, None, (80, 24));
    client.wait_for_message("\"attached\"");
    client.send(b"\x02wrecipe\t");
    click_account_text(&mut client, "Delete");
    client.wait_for_row("Delete recipe?");
    client.send(b"y");
    client.wait_for_row("Workspace deleted");
    assert!(!marker.exists());
    let path = env.root.join("config/kraal/workspaces/recipe.kdl");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
    env.run(&["kill"]);
    assert!(!env.run(&["workspaces", "--json"]).contains("recipe"));
    std::fs::remove_file(&path).unwrap();
    env.run(&["tunnel", "list"]);
    let mut client = RawClient::attach_sized(&env, None, (80, 24));
    client.wait_for_message("\"attached\"");
    client.send(b"\x02w");
    click_account_text(&mut client, "Deleted workspaces (1)");
    client.wait_for_row("recipe");
    client.send(b"\r");
    client.wait_for_row("Workspace restored");
    assert!(!marker.exists());
    assert!(!path.exists());
    let restored: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    let recipe = restored.as_array().unwrap().iter().find(|w| w["name"] == "recipe").unwrap();
    assert_eq!(recipe["open"], false);
    assert_eq!(recipe["panes"], 1);
    env.run(&["open", "recipe"]);
    wait_until(PATIENCE, "the explicitly opened recipe did not run", || marker.exists());
}

#[test]
fn workspace_save_errors_keep_the_name_and_full_reason_at_both_sizes() {
    let env = Env::new("wssaveerrors");
    let dir = env.root.join("workspaces-link");
    std::os::unix::fs::symlink(env.root.join("config/kraal/workspaces"), &dir).unwrap();
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    for size in [(80, 24), (124, 36)] {
        let name = format!("copy-{}-a  b-{}", size.0, "long-name-".repeat(5));
        let path = dir.join(format!("{name}.kdl"));
        std::fs::create_dir(&path).unwrap();
        let mut client = RawClient::attach_sized(&env, Some("w"), size);
        client.wait_for_message("\"attached\"");
        client.send(b"\x02Ws");
        client.wait_for_row("Export layout");
        client.send(format!("\x01\x0b {}\r", path.display()).as_bytes());
        let expected = format!("{} is not a regular file", path.canonicalize().unwrap().display());
        let rows = client.wait_for_rows("the full export error and Retry", |rows| {
            // Remove only the prompt's padding: a path can continue across rows and contain spaces.
            let message: String = rows
                .iter()
                .filter_map(|r| r.split('│').nth(2))
                .filter_map(|r| r.strip_prefix(' ')?.strip_suffix(' '))
                .collect();
            message.contains(&expected) && rows.iter().any(|r| r.contains("Enter Retry"))
        });
        assert!(rows.iter().any(|r| r.contains("Esc Cancel")));
        std::fs::remove_dir(&path).unwrap();
        client.send(b"\r");
        client.wait_for_row("Layout exported");
        assert!(path.is_file());
        assert!(!path.with_extension("kdl.bak").exists());
        assert_eq!(env.run(&["panes"]).lines().count(), 1);
    }
}

#[test]
fn workspace_saves_check_ownership_and_confirm_the_exact_file() {
    let env = Env::new("wssavechecked");
    let dir = env.root.join("config/kraal/workspaces");
    let old = "layout {\n    pane name=\"original\"\n}\n";
    env.workspace("a", old);
    env.workspace("b", old);
    env.workspace("copy", old);
    env.run(&["open", "a"]);
    env.run(&["open", "b"]);
    let refused = env.try_run(&["save", "b", "--workspace", "a"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("belongs to workspace"));
    assert_eq!(std::fs::read_to_string(dir.join("b.kdl")).unwrap(), old);
    let refused = env.try_run(&["save", "copy", "--workspace", "a"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("confirm replacement"));
    assert!(!dir.join("copy.kdl.bak").exists());

    let mut client = RawClient::attach(&env, "a");
    client.wait_for_message("\"attached\"");
    client.send(b"\x02Ws");
    client.wait_for_row("Export layout");
    client.send(format!("\x01\x0b{}\r", dir.join("copy.kdl").display()).as_bytes());
    client.wait_for_row("Replace file?");
    client.send(b"\r");
    client.wait_for_row("Export layout");
    assert_eq!(std::fs::read_to_string(dir.join("copy.kdl")).unwrap(), old);
    client.send(b"\r");
    client.wait_for_row("Replace file?");
    std::fs::write(dir.join("copy.kdl"), "edited after confirmation").unwrap();
    client.send(b"y");
    // The reason stays visible even when the full destination is wider than the prompt.
    client.wait_for_row("changed while saving");
    client.send(b"\r");
    client.wait_for_row("Replace file?");
    client.send(b"y");
    client.wait_for_row("Layout exported");
    assert_eq!(std::fs::read_to_string(dir.join("copy.kdl.bak")).unwrap(), "edited after confirmation");
    assert_eq!(listed_workspaces(&env), ["a: a.kdl (open)", "b: b.kdl (open)"]);
    // Saving the linked source after a manual edit also asks, without restarting its panes.
    std::fs::write(dir.join("a.kdl"), "invalid KDL from an editor").unwrap();
    client.send(b"\x1b\x1b\x02Ws");
    client.wait_for_row("Export layout");
    client.send(format!("\x01\x0b{}\r", dir.join("a.kdl").display()).as_bytes());
    client.wait_for_row("File changed");
    client.send(b"y");
    client.wait_for_row("Layout exported");
    wait_until(PATIENCE, "the confirmed source was not saved", || {
        std::fs::read_to_string(dir.join("a.kdl.bak")).is_ok_and(|s| s == "invalid KDL from an editor")
    });
    assert_eq!(env.run(&["panes"]).lines().count(), 2);
}

#[test]
fn a_cli_save_persists_its_first_link_before_the_periodic_save() {
    let env = Env::new("wssavelink");
    env.workspace("base", "layout { pane; }\n");
    env.run(&["open", "base"]);
    let mut client = RawClient::attach(&env, "base");
    client.wait_for_message("\"attached\"");
    client.send(b"\x02Wn");
    client.wait_for_row("Name    ");
    client.send(b"\x01\x0bscratch\r");
    wait_until(PATIENCE, "scratch was not created", || listed_workspaces(&env).iter().any(|w| w == "scratch:  (open)"));
    env.run(&["save", "linked", "--workspace", "scratch"]);
    wait_until(Duration::from_secs(10), "the CLI link was not persisted promptly", || {
        let Ok(text) = std::fs::read_to_string(env.state_file()) else { return false };
        let Ok(state) = serde_json::from_str::<serde_json::Value>(&text) else { return false };
        state["workspaces"].as_array().is_some_and(|list| {
            list.iter()
                .any(|w| w["name"] == "scratch" && w["definition"].as_str().is_some_and(|p| p.ends_with("/linked.kdl")))
        })
    });
    env.run(&["kill"]);
    env.run(&["open", "linked"]);
    assert_eq!(listed_workspaces(&env), ["base: base.kdl (open)", "scratch: linked.kdl (open)"]);
}

/// Independent sessions share files, not revisions. They cannot overwrite each other's saves,
/// including when both try to create the same copy at once.
#[test]
fn two_sessions_cannot_silently_replace_each_others_layouts() {
    let first = Env::new("wssavefirst");
    let second = Env::new("wssavesecond");
    let shared = first.root.join("config/kraal/workspaces");
    let alias = second.root.join("config/kraal/workspaces");
    std::fs::remove_dir(&alias).unwrap();
    std::os::unix::fs::symlink(&shared, &alias).unwrap();
    let old = "layout {\n    pane name=\"shared\"\n}\n";
    first.workspace("work", old);
    first.run(&["open", "work"]);
    second.run(&["open", "work"]);
    first.run(&["run", "--name", "first", "--", "sleep", "100"]);
    second.run(&["run", "--name", "second", "--", "sleep", "100"]);
    first.run(&["export", shared.join("work.kdl").to_str().unwrap(), "--workspace", "work", "--replace"]);
    let winner = std::fs::read_to_string(shared.join("work.kdl")).unwrap();
    let refused = second.try_run(&["save", "--workspace", "work"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("File changed"));
    assert_eq!(std::fs::read_to_string(shared.join("work.kdl")).unwrap(), winner);
    assert_eq!(std::fs::read_to_string(shared.join("work.kdl.bak")).unwrap(), old);
    // Restarting cannot silently adopt the version written by the other session.
    second.run(&["kill"]);
    second.run(&["open", "work"]);
    assert!(!second.try_run(&["save", "--workspace", "work"]).status.success());
    let mut one = first.cmd(&["save", "copy", "--workspace", "work"]);
    let mut two = second.cmd(&["save", "copy", "--workspace", "work"]);
    one.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    two.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let one = one.spawn().unwrap();
    let two = two.spawn().unwrap();
    let (one, two) = (one.wait_with_output().unwrap(), two.wait_with_output().unwrap());
    assert_ne!(one.status.success(), two.status.success(), "{one:?} {two:?}");
    let copy = std::fs::read_to_string(shared.join("copy.kdl")).unwrap();
    let expected = if one.status.success() { "name=\"first\"" } else { "name=\"second\"" };
    assert!(copy.contains(expected), "{copy}");
    assert!(!shared.join("copy.kdl.bak").exists());
}

/// `kraal save` run in a pane saves that pane's workspace, as every command run in a pane acts
/// where it runs: not the workspace a terminal shows, nor the first one. `--workspace` names
/// another, and outside a pane the one on screen is saved.
#[test]
fn save_in_a_pane_saves_the_workspace_of_that_pane() {
    let env = Env::new("savehere");
    let dir = env.root.join("config/kraal/workspaces");
    env.workspace("w", "layout {\n    pane name=\"first\"\n}\n");
    env.workspace("v", "layout {\n    pane name=\"second\"\n}\n");
    env.run(&["open", "w"]);
    env.run(&["open", "v"]);
    // A terminal shows w, and the command runs in the pane of v.
    let client = RawClient::attach(&env, "w");
    client.wait_for_message("\"attached\"");
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let second = panes.as_array().unwrap().iter().find(|p| p["name"] == "second").unwrap()["id"].to_string();
    let in_pane = |args: &[&str]| {
        let out = env.cmd(args).env("KRAAL_PANE", &second).env("KRAAL_SESSION", &env.session).output().unwrap();
        assert!(out.status.success(), "kraal {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let saved = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap_or_default();

    std::fs::remove_file(dir.join("v.kdl")).unwrap();
    assert!(in_pane(&["save"]).contains("/v.kdl"));
    // Recreating a missing definition keeps the persistent lock shared with account migration.
    assert_eq!(names_in(&dir), [".v.kdl.lock", "v.kdl", "w.kdl"]);
    assert!(in_pane(&["save", "copy"]).contains("/copy.kdl"));
    assert!(saved("copy.kdl").contains("name=\"second\"") && !saved("copy.kdl").contains("first"));
    assert!(in_pane(&["save", "named", "--workspace", "w"]).contains("/named.kdl"));
    assert!(saved("named.kdl").contains("name=\"first\""), "{}", saved("named.kdl"));
    // Outside a pane: the workspace on screen.
    std::fs::remove_file(dir.join("w.kdl")).unwrap();
    assert!(env.run(&["save"]).contains("/w.kdl"));
}

/// `kraal workspaces --json`, a line each: name, file, and whether it is open.
fn listed_workspaces(env: &Env) -> Vec<String> {
    let list: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    let line = |w: &serde_json::Value| {
        let file = w["definition"].as_str().and_then(|p| Path::new(p).file_name()).unwrap_or_default();
        let open = if w["open"] == true { " (open)" } else { "" };
        format!("{}: {}{open}", w["name"].as_str().unwrap(), file.to_string_lossy())
    };
    list.as_array().unwrap().iter().map(line).collect()
}

/// The names in `dir`, sorted.
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> =
        std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names
}

/// The rows naming the definitions the sidebar lists as Closed, once the screen settles.
fn closed_sidebar_rows(client: &RawClient) -> Vec<String> {
    client.settle();
    client.rows().windows(2).filter(|w| w[1].contains("Closed")).map(|w| w[0].clone()).collect()
}

/// Rename keeps the identity and source, including after restart. Only an explicit save
/// writes the source file.
#[test]
fn a_renamed_workspace_keeps_its_file_unchanged() {
    let env = Env::new("wsrename");
    let dir = env.root.join("config/kraal/workspaces");
    env.workspace("perso", "layout {\n    pane name=\"shell\"\n}\n");
    env.workspace("work", "layout {\n    pane name=\"other\"\n}\n");
    env.run(&["open", "perso"]);
    let identity =
        || serde_json::from_str::<serde_json::Value>(&env.run(&["workspaces", "--json"])).unwrap()[0]["id"].clone();
    let id = identity();
    assert!(id.as_str().is_some());
    let mut client = RawClient::attach(&env, "perso");
    client.wait_for_message("\"attached\"");
    client.wait_for_row("Closed");
    client.send(b"\x02We");
    client.wait_for_row("Rename workspace");
    // Home, then the prefix.
    client.send(b"\x1b[Hinfra - \r");
    client.wait_for_row("Workspace renamed");
    assert_eq!(identity(), id);
    assert_eq!(names_in(&dir), ["perso.kdl", "work.kdl"]);
    let shown = closed_sidebar_rows(&client);
    assert!(shown.len() == 1 && shown[0].contains("work"), "{shown:?}");
    assert_eq!(listed_workspaces(&env), ["infra - perso: perso.kdl (open)", "work: work.kdl"]);
    let text = env.run(&["workspaces"]);
    assert!(!text.lines().any(|l| l.starts_with("perso ")), "{text}");
    // Export proposes a copy; replacing the source is an explicit choice.
    client.send(b"\x02Ws");
    client.wait_for_row("Export layout");
    client.send(format!("\x01\x0b{}\r", dir.join("perso.kdl").display()).as_bytes());
    client.wait_for_row("Replace file?");
    client.send(b"y");
    client.wait_for_row("Layout exported");
    assert_eq!(names_in(&dir), [".perso.kdl.lock", "perso.kdl", "perso.kdl.bak", "work.kdl"]);
    // Restored after a restart, still linked to its file.
    env.run(&["kill"]);
    env.run(&["open", "infra - perso"]);
    assert_eq!(listed_workspaces(&env), ["infra - perso: perso.kdl (open)", "work: work.kdl"]);
    assert_eq!(env.run(&["panes"]).lines().count(), 1);
    assert_eq!(identity(), id);
}

/// Existing list names are refused, but workspace names have no file-name restrictions.
#[test]
fn a_workspace_rename_never_replaces_a_file() {
    let env = Env::new("wsrenameexists");
    let dir = env.root.join("config/kraal/workspaces");
    env.workspace("a", "layout { pane name=\"one\"; }\n");
    env.workspace("b", "layout { pane name=\"two\"; }\n");
    env.run(&["open", "a"]);
    let mut client = RawClient::attach(&env, "a");
    client.wait_for_message("\"attached\"");
    client.send(b"\x02We");
    client.wait_for_row("Rename workspace");
    client.send(b"\x7fb\r");
    client.wait_for_row("This name is already used.");
    client.send(b"\x7fx/y\r");
    client.wait_for_row("Workspace renamed");
    assert_eq!(listed_workspaces(&env), ["x/y: a.kdl (open)", "b: b.kdl"]);
    assert_eq!(names_in(&dir), ["a.kdl", "b.kdl"]);
    assert_eq!(std::fs::read_to_string(dir.join("a.kdl")).unwrap(), "layout { pane name=\"one\"; }\n");
    assert_eq!(std::fs::read_to_string(dir.join("b.kdl")).unwrap(), "layout { pane name=\"two\"; }\n");
}

/// A session where a workspace was renamed without its file, by an older kraal, keeps it linked
/// to that file: listed once, opened by the file's name, saved to it. Renaming again keeps it.
#[test]
fn a_workspace_renamed_without_its_file_is_still_that_files() {
    let env = Env::new("wslegacy");
    let dir = env.root.join("config/kraal/workspaces");
    env.workspace("perso", "layout {\n    pane name=\"shell\"\n}\n");
    env.workspace("work", "layout {\n    pane name=\"other\"\n}\n");
    env.run(&["open", "perso"]);
    env.run(&["kill"]);
    let state = std::fs::read_to_string(env.state_file()).unwrap();
    assert_eq!(state.matches("\"name\": \"perso\"").count(), 1, "{state}");
    std::fs::write(env.state_file(), state.replace("\"name\": \"perso\"", "\"name\": \"infra - perso\"")).unwrap();
    // The old name switches to it rather than starting it again.
    env.run(&["open", "perso"]);
    assert_eq!(listed_workspaces(&env), ["infra - perso: perso.kdl (open)", "work: work.kdl"]);
    assert_eq!(env.run(&["panes"]).lines().count(), 1);
    let mut client = RawClient::attach(&env, "perso");
    client.wait_for_message("\"attached\"");
    let shown = closed_sidebar_rows(&client);
    assert!(shown.len() == 1 && shown[0].contains("work"), "{shown:?}");
    let saved =
        env.run(&["export", dir.join("perso.kdl").to_str().unwrap(), "--workspace", "infra - perso", "--replace"]);
    assert!(saved.trim_end().ends_with("/perso.kdl"), "{saved}");
    assert_eq!(names_in(&dir), [".perso.kdl.lock", "perso.kdl", "perso.kdl.bak", "work.kdl"]);
    client.send(b"\x02We");
    client.wait_for_row("Rename workspace");
    client.send(b"\r");
    client.wait_for_row("Workspace renamed");
    assert_eq!(names_in(&dir), [".perso.kdl.lock", "perso.kdl", "perso.kdl.bak", "work.kdl"]);
}

/// A definition renamed on disk while its workspace is open stays linked to it, and its new name
/// switches to the workspace. Deleted, it is written again by the next save.
#[test]
fn a_definition_renamed_on_disk_stays_linked() {
    let env = Env::new("wsmoved");
    let dir = env.root.join("config/kraal/workspaces");
    env.workspace("perso", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "perso"]);
    std::fs::rename(dir.join("perso.kdl"), dir.join("moved.kdl")).unwrap();
    wait_until(Duration::from_secs(10), "the renamed file is never found", || {
        listed_workspaces(&env) == ["perso: moved.kdl (open)"]
    });
    env.run(&["open", "moved"]);
    assert_eq!(env.run(&["panes"]).lines().count(), 1);
    std::fs::remove_file(dir.join("moved.kdl")).unwrap();
    env.run(&["save", "--workspace", "perso"]);
    assert_eq!(names_in(&dir), [".moved.kdl.lock", "moved.kdl"]);
}

#[test]
fn pane_edit_confirms_a_restart_and_keeps_the_same_pane_and_source_file() {
    for size in [(80, 24), (124, 36)] {
        let env = Env::new("pane-edit-live");
        let starts = env.root.join("starts");
        let quoted = format!("'{}'", starts.display().to_string().replace('\'', "'\\''"));
        let record = format!("printf '%s\\n' \"$$\" >> {quoted}");
        let script = format!("{record}; printf 'original output\\n'; exec sleep 60");
        let layout = format!(
            "layout {{\n    pane name=\"tests\" command=\"sh\" {{\n        args \"-c\" {}\n    }}\n}}\n",
            serde_json::to_string(&script).unwrap()
        );
        env.workspace("edit", &layout);
        env.run(&["open", "edit"]);
        env.wait_for_text("tests", "original output");
        let pane = || {
            let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
            panes.as_array().unwrap().iter().find(|p| p["workspace"] == "edit").unwrap().clone()
        };
        let before = pane();
        let started = std::fs::read_to_string(&starts).unwrap();
        assert_eq!(started.lines().count(), 1);
        let mut client = RawClient::attach_sized(&env, Some("edit"), size).traced();
        client.wait_for_message("\"attached\"");
        client.send(b"\x02Pe");
        client.wait_for_row("Edit pane");
        client.send(b"\x15renamed\r");
        client.wait_for_row("Pane updated");
        assert_eq!(pane()["id"], before["id"]);
        assert_eq!(std::fs::read_to_string(&starts).unwrap(), started);
        assert_eq!(pane()["command"], before["command"]);
        client.send(b"\x02Pe");
        client.wait_for_row("Edit pane");
        client.send(b"\t\t\x15");
        client.paste(format!("{record}; printf 'replacement output\\n'; exec sleep 60").as_bytes());
        client.send(b"\r");
        client.wait_for_row("Apply command?");
        client.send(b"\r");
        client.wait_for_row("Edit pane");
        assert_eq!(std::fs::read_to_string(&starts).unwrap(), started);
        client.send(b"\r");
        client.wait_for_row("Apply command?");
        client.send(b"\t\r");
        env.wait_for_text("renamed", "replacement output");
        assert_eq!(pane()["id"], before["id"]);
        let replaced = std::fs::read_to_string(&starts).unwrap();
        assert_eq!(replaced.lines().count(), 2);
        assert_ne!(replaced.lines().last(), started.lines().next());
        assert!(env.run(&["read", "-p", "renamed"]).contains("original output"));
        assert_eq!(std::fs::read_to_string(env.root.join("config/kraal/workspaces/edit.kdl")).unwrap(), layout);
    }
}

#[test]
fn pane_edit_keeps_errors_and_restores_a_stopped_command_until_enter() {
    let env = Env::new("pane-edit-stopped");
    env.workspace("edit", "layout {\n    pane name=\"tests\" command=\"true\"\n}\n");
    env.run(&["open", "edit"]);
    wait_until(PATIENCE, "command did not end", || {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        panes.as_array().unwrap().iter().any(|p| p["name"] == "tests" && !p["exit_code"].is_null())
    });
    let folder = env.root.join("project folder");
    std::fs::create_dir(&folder).unwrap();
    let marker = folder.join("ran");
    let mut client = RawClient::attach_sized(&env, Some("edit"), (80, 24)).traced();
    client.wait_for_message("\"attached\"");
    client.send(b"\x02Pe");
    client.wait_for_row("Edit pane");
    client.send(b"\t\x15/nonexistent/kraal-pane-edit\r");
    client.wait_for_row("is not available.");
    client.send(b"\x15");
    client.paste(folder.to_str().unwrap().as_bytes());
    client.send(b"\t\x15");
    client.paste(b"pwd > ran; printf 'edited command ran\\n'");
    client.send(b"\r");
    client.wait_for_row("Ready to run");
    assert!(!marker.exists());
    drop(client);
    env.run(&["kill"]);
    env.run(&["open", "edit"]);
    let mut client = RawClient::attach_sized(&env, Some("edit"), (124, 36)).traced();
    client.wait_for_row("enter run");
    assert!(!marker.exists());
    client.send(b"\r");
    wait_until(PATIENCE, "edited command did not run", || {
        env.run(&["read", "-p", "tests"]).lines().any(|line| line.trim() == "edited command ran")
    });
    assert_eq!(std::fs::read_to_string(marker).unwrap().trim(), folder.canonicalize().unwrap().to_str().unwrap());
}

/// Panes stacked from the keys share one place and keep one size; the one the focus reaches
/// opens (a key, a click on its row); a pane comes out again; one key goes through the layouts;
/// stacks and the layout chosen survive a restart, and a saved workspace keeps its stacks.
#[test]
fn stacks_and_layouts_from_the_keys() {
    let env = Env::new("stacks");
    let config = env.root.join("config/kraal/config.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{text}[sidebar]\nvisible = false\n")).unwrap();
    env.workspace(
        "w",
        "layout {\n    pane split_direction=\"vertical\" {\n        pane name=\"alpha\"\n        pane name=\"bravo\"\n        pane name=\"charlie\"\n    }\n}\n",
    );
    env.run(&["open", "w"]);
    let panes = || -> Vec<serde_json::Value> {
        let v: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        v.as_array().unwrap().iter().filter(|p| p["workspace"] == "w").cloned().collect()
    };
    let size = |name: &str| -> (u64, u64) {
        let p = panes().into_iter().find(|p| p["name"] == name).unwrap();
        (p["cols"].as_u64().unwrap(), p["rows"].as_u64().unwrap())
    };
    let focused = || -> String {
        let p = panes().into_iter().find(|p| p["focused"] == true);
        p.and_then(|p| p["name"].as_str().map(String::from)).unwrap_or_default()
    };
    let until = |what: &str, done: &dyn Fn() -> bool| {
        let start = Instant::now();
        while !done() {
            assert!(start.elapsed() < Duration::from_secs(10), "{what}: {}", env.run(&["panes", "--json"]));
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_message("\"attached\"");
    // 120×40: the panes take rows 1 to 38, between the bars, a third of the width each.
    until("three columns", &|| size("alpha") == (37, 36));
    env.run(&["focus", "bravo"]);

    // Bravo goes right, onto charlie: bravo open in two thirds of the width, charlie folded to
    // the row under it, of the same size.
    client.send(b"\x02Ps\x1b[C");
    until("stacked", &|| size("bravo") == (78, 35) && size("charlie") == (78, 35));
    assert_eq!(focused(), "bravo");
    // Down goes to the next pane of the stack, which opens.
    client.send(b"\x1b[1;3B");
    until("charlie focused", &|| focused() == "charlie");
    // Bravo is folded to the top row of the stack now: a click on it opens it.
    client.send(b"\x1b[<0;61;2M\x1b[<0;61;2m");
    until("bravo clicked", &|| focused() == "bravo");
    assert_eq!(size("charlie"), (78, 35), "opening a pane resizes nothing");
    // Out of the stack again: above charlie, each half of the height, frame on frame.
    client.send(b"\x02Pu");
    until("unstacked", &|| size("bravo") == (78, 17) && size("charlie") == (78, 17));
    assert_eq!(focused(), "bravo");

    // The layouts, in turn: even columns, even rows, the main pane left, the others stacked.
    client.send(b"\x02Pl");
    until("columns", &|| size("alpha") == (37, 36) && size("charlie") == (38, 36));
    client.send(b"\x02Pl");
    until("rows", &|| size("alpha").0 == 118 && size("charlie").0 == 118);
    client.send(b"\x02Pl");
    until("main left", &|| size("alpha") == (69, 36) && size("bravo").0 == 46);
    client.send(b"\x02Pl");
    until("main left, stacked", &|| size("bravo") == (46, 35) && size("charlie") == (46, 35));
    assert_eq!(focused(), "bravo");

    // A restart brings the stack and the layout back.
    drop(client);
    env.run(&["kill"]);
    let state = std::fs::read_to_string(env.state_file()).unwrap();
    assert!(state.contains("\"stacked\": true") && state.contains("\"preset\": \"main-left-stacked\""), "{state}");
    env.run(&["open", "w"]);
    until("restored", &|| panes().len() == 3);
    until("stacked again", &|| size("bravo") == (46, 35) && size("charlie") == (46, 35) && size("alpha") == (69, 36));
    // The layout is still the tab's: a pane that comes joins the stack.
    env.run(&["run", "-t", "alpha", "--name", "delta", "--", "sh"]);
    until("delta stacked", &|| panes().len() == 4 && size("delta") == (46, 34) && size("bravo") == (46, 34));

    // Saved, the stack is `stacked=true`, with the pane open `expanded=true`, and opens as one.
    env.run(&["save", "copy", "--workspace", "w"]);
    let saved = std::fs::read_to_string(env.root.join("config/kraal/workspaces/copy.kdl")).unwrap();
    assert!(saved.contains("pane stacked=true") && saved.contains("expanded=true"), "{saved}");
    env.run(&["open", "copy"]);
    let copies = || -> Vec<(u64, u64)> {
        let v: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let mut sizes: Vec<(u64, u64)> = v
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["workspace"] == "copy")
            .map(|p| (p["cols"].as_u64().unwrap(), p["rows"].as_u64().unwrap()))
            .collect();
        sizes.sort();
        sizes
    };
    until("the copy opens stacked", &|| copies() == [(46, 34), (46, 34), (46, 34), (69, 36)]);
}

#[test]
fn a_stack_opens_on_its_expanded_pane_without_a_focus_given() {
    let env = Env::new("expanded");
    let stack = "pane stacked=true {\n        pane name=\"a\"\n        pane name=\"b\" expanded=true\n    }";
    env.workspace("w", &format!("layout {{\n    {stack}\n}}\n"));
    env.run(&["open", "w"]);
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let focused: Vec<&str> =
        panes.as_array().unwrap().iter().filter(|p| p["focused"] == true).filter_map(|p| p["name"].as_str()).collect();
    assert_eq!(focused, ["b"]);
}

/// A kraal the tests run stops rather than use what is the developer's, however a test let it
/// through: as the home, as a directory of kraal's or of an agent, or in `PATH`.
#[test]
fn a_kraal_run_by_a_test_stays_out_of_what_is_the_developers() {
    let env = Env::new("keepout");
    let theirs = env.root.join("theirs");
    std::fs::create_dir_all(theirs.join("run")).unwrap();
    let kraal = || {
        let mut c = env.cmd(&["ls"]);
        c.env(common::isolation::KEEP_OUT, &theirs);
        c
    };
    let path = format!("{}:{}", theirs.join("bin").display(), std::env::var("PATH").unwrap());
    for (name, value) in [
        ("HOME", theirs.clone().into_os_string()),
        ("XDG_CONFIG_HOME", theirs.join(".config").into()),
        ("XDG_STATE_HOME", theirs.join(".local/state").into()),
        ("XDG_DATA_HOME", theirs.join(".local/share").into()),
        ("XDG_RUNTIME_DIR", theirs.join("run").into()),
        ("CLAUDE_CONFIG_DIR", theirs.join(".claude-work").into()),
        ("CODEX_HOME", theirs.join(".codex").into()),
        ("PATH", path.into()),
    ] {
        let out = kraal().env(name, &value).output().unwrap();
        let said = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{name} let through");
        let named = said.starts_with("kraal: run by a test, ") && said.contains(&*theirs.to_string_lossy());
        assert!(named, "{name}: {said}");
    }
    let out = kraal().output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!theirs.join("run/kraal").exists(), "nothing was made there");
}

#[test]
fn commands_run_in_a_pane_stay_in_its_session() {
    let a = Env::new("sessa");
    a.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    a.run(&["open", "w"]);
    let base = || a.isolated(Path::new(env!("CARGO_BIN_EXE_kraal")));
    // A second session in the same directories, whose pane has the same id.
    let b = |args: &[&str]| base().args(["-s", "sessb"]).args(args).output().unwrap();
    assert!(b(&["open", "w"]).status.success());
    let id = |out: &str| -> String {
        let v: serde_json::Value = serde_json::from_str(out).unwrap();
        v[0]["id"].to_string()
    };
    let pane_a = id(&a.run(&["panes", "--json"]));
    let pane_b = id(&String::from_utf8(b(&["panes", "--json"]).stdout).unwrap());
    assert_eq!(pane_a, pane_b);
    // As if typed in session a's pane: no `-s`, the pane's variables.
    let in_pane = |args: &[&str]| {
        base()
            .args(args)
            .env("KRAAL_SESSION", "sessa")
            .env("KRAAL_SOCKET", a.root.join("run/kraal/sessa.sock"))
            .env("KRAAL_PANE", &pane_a)
            .output()
            .unwrap()
    };
    // Another session's pane is never the one the command runs in.
    let out = in_pane(&["-s", "sessb", "close"]);
    assert!(!out.status.success(), "closed a pane of sessb");
    assert!(String::from_utf8_lossy(&b(&["panes"]).stdout).contains("one"));
    // `kill` without `-s` stops the session it runs in, even with another session named in
    // `$KRAAL_SESSION_NAME` (exported by a shell's startup files, inherited by every pane).
    let out = base()
        .arg("kill")
        .env("KRAAL_SESSION", "sessa")
        .env("KRAAL_SOCKET", a.root.join("run/kraal/sessa.sock"))
        .env("KRAAL_PANE", &pane_a)
        .env("KRAAL_SESSION_NAME", "sessb")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!a.try_run(&["panes"]).status.success(), "sessa still runs");
    assert!(b(&["panes"]).status.success(), "sessb was stopped");
    // Outside kraal, `$KRAAL_SESSION_NAME` still names the session.
    let named = base().arg("panes").env("KRAAL_SESSION_NAME", "sessb").output().unwrap();
    assert!(named.status.success() && String::from_utf8_lossy(&named.stdout).contains("one"));
    let _ = b(&["kill"]);
}

#[test]
fn kill_says_when_the_layout_was_not_saved() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new("unsaved");
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    env.run(&["open", "w"]);
    let sessions = env.root.join("state/kraal/sessions-v2");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o500)).unwrap();
    let out = env.try_run(&["kill"]);
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o700)).unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("could not be saved"), "{err}");
    assert!(env.server_pid().is_none(), "the server stopped anyway");
}

/// A session has one server: another one started for it while it runs refuses to, even when
/// the first one's socket is gone (a cleaner of /tmp may remove it), so that two servers never
/// write the same session.
#[test]
fn a_session_never_has_two_servers() {
    let env = Env::new("oneserver");
    let _evidence = Evidence(&env);
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    env.run(&["open", "w"]);
    let first = env.server_pid().expect("the server runs");
    let socket = env.root.join("run/kraal/oneserver.sock");
    let aside = env.root.join("run/kraal/aside.sock");
    std::fs::rename(&socket, &aside).unwrap();
    let second = env.cmd(&["server"]).stderr(std::process::Stdio::piped()).spawn().unwrap();
    let out = finish(second, Duration::from_secs(10));
    std::fs::rename(&aside, &socket).unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("holds session \"oneserver\""), "{err}");
    assert_eq!(env.server_pid(), Some(first));
    env.run(&["panes"]);
}

/// `kraal forget` deletes nothing of a session whose server runs, even one whose socket is gone:
/// a server holds its session, and `forget` must hold it too.
#[test]
fn forget_leaves_a_running_session_alone() {
    let env = Env::new("forgetheld");
    let _evidence = Evidence(&env);
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    env.run(&["open", "w"]);
    wait_until(Duration::from_secs(10), "the layout was never saved", || env.state_file().exists());
    let socket = env.root.join("run/kraal/forgetheld.sock");
    let aside = env.root.join("run/kraal/aside.sock");
    std::fs::rename(&socket, &aside).unwrap();
    let out = env.try_run(&["forget"]);
    std::fs::rename(&aside, &socket).unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("is running"), "{err}");
    assert!(env.state_file().exists(), "the layout was deleted");
}

/// The lock file of `env`'s session in one of its places: where the session saves (`state`), or
/// one of its two runtime directories (`run`, and `run2` of [`environment`]).
fn lock_file(env: &Env, place: &str) -> PathBuf {
    env.root.join(place).join(format!("kraal/{}.lock", env.session))
}

/// How long a start that finds its session held looks for that one's server before it gives up:
/// the time a start may take, and not much more.
fn gave_up_in_time(took: Duration) -> bool {
    took >= Duration::from_secs(5) && took < Duration::from_secs(20)
}

/// The same user, state and configuration as `env`, with another runtime directory when
/// `other`: a cron job without `$XDG_RUNTIME_DIR`, say, beside a desktop terminal that has it.
fn environment(env: &Env, other: bool, args: &[&str]) -> Command {
    let mut c = env.cmd(args);
    if other {
        let run = env.root.join("run2");
        std::fs::create_dir_all(&run).unwrap();
        c.env("XDG_RUNTIME_DIR", run);
    }
    c
}

/// Stops the server that `env`'s session has in its other runtime directory when the test
/// ends, however it ends: `Env` stops the one of its own.
struct StopsOther<'a>(&'a Env);

impl Drop for StopsOther<'_> {
    fn drop(&mut self) {
        use std::process::Stdio;
        let _ = environment(self.0, true, &["kill"]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
}

/// What `kraal args` prints in one of the two environments of `env`, which must succeed, and
/// within the time any command may take ([`bounded`]).
fn in_environment(env: &Env, other: bool, args: &[&str]) -> String {
    let out = bounded(env, other, args);
    let (said, err) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let failed = format!("kraal {args:?} (other: {other}): {}\n{said}{err}", out.status);
    assert!(out.status.success(), "{failed}\n{}", evidence(env));
    said.into_owned()
}

/// What `kraal args` did in one of the two environments of `env`, within the time any command
/// may take: one that does not end is stopped, and fails the test with what is known of the
/// session ([`evidence`]). What it says goes to files, not to pipes: a pipe is read to its end,
/// which comes once every process that has it is gone, and on macOS a program another thread
/// starts at that moment is given it.
fn bounded(env: &Env, other: bool, args: &[&str]) -> Output {
    let mut command = environment(env, other, args);
    within(env, &mut command, &format!("kraal {args:?}"))
}

/// What `command`, a program of `env`'s test, did within the time any command may take, as
/// [`bounded`] has it.
fn within(env: &Env, command: &mut Command, what: &str) -> Output {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let said = env.root.join(format!("said-{}", NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    let file = |end: &str| std::fs::File::create(said.with_extension(end)).unwrap();
    let mut child = command.stdout(file("out")).stderr(file("err")).spawn().unwrap();
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if start.elapsed() > PATIENCE {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let read = |end: &str| std::fs::read(said.with_extension(end)).unwrap_or_default();
    let (stdout, stderr) = (read("out"), read("err"));
    let Some(status) = status else {
        let (out, err) = (String::from_utf8_lossy(&stdout), String::from_utf8_lossy(&stderr));
        panic!("{what} still ran after {PATIENCE:?}. It said:\n{out}{err}\n{}", evidence(env));
    };
    Output { status, stdout, stderr }
}

/// Says what is known of `env`'s session ([`evidence`]) when the test that has it fails,
/// whatever fails in it: an assertion, a command that did not end, a thread of its own.
struct Evidence<'a>(&'a Env);

impl Drop for Evidence<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("{}", evidence(self.0));
        }
    }
}

/// What is known of `env`'s session when a command did not do what a test expects: what its
/// servers logged, and what is where it saves and in its two runtime directories (sockets, their
/// second names, lock files).
fn evidence(env: &Env) -> String {
    let mut all = String::new();
    for dir in ["state/kraal", "run/kraal", "run2/kraal"] {
        let mut names: Vec<String> = std::fs::read_dir(env.root.join(dir))
            .into_iter()
            .flatten()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        all.push_str(&format!("--- {dir}: {names:?}\n"));
        for log in names.iter().filter(|name| name.starts_with("server-") && name.ends_with(".log")) {
            let logged = std::fs::read_to_string(env.root.join(dir).join(log)).unwrap_or_default();
            all.push_str(&format!("--- {log}:\n{logged}\n"));
        }
    }
    all
}

/// Where the session of `env` has a socket, of its two runtime directories.
fn sockets(env: &Env) -> Vec<&'static str> {
    let socket = format!("kraal/{}.sock", env.session);
    ["run", "run2"].into_iter().filter(|run| env.root.join(run).join(&socket).exists()).collect()
}

/// Hold a lock of `env`'s session, as another kraal would: the one where the session saves
/// (`state`) or the one of its runtime directory (`run`), with what that kraal says of itself
/// there. Held until what is returned is dropped.
fn hold(env: &Env, place: &str, said: &str) -> std::fs::File {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::DirBuilderExt;
    let dir = env.root.join(place).join("kraal");
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).unwrap();
    let path = dir.join(format!("{}.lock", env.session));
    let mut lock = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(path).unwrap();
    // Once nobody has it: a lock this test let go a moment ago is still that of a program
    // another test is starting, which has this process's files until it runs.
    // SAFETY: plain syscall on a file this test holds open.
    wait_until(PATIENCE, "the session is held", || unsafe {
        libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0
    });
    lock.write_all(said.as_bytes()).unwrap();
    lock
}

/// A session has one server for what it saves, whichever runtime directory a kraal of the user
/// resolves: started from one environment, it is not started, served or forgotten from the
/// other; in both orders. A start from the other looks for a server of its own for the time a
/// start may take, then says where the session runs and how to reach it. Stopped where it runs,
/// the session starts from the other.
#[test]
fn a_session_running_under_another_runtime_directory_is_refused_and_said() {
    for first in [false, true] {
        let env = Env::new(if first { "onestate-b" } else { "onestate-a" });
        let _other = StopsOther(&env);
        let _evidence = Evidence(&env);
        env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
        let (here, there) = (first, !first);
        let run = |other: bool| if other { "run2" } else { "run" };
        in_environment(&env, here, &["open", "w"]);
        wait_until(PATIENCE, "the layout was never saved", || env.state_file().exists());
        let where_it_is = format!("{}/kraal\", not in", run(here));
        let reach_it = format!("{}/kraal\" where this kraal looks: reach it with XDG_RUNTIME_DIR=", run(there));
        let start = Instant::now();
        let refused = bounded(&env, there, &["open", "w"]);
        let said = String::from_utf8_lossy(&refused.stderr);
        assert!(gave_up_in_time(start.elapsed()), "refused after {:?}", start.elapsed());
        assert!(!refused.status.success() && said.contains("session onestate-"), "{said}");
        assert!(said.contains("is not started here: kraal ") && said.contains(&where_it_is), "{said}");
        assert!(said.contains(&reach_it), "{said}");
        let forget = bounded(&env, there, &["forget"]);
        let said = String::from_utf8_lossy(&forget.stderr);
        assert!(!forget.status.success() && said.contains("is running (kraal ") && said.contains(&reach_it), "{said}");
        let second = bounded(&env, there, &["server"]);
        assert_eq!(second.status.code(), Some(75), "{}", String::from_utf8_lossy(&second.stderr));
        // Nothing of it was touched: it answers where it runs, with what it saved.
        assert_eq!((sockets(&env), env.state_file().exists()), (vec![run(here)], true));
        assert!(in_environment(&env, here, &["panes"]).contains("one"));
        assert!(in_environment(&env, here, &["kill"]).contains("stopped session"));
        assert_eq!((sockets(&env), lock_file(&env, "state").exists()), (Vec::new(), false));
        in_environment(&env, there, &["open", "w"]);
        assert!(in_environment(&env, there, &["panes"]).contains("one"));
        in_environment(&env, there, &["kill"]);
        in_environment(&env, here, &["forget"]);
        let left = (sockets(&env), lock_file(&env, "state").exists(), env.state_file().exists());
        assert_eq!(left, (Vec::new(), false, false));
    }
}

/// Started at the same instant from two environments with different runtime directories, a
/// session ends with one socket, and one of the two starts is refused: a few rounds of what the
/// lock decides, which the unit tests of the lock show step by step.
#[test]
fn a_session_started_at_once_from_two_runtime_directories_has_one_server() {
    let env = Env::new("atonce");
    let _other = StopsOther(&env);
    let _evidence = Evidence(&env);
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    for round in 0..3 {
        let together = std::sync::Barrier::new(2);
        let started: Vec<Output> = std::thread::scope(|scope| {
            let starts: Vec<_> = [false, true]
                .into_iter()
                .map(|other| {
                    let (env, together) = (&env, &together);
                    scope.spawn(move || {
                        together.wait();
                        bounded(env, other, &["open", "w"])
                    })
                })
                .collect();
            starts.into_iter().map(|start| start.join().unwrap()).collect()
        });
        let at = sockets(&env);
        assert_eq!(at.len(), 1, "round {round}: {at:?}");
        let winner = at[0] == "run2";
        assert!(started[usize::from(winner)].status.success(), "round {round}");
        let loser = &started[usize::from(!winner)];
        let said = String::from_utf8_lossy(&loser.stderr);
        let told = said.contains("is not started here") || said.contains("no server of it listens");
        assert!(!loser.status.success() && told, "round {round}: {said}");
        assert!(in_environment(&env, winner, &["panes"]).contains("one"), "round {round}");
        in_environment(&env, winner, &["kill"]);
        assert!(!lock_file(&env, "state").exists(), "round {round}");
        in_environment(&env, !winner, &["forget"]);
    }
}

/// A server killed outright leaves its lock files, what it said of itself and its socket:
/// nothing of that keeps the session from starting again, from either environment.
#[test]
fn a_killed_server_leaves_nothing_that_blocks_the_next_start() {
    let env = Env::new("killedheld");
    let _other = StopsOther(&env);
    let _evidence = Evidence(&env);
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    for next in [false, true] {
        in_environment(&env, false, &["open", "w"]);
        let pid = env.server_pid().expect("the server runs");
        let socket = env.root.join("run/kraal/killedheld.sock");
        // SAFETY: plain syscall to the server this test started.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
        // Gone once nothing listens: a process being killed still does for a moment.
        wait_until(PATIENCE, "the killed server still listens", || {
            std::os::unix::net::UnixStream::connect(&socket).is_err()
        });
        assert!(lock_file(&env, "state").exists() && socket.exists(), "what a killed server leaves");
        let started = bounded(&env, next, &["open", "w"]);
        assert!(started.status.success(), "{}", String::from_utf8_lossy(&started.stderr));
        assert!(in_environment(&env, next, &["panes"]).contains("one"));
        in_environment(&env, next, &["kill"]);
        assert!(!lock_file(&env, "state").exists(), "the lock file a killed server left");
    }
}

/// Where the state directory cannot hold a session, none is started there and none forgotten:
/// the reason is said, with what to do, and what the session saved stays. Here the lock file
/// cannot be made; a file system that locks no file is what the unit tests put in its place.
#[test]
fn a_state_directory_that_cannot_hold_a_session_starts_none_and_forgets_none() {
    let env = Env::new("nostate");
    let _evidence = Evidence(&env);
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    env.run(&["open", "w"]);
    wait_until(PATIENCE, "the layout was never saved", || env.state_file().exists());
    env.run(&["kill"]);
    // No lock can be had there: a directory is where its file goes.
    let lock = env.root.join("state/kraal/nostate.lock");
    std::fs::create_dir(&lock).unwrap();
    for command in [&["open", "w"][..], &["forget"]] {
        let out = bounded(&env, false, command);
        let said = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && said.contains("no session can be held in"), "{command:?}: {said}");
        assert!(said.contains("XDG_STATE_HOME"), "{command:?}: {said}");
    }
    assert_eq!((env.server_pid(), sockets(&env)), (None, Vec::new()));
    assert!(env.state_file().exists(), "what it saved was deleted");
    std::fs::remove_dir(&lock).unwrap();
    env.run(&["open", "w"]);
    assert!(env.run(&["panes"]).contains("one"));
}

/// `$XDG_RUNTIME_DIR` set to a directory kraal cannot keep its sockets in is said with the
/// reason: by `kraal config check`, to who starts a server so, in that server's log, and to who
/// attaches to it. Not set, the sockets go where they go without a word.
#[test]
fn a_runtime_directory_kraal_cannot_use_is_said() {
    let env = Env::new("xdgno");
    let _evidence = Evidence(&env);
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    std::fs::create_dir_all(env.root.join("state/kraal")).unwrap();
    std::fs::write(env.root.join("state/kraal/welcomed"), "").unwrap();
    // Too long for a socket path. The sockets then go under the temporary directory: this
    // test's own.
    let (long, tmp) = (env.root.join("x".repeat(60)), env.root.join("t"));
    for dir in [&long, &tmp] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let tmp = tmp.canonicalize().unwrap();
    // SAFETY: getuid cannot fail.
    let sockets = tmp.join(format!("kraal-{}/kraal", unsafe { libc::getuid() }));
    let warning = format!(
        "XDG_RUNTIME_DIR ({}) is not used: it is {} bytes long, and a socket path leaves it 59. Sockets are in {}",
        long.display(),
        long.as_os_str().len(),
        sockets.display()
    );
    let kraal = |args: &[&str]| env.cmd(args).env("XDG_RUNTIME_DIR", &long).env("TMPDIR", &tmp).output().unwrap();
    let checked = kraal(&["config", "check"]);
    assert_eq!(String::from_utf8_lossy(&checked.stderr), format!("warning: {warning}\n"));
    let unset = env.cmd(&["config", "check"]).env_remove("XDG_RUNTIME_DIR").env("TMPDIR", &tmp).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&unset.stderr), "");
    let opened = kraal(&["open", "w"]);
    assert!(opened.status.success(), "{}", String::from_utf8_lossy(&opened.stderr));
    assert_eq!(String::from_utf8_lossy(&opened.stderr), format!("kraal: warning: {warning}\n"));
    let log = std::fs::read_to_string(env.root.join("state/kraal/server-xdgno.log")).unwrap();
    assert!(log.contains(&format!("kraal: warning: {warning}\n")), "{log}");
    // Said once, by who started the server.
    assert_eq!(String::from_utf8_lossy(&kraal(&["panes"]).stderr), "");
    let client = RawClient::attach_at(&sockets.join("xdgno.sock"), Some("w"), (120, 40), None);
    client.wait_for_screen("XDG_RUNTIME_DIR (");
    drop(client);
    assert!(kraal(&["kill"]).status.success());
}

/// A session is not forgotten while it is restarted: in the moment between its server and the
/// next, when nobody holds it, `kraal forget` is refused, and the session comes back with what
/// it saved. One restart at a time.
#[test]
fn a_session_is_not_forgotten_while_it_restarts() {
    use std::os::fd::AsRawFd;
    let env = Env::new("restartgap");
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    env.run(&["open", "w"]);
    env.run(&["run", "--name", "kept", "--", "sh"]);
    wait_until(PATIENCE, "the layout was never saved", || env.state_file().exists());
    // As a restart keeps it: `forget` deletes nothing, another restart stops nothing.
    let kept = std::fs::File::create(env.root.join("state/kraal/restartgap.restart")).unwrap();
    // SAFETY: plain syscall on a file this test holds open.
    assert_eq!(unsafe { libc::flock(kept.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    let pid = env.server_pid();
    let restart = bounded(&env, false, &["restart", "--yes"]);
    let said = String::from_utf8_lossy(&restart.stderr);
    assert!(!restart.status.success() && said.contains("another kraal is restarting session restartgap"), "{said}");
    assert_eq!(env.server_pid(), pid, "the session was stopped");
    env.run(&["kill"]);
    let forget = bounded(&env, false, &["forget"]);
    let said = String::from_utf8_lossy(&forget.stderr);
    assert!(!forget.status.success() && said.contains("another kraal is restarting session restartgap"), "{said}");
    assert!(env.state_file().exists(), "forgotten all the same");
    drop(kept);
    env.run(&["open", "w"]);
    // Restarted again and again while `forget` keeps coming: it comes back whole each time.
    /// Says so when it goes, however this test ends: `forget` then stops coming.
    struct Done<'a>(&'a std::sync::atomic::AtomicBool);
    impl Drop for Done<'_> {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let done = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            while !done.load(std::sync::atomic::Ordering::SeqCst) {
                let forget = bounded(&env, false, &["forget"]);
                assert!(!forget.status.success(), "a running session was forgotten");
            }
        });
        let _done = Done(&done);
        for round in 0..8 {
            let restarted = bounded(&env, false, &["restart", "--yes"]);
            let panes = String::from_utf8_lossy(&bounded(&env, false, &["panes"]).stdout).into_owned();
            let whole = restarted.status.success() && panes.contains("one") && panes.contains("kept");
            assert!(whole, "round {round}: {}\n{panes}", String::from_utf8_lossy(&restarted.stderr));
        }
    });
    assert!(lock_file(&env, "state").exists(), "its server holds it");
    assert!(!env.root.join("state/kraal/restartgap.restart").exists(), "what kept it for its restart was left");
}

/// A restart keeps the session from `kraal forget` until the server it started has it, whatever
/// becomes of the kraal that restarts: killed, or done waiting, while that server is still on
/// its way. That server alone has what kept it: once it has the session, nothing else does.
#[test]
fn a_restart_keeps_the_session_until_its_server_has_it() {
    use std::process::Stdio;
    let env = Env::new("restartkept");
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    env.run(&["open", "w"]);
    env.run(&["run", "--name", "kept", "--", "sh"]);
    wait_until(PATIENCE, "the layout was never saved", || env.state_file().exists());
    // While this file is there, a server started for the session waits before it takes it.
    let hold = env.root.join("hold");
    /// Lets that server go and stops it when the test ends, however it ends: one still held
    /// back would start once the test's directory, and this file with it, is gone.
    struct LetGo<'a>(&'a Env, PathBuf);
    impl Drop for LetGo<'_> {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.1);
            let start = Instant::now();
            while self.0.server_pid().is_some() && start.elapsed() < Duration::from_secs(10) {
                let _ = self.0.try_run(&["kill"]);
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    let _let_go = LetGo(&env, hold.clone());
    let reservation = env.root.join("state/kraal/restartkept.restart");
    for killed in [true, false] {
        std::fs::write(&hold, "").unwrap();
        let old = env.server_pid().expect("the server runs");
        let mut restart = env
            .cmd(&["restart", "--yes"])
            .env("KRAAL_TEST_HOLD_SERVER", &hold)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // SAFETY: signal 0 only asks whether the server this test started is still there.
        wait_until(PATIENCE, "the next server was never started", || unsafe {
            libc::kill(old, 0) != 0 && env.server_pid().is_some()
        });
        if killed {
            restart.kill().unwrap();
            restart.wait().unwrap();
        } else {
            let out = finish(restart, PATIENCE);
            let said = String::from_utf8_lossy(&out.stderr);
            assert!(!out.status.success() && said.contains("did not start in time"), "{said}");
        }
        // The server on its way has no session yet, and still keeps it.
        let forget = bounded(&env, false, &["forget"]);
        let said = String::from_utf8_lossy(&forget.stderr);
        assert!(!forget.status.success() && said.contains("another kraal is restarting session"), "{killed}: {said}");
        assert!(env.state_file().exists() && reservation.exists(), "killed: {killed}");
        // Let go, it takes the session and restores it.
        std::fs::remove_file(&hold).unwrap();
        wait_until(PATIENCE, "the session never came back", || {
            String::from_utf8_lossy(&bounded(&env, false, &["panes"]).stdout).contains("kept")
        });
        assert!(!reservation.exists(), "what kept it for its restart was left");
        // What kept it is nobody's any more, a pane's least of all: `forget` gets as far as the
        // session, which its server has.
        let forget = bounded(&env, false, &["forget"]);
        let said = String::from_utf8_lossy(&forget.stderr);
        assert!(!forget.status.success() && said.contains("is running"), "killed: {killed}: {said}");
    }
}

/// A start that finds the session held by a kraal that does not listen yet (the one that won
/// when two starts came at the same moment, a kraal 0.3.0 that holds only its runtime
/// directory's lock) waits for it, and has a server once that one listens or is gone: it never
/// says that its own server exited. What is written of the holder decides nothing of that: a
/// kraal that has just taken the session and not yet said so, under what one killed in another
/// runtime directory left written, is waited for like any other. While the session stays held
/// and nothing listens, the start gives up after the time a start may take, and what is written
/// then tells why.
#[test]
fn a_start_waits_for_the_kraal_that_has_the_session() {
    let env = Env::new("heldstart");
    let _evidence = Evidence(&env);
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    let elsewhere = "{\"pid\":4242,\"host\":\"another-machine.invalid\",\"runtime\":\"/run/user/1/kraal\"}\n";
    let held_for = Duration::from_secs(1);
    for (place, said) in [("run", ""), ("state", ""), ("state", elsewhere)] {
        let lock = hold(&env, place, said);
        let start = Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(held_for);
                drop(lock);
            });
            let out = bounded(&env, false, &["open", "w"]);
            assert!(out.status.success(), "{place} {said:?}: {}", String::from_utf8_lossy(&out.stderr));
        });
        assert!(start.elapsed() >= held_for, "{place}: started while the session was held");
        assert!(env.run(&["panes"]).contains("one"));
        env.run(&["kill"]);
    }
    // Held for good, with no server: said, once a start has had its time.
    let by_another = "session heldstart is held (by another kraal) and no server of it listens";
    let on_another = "session heldstart is not started here: it runs on \"another-machine.invalid\" (kraal 4242)";
    // What is written may name anything where the holder's sockets are: a FIFO, which would
    // never open, keeps nobody waiting.
    let fifo = env.root.join("fifo");
    let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: a path that ends in a zero byte.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let at_a_fifo =
        format!("{{\"pid\":4242,\"host\":\"another-machine.invalid\",\"runtime\":{:?}}}\n", fifo.display().to_string());
    for (said, told) in [("", by_another), (elsewhere, on_another), (at_a_fifo.as_str(), on_another)] {
        let lock = hold(&env, "state", said);
        let start = Instant::now();
        let out = bounded(&env, false, &["open", "w"]);
        let answer = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && answer.contains(told) && !answer.contains("exited"), "{answer}");
        assert!(gave_up_in_time(start.elapsed()), "gave up after {:?}", start.elapsed());
        assert_eq!((env.server_pid(), sockets(&env)), (None, Vec::new()));
        drop(lock);
    }
    env.run(&["open", "w"]);
    assert!(env.run(&["panes"]).contains("one"));
}

/// A state directory and a runtime directory that are one directory, as given or through a
/// link, hold a session as two do: it starts at once, is not forgotten while it runs, and is
/// forgotten once stopped.
#[test]
fn a_state_and_a_runtime_directory_that_are_one_hold_a_session() {
    let env = Env::new("onedir");
    let _evidence = Evidence(&env);
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    let (both, alias) = (env.root.join("both"), env.root.join("alias"));
    std::fs::create_dir_all(&both).unwrap();
    std::os::unix::fs::symlink(&both, &alias).unwrap();
    for runtime in [&both, &alias] {
        let kraal = |args: &[&str]| {
            use std::process::Stdio;
            let mut c = env.cmd(args);
            c.env("XDG_STATE_HOME", &both)
                .env("XDG_RUNTIME_DIR", runtime)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            finish(c.spawn().unwrap(), PATIENCE)
        };
        let start = Instant::now();
        let opened = kraal(&["open", "w"]);
        assert!(opened.status.success(), "{}", String::from_utf8_lossy(&opened.stderr));
        assert!(start.elapsed() < Duration::from_secs(5), "started after {:?}", start.elapsed());
        assert!(String::from_utf8_lossy(&kraal(&["panes"]).stdout).contains("one"));
        let forget = kraal(&["forget"]);
        assert!(String::from_utf8_lossy(&forget.stderr).contains("is running"), "{forget:?}");
        assert!(kraal(&["kill"]).status.success());
        let forget = kraal(&["forget"]);
        assert!(forget.status.success(), "{}", String::from_utf8_lossy(&forget.stderr));
    }
}

/// Started many times at the same moment, a session has one server, and every start ends with
/// it: none fails because another one's server was first.
#[test]
fn a_session_started_many_times_at_once_starts_once_and_for_all() {
    let env = Env::new("startrace");
    let _evidence = Evidence(&env);
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    for round in 0..10 {
        let together = std::sync::Barrier::new(8);
        let starts: Vec<Output> = std::thread::scope(|scope| {
            let starts: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        together.wait();
                        bounded(&env, false, &["open", "w"])
                    })
                })
                .collect();
            starts.into_iter().map(|start| start.join().unwrap()).collect()
        });
        let all: Vec<String> = starts
            .iter()
            .map(|out| format!("{}: {}", out.status, String::from_utf8_lossy(&out.stderr).trim()))
            .collect();
        assert!(starts.iter().all(|out| out.status.success()), "round {round}: {all:#?}\n{}", evidence(&env));
        assert!(in_environment(&env, false, &["panes"]).contains("one"), "round {round}\n{}", evidence(&env));
        // One server: stopped, none is left.
        in_environment(&env, false, &["kill"]);
        let start = Instant::now();
        while env.server_pid().is_some() {
            assert!(start.elapsed() < PATIENCE, "round {round}: a second server was running\n{}", evidence(&env));
            std::thread::sleep(Duration::from_millis(50));
        }
        in_environment(&env, false, &["forget"]);
    }
}

/// A server keeps open nothing of what the kraal that started it was started with. A pipe that
/// kraal was given (by a script that reads what its commands say, a build tool, a test) ends
/// for whoever reads it when that kraal is gone, not when the session is.
#[test]
fn a_server_keeps_nothing_its_starter_was_given() {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    let env = Env::new("nofds");
    let _evidence = Evidence(&env);
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    let (mut read, given) = std::io::pipe().unwrap();
    let fd = given.as_raw_fd();
    let mut open = env.cmd(&["open", "w"]);
    // SAFETY: fcntl alone, which may run between fork and exec. The kraal started has the
    // writing end of the pipe as a descriptor nobody marked to be closed.
    unsafe {
        open.pre_exec(move || match libc::fcntl(fd, libc::F_SETFD, 0) {
            0 => Ok(()),
            _ => Err(std::io::Error::last_os_error()),
        })
    };
    let out = within(&env, &mut open, "kraal open w");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    drop(given);
    assert!(env.server_pid().is_some(), "no server runs");
    let (ended, end) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut rest = Vec::new();
        let _ = ended.send(read.read_to_end(&mut rest));
    });
    let ended = end.recv_timeout(Duration::from_secs(10));
    assert!(ended.is_ok(), "the server holds a pipe of whoever started the kraal that started it");
}

/// The lock file where a session saves goes with its holder, a server or `kraal forget`: a
/// session name that was used leaves nothing there, and the file a killed server left is the
/// next one's to remove. The one of the runtime directory stays, for kraal 0.3.0: it takes that
/// file without looking at it again, and would hold one that is gone.
#[test]
fn a_session_leaves_no_lock_file_where_it_saves() {
    let env = Env::new("nolock");
    let _evidence = Evidence(&env);
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    env.run(&["open", "w"]);
    assert!(lock_file(&env, "state").exists() && lock_file(&env, "run").exists(), "a server holds its session");
    // Open here until the end: a file made once it is gone could have its number.
    let runtime_lock = std::fs::File::open(lock_file(&env, "run")).unwrap();
    env.run(&["kill"]);
    assert!(!lock_file(&env, "state").exists(), "left by a server that stopped");
    env.run(&["forget"]);
    assert!(!lock_file(&env, "state").exists(), "left by kraal forget");
    // A server killed outright cannot remove its file: the next server takes it, and removes it.
    env.run(&["open", "w"]);
    let pid = env.server_pid().expect("the server runs");
    // SAFETY: plain syscall to the server this test started.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    // Gone once nothing listens: a process being killed still does for a moment.
    let socket = env.root.join("run/kraal/nolock.sock");
    wait_until(Duration::from_secs(10), "the killed server still listens", || {
        std::os::unix::net::UnixStream::connect(&socket).is_err()
    });
    assert!(lock_file(&env, "state").exists());
    env.run(&["open", "w"]);
    env.run(&["kill"]);
    assert!(!lock_file(&env, "state").exists(), "left by the server after a killed one");
    let number = |file: std::fs::Metadata| std::os::unix::fs::MetadataExt::ino(&file);
    let (then, now) = (runtime_lock.metadata().unwrap(), std::fs::metadata(lock_file(&env, "run")).unwrap());
    assert_eq!(number(now), number(then), "the runtime directory's lock file was replaced");
}

/// Servers started and stopped and the session forgotten, all at once and from many processes:
/// the session is never held by two of them, every command works or is refused for what it says,
/// and nothing is left of the lock once all is stopped.
#[test]
fn a_session_started_stopped_and_forgotten_at_once_is_held_by_one_at_a_time() {
    use std::process::Stdio;
    let env = Env::new("lockrace");
    let _evidence = Evidence(&env);
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    // Each server with the file it says in: no pipe, which a server started at the same moment
    // would keep open for as long as it runs.
    let servers = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for worker in 0..8u64 {
            let (env, servers) = (&env, &servers);
            scope.spawn(move || {
                // The same commands in the same order at each run, different for each worker.
                let mut random = worker + 1;
                for n in 0..30 {
                    random = random.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    match (random >> 33) % 4 {
                        0 | 1 => {
                            let said = env.root.join(format!("server-{worker}-{n}.err"));
                            let err = std::fs::File::create(&said).unwrap();
                            let server = env.cmd(&["server"]).stdout(Stdio::null()).stderr(err).spawn();
                            servers.lock().unwrap().push((server.unwrap(), said));
                        }
                        2 => {
                            let out = bounded(env, false, &["kill"]);
                            let said = String::from_utf8_lossy(&out.stderr);
                            assert!(out.status.success() || said.contains("no kraal server"), "kill: {said}");
                        }
                        _ => {
                            let out = bounded(env, false, &["forget"]);
                            let said = String::from_utf8_lossy(&out.stderr);
                            assert!(out.status.success() || said.contains("is running"), "forget: {said}");
                        }
                    }
                }
            });
        }
    });
    // Each server ran until it was stopped, or was refused the session. One that had found
    // another one listening, or its socket taken, would have held the session with that one.
    let servers = servers.into_inner().unwrap();
    assert!(servers.len() > 100, "{} servers started", servers.len());
    let (mut ran, mut refused) = (0, 0);
    for (mut server, said) in servers {
        let start = Instant::now();
        let status = loop {
            if let Some(status) = server.try_wait().unwrap() {
                break status;
            }
            let _ = bounded(&env, false, &["kill"]);
            assert!(start.elapsed() < Duration::from_secs(30), "a server would not stop\n{}", evidence(&env));
        };
        let said = std::fs::read_to_string(said).unwrap();
        match status.success() {
            true => ran += 1,
            false => refused += 1,
        }
        assert_eq!(status.success(), said.is_empty(), "{said}");
        assert!(said.is_empty() || said.contains("holds session \"lockrace\""), "{said}");
    }
    assert!(ran > 0 && refused > 0, "{ran} servers ran, {refused} were refused");
    assert!(!lock_file(&env, "state").exists());
    // And the session is as any other.
    env.run(&["open", "w"]);
    assert!(env.run(&["panes"]).contains("one"));
    env.run(&["kill"]);
    env.run(&["forget"]);
    assert!(!lock_file(&env, "state").exists());
}

/// A workspace that cannot be restored (its shell is gone) stays saved as it was, scrollback
/// included, while the server saves the others; it comes back once it can.
#[test]
fn a_workspace_that_cannot_be_restored_stays_saved() {
    let env = Env::new("unrestored");
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(&bin.join("claude"), "#!/bin/sh\necho original-agent\nexec sleep 1000\n");
    let home = env.root.join("work-home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        env.root.join("config/kraal/accounts.toml"),
        format!("[[account]]\nname = 'work'\nagent = 'claude'\nhome = {:?}\n", home.to_str().unwrap()),
    )
    .unwrap();
    env.workspace("shells", "layout {\n    pane name=\"shell\"\n    pane name=\"agent\" agent=\"claude:work\"\n}\n");
    env.workspace("jobs", "layout {\n    pane name=\"job\" command=\"sh\" { args \"-c\" \"exec sleep 4246\"; }\n}\n");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let opened = env.cmd(&["open", "shells"]).env("PATH", &path).output().unwrap();
    assert!(opened.status.success(), "{}", String::from_utf8_lossy(&opened.stderr));
    env.wait_for_text("agent", "original-agent");
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let original_id = panes.as_array().unwrap().iter().find(|p| p["name"] == "agent").unwrap()["account_id"]
        .as_str()
        .unwrap()
        .to_string();
    env.run(&["open", "jobs"]);
    env.run(&["send", "-p", "shell", "--enter", "echo kept-$((4*4))"]);
    env.wait_for_text("shell", "kept-16");
    env.run(&["kill"]);
    // A released server saved only the name and launch home. The failing shell precedes
    // the agent, so binding only panes that successfully spawn would miss it.
    let mut state: serde_json::Value = serde_json::from_slice(&std::fs::read(env.state_file()).unwrap()).unwrap();
    let workspace = state["workspaces"].as_array_mut().unwrap().iter_mut().find(|ws| ws["name"] == "shells").unwrap();
    let spec = &mut workspace["tabs"][0]["root"]["Split"]["children"][1][0]["Pane"]["spec"];
    assert_eq!(spec["account_id"], original_id);
    spec.as_object_mut().unwrap().remove("account_id");
    spec["env"].as_array_mut().unwrap().retain(|pair| pair[0] != "KRAAL_ACCOUNT_ID");
    std::fs::write(env.state_file(), serde_json::to_vec(&state).unwrap()).unwrap();
    let names = || -> Vec<String> {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        panes.as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap_or_default().to_string()).collect()
    };
    // Without its shell, that workspace cannot come back; the other one does, and is saved.
    let shell = env.root.join("sh");
    let away = env.root.join("sh.away");
    std::fs::rename(&shell, &away).unwrap();
    env.run(&["open", "jobs"]);
    assert_eq!(names(), ["job"]);
    env.run(&["accounts", "rename", "claude:work", "renamed"]);
    env.run(&["config", "reload"]);
    env.run(&["accounts", "rm", "claude:renamed"]);
    env.run(&["accounts", "add", "claude", "renamed", "--home", home.to_str().unwrap(), "--no-login"]);
    env.run(&["config", "reload"]);
    env.run(&["kill"]);
    let state = std::fs::read_to_string(env.state_file()).unwrap();
    assert!(state.contains("\"shells\""), "the workspace was dropped: {state}");
    let saved: serde_json::Value = serde_json::from_str(&state).unwrap();
    let workspace = saved["workspaces"].as_array().unwrap().iter().find(|ws| ws["name"] == "shells").unwrap();
    let spec = &workspace["tabs"][0]["root"]["Split"]["children"][1][0]["Pane"]["spec"];
    assert_eq!(spec["account_id"], original_id);
    assert_eq!(spec["account"], "claude:renamed");
    std::fs::rename(&away, &shell).unwrap();
    env.run(&["open", "jobs"]);
    let mut back = names();
    back.sort();
    assert_eq!(back, ["agent", "job", "shell"]);
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let agent = panes.as_array().unwrap().iter().find(|p| p["name"] == "agent").unwrap();
    assert_eq!(agent["account_id"], original_id, "the replacement cannot capture the restored pane");
    assert_eq!(agent["account"], "claude:renamed");
    env.wait_for_text("shell", "kept-16");
}

/// A restart stops the session's programs only once the layout it brings back is saved: when it
/// cannot be, the session keeps running as it is, and the restart says why.
#[test]
fn a_restart_that_cannot_save_the_session_leaves_it_running() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new("unsavedrestart");
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    env.run(&["open", "w"]);
    wait_until(Duration::from_secs(10), "the layout was never saved", || env.state_file().exists());
    let pid = env.server_pid().expect("the server runs");
    let sessions = env.root.join("state/kraal/sessions-v2");
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o500)).unwrap();
    // A pane opened since the last save: the layout on disk is not the session's any more.
    env.run(&["run", "--name", "two", "--target", "one", "--", "sh"]);
    let out = env.try_run(&["restart", "--yes"]);
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o700)).unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("not restarting: the layout could not be saved"), "{err}");
    assert_eq!(env.server_pid(), Some(pid), "the session stopped");
    // Once it can be saved, the restart brings back the pane opened since.
    let said = env.run(&["restart", "--yes"]);
    assert!(said.contains("restarted session unsavedrestart"), "{said}");
    assert_ne!(env.server_pid(), Some(pid));
    assert!(env.run(&["panes"]).contains("two"));
}

/// `kraal kill` and `kraal restart` give up on a server that does not answer (stopped, here) in
/// a bounded time, instead of waiting for it for good.
#[test]
fn stopping_a_server_that_does_not_answer_gives_up_in_time() {
    let env = Env::new("frozen");
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    env.run(&["open", "w"]);
    let stopped = Stopped::stop(env.server_pid().expect("the server runs"));
    let quiet = |args: &[&str]| {
        let mut c = env.cmd(args);
        c.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped());
        c.spawn().unwrap()
    };
    let (kill, restart) = (quiet(&["kill"]), quiet(&["restart", "--yes"]));
    let outs = [finish(kill, Duration::from_secs(60)), finish(restart, Duration::from_secs(60))];
    drop(stopped);
    for out in outs {
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && err.contains("is still running after"), "{err}");
    }
}

#[test]
fn a_failed_start_leaves_no_empty_workspace() {
    let env = Env::new("emptyws");
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    // A server with no workspace yet, asked for an agent on an account that does not exist.
    let mut server = env.cmd(&["server"]).spawn().unwrap();
    let socket = env.root.join("run/kraal/emptyws.sock");
    let start = Instant::now();
    while std::os::unix::net::UnixStream::connect(&socket).is_err() {
        assert!(start.elapsed() < Duration::from_secs(5), "the server did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = env.cmd(&["agent", "claude", "--account", "nope"]).env("KRAAL_SOCKET", &socket).output().unwrap();
    assert!(!out.status.success());
    let list = env.run(&["workspaces", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&list).unwrap();
    assert!(v.as_array().unwrap().iter().all(|w| w["name"] != "main"), "{list}");
    env.run(&["open", "w"]);
    env.run(&["close", "one"]);
    let before = env.run(&["workspaces", "--json"]);
    let out = env.cmd(&["agent", "claude", "--account", "nope"]).env("KRAAL_SOCKET", &socket).output().unwrap();
    assert!(!out.status.success());
    assert_eq!(env.run(&["workspaces", "--json"]), before, "an existing empty workspace stays open");
    env.run(&["kill"]);
    let _ = server.wait();
}

#[test]
fn unknown_panes_and_workspaces_are_errors() {
    let env = Env::new("errors");
    env.workspace("w", "layout {\n    pane\n}\n");
    env.run(&["open", "w"]);
    let out = env.try_run(&["read", "-p", "nope"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no pane named"));
    let out = env.try_run(&["open", "missing"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("no workspace named"));
}

/// A name given where a workspace's goes that names no workspace but reads as a server
/// (`user@host`, or a server kraal knows) is answered with the command that was meant, and
/// kraal attaches nowhere; any other name gets the plain answer. No session is started only to
/// say that a name is unknown.
#[test]
fn a_server_given_where_a_workspace_goes_is_told_how_to_attach_to_it() {
    let env = Env::new("hostname");
    std::fs::write(env.root.join("config/kraal/servers.toml"), "[[server]]\nname = \"db\"\nhost = \"db.lan\"\n")
        .unwrap();
    let said = |name: &str| {
        let out = env.try_run(&["open", name]);
        assert!(!out.status.success(), "{name}");
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    let meant = |name: &str| format!("; to attach to kraal on that server: kraal attach --host {name}");
    let open_says = |when: &str| {
        for name in ["fpa@anvil", "db"] {
            let said = said(name);
            assert!(said.contains(&format!("no workspace named {name:?} in ")), "{when}: {said}");
            assert!(said.trim_end().ends_with(&meant(name)), "{when}: {said}");
        }
        let plain = said("nothing");
        assert!(plain.contains("no workspace named \"nothing\" in ") && !plain.contains("--host"), "{when}: {plain}");
    };

    open_says("without a session");
    assert_eq!(env.server_pid(), None, "a session was started only to say that a name is unknown");

    // A session running: its server answers the same, and a terminal is not attached to it.
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    open_says("with a session");
    let refused = RawClient::attach(&env, "fpa@anvil");
    refused.wait_for_message(&meant("fpa@anvil"));
    assert!(!refused.messages.lock().unwrap().iter().any(|m| m.contains("\"attached\"")));
    // Any other unknown name attaches all the same, with the plain message.
    let client = RawClient::attach(&env, "nothing");
    client.wait_for_message("\"attached\"");
    let rows = client.wait_for_row("no workspace named \"nothing\"");
    assert!(!rows.iter().any(|r| r.contains("--host")), "{rows:?}");
    // A workspace named like a server is a workspace.
    env.workspace("db", "layout {\n    pane name=\"data\"\n}\n");
    env.run(&["open", "db"]);
    let named = RawClient::attach(&env, "db");
    named.wait_for_message("\"attached\"");
    named.wait_for_row("data");
}

/// `kraal attach fpa@anvil` and `kraal fpa@anvil`, typed for `kraal attach --host fpa@anvil`:
/// the terminal gets the answer and is not attached to the local session, running or not.
#[cfg(target_os = "linux")]
#[test]
fn attaching_to_a_server_given_as_a_workspace_says_what_was_meant() {
    let env = Env::new("hostattach");
    let forms: [&[&str]; 2] = [&["attach", "fpa@anvil"], &["fpa@anvil"]];
    let check = |when: &str| {
        for args in forms {
            let shown = in_terminal(env.cmd(args), b"");
            // Said in the terminal as it is: the screen is not taken and given back.
            assert!(shown.starts_with("kraal: no workspace named \"fpa@anvil\" in "), "{when}, {args:?}: {shown:?}");
            assert!(shown.contains("to attach to kraal on that server: kraal attach --host fpa@anvil"), "{shown}");
        }
    };
    check("without a session");
    assert_eq!(env.server_pid(), None, "a session was started only to say that a name is unknown");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    check("with a session");
}

/// Outside a pane, a command that acts on a pane and is given none says how it takes one: the
/// option for those that take an option, the argument for the one that takes an argument.
#[test]
fn a_command_given_no_pane_says_how_it_takes_one() {
    let env = Env::new("whichpane");
    let said = |args: &[&str]| {
        let out = env.try_run(args);
        assert!(!out.status.success(), "{args:?}");
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    let with_option: [&[&str]; 6] =
        [&["send", "hi"], &["keys", "enter"], &["read"], &["wait"], &["rename", "x"], &["buffer", "paste"]];
    for args in with_option {
        assert!(said(args).contains("which pane? pass --pane"), "{args:?}: {}", said(args));
        // What the message names is taken: the command gets as far as looking for the session.
        let named = said(&[args, &["--pane", "1"][..]].concat());
        assert!(named.contains("no kraal server"), "{args:?}: {named}");
    }
    let close = said(&["close"]);
    assert!(close.contains("which pane? name it after `kraal close`") && !close.contains("--pane"), "{close}");
    assert!(said(&["close", "1"]).contains("no kraal server"));
}

/// `kraal servers add` for an address already saved leaves its entry as it is, and says what it
/// did: it never says "saved" for options it dropped. `--agents` is the one thing it sets there.
#[test]
fn adding_a_saved_server_again_never_claims_a_save() {
    let env = Env::new("readd");
    let file = env.root.join("config/kraal/servers.toml");
    let add = |args: &[&str]| env.try_run(&[&["servers", "add", "admin@db.lan"], args].concat());
    let said =
        |out: &Output| format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let saved = || std::fs::read_to_string(&file).unwrap();

    let out = add(&["--name", "db", "--group", "data"]);
    assert!(out.status.success() && said(&out).starts_with("saved db in "), "{}", said(&out));
    let first = saved();
    // An option that would change the entry is refused, by its name, and nothing is written.
    let options: [&[&str]; 7] = [
        &["--env", "prod"],
        &["--group", "web"],
        &["--jump", "bastion"],
        &["--identity", "~/.ssh/db"],
        &["--note", "primary"],
        &["--name", "other"],
        &["--env", "prod", "--agents", "main"],
    ];
    for option in options {
        let out = add(option);
        let said = said(&out);
        assert!(!out.status.success(), "{option:?}: {said}");
        assert!(said.contains("admin@db.lan is already saved as db") && said.contains(option[0]), "{option:?}: {said}");
        assert!(said.contains("`kraal servers edit`") && !said.contains("saved db in"), "{option:?}: {said}");
        assert_eq!(saved(), first, "{option:?}");
    }
    // What the entry already has changes nothing, and is not refused.
    for same in [&[][..], &["--name", "db", "--group", "data"]] {
        let out = add(same);
        assert!(out.status.success() && said(&out).starts_with("db is already saved in "), "{}", said(&out));
        assert_eq!(saved(), first);
    }
    // The session whose agents are listed is set on a saved server too, and said.
    let out = add(&["--agents", "main"]);
    assert!(out.status.success(), "{}", said(&out));
    assert!(said(&out).starts_with("db is already saved in ") && said(&out).contains("session main"), "{}", said(&out));
    assert_eq!(saved(), format!("{first}agents = \"main\"\n"));
    // With another port it is another address: a new entry, with what it was given.
    let out = env.try_run(&["servers", "add", "admin@db.lan:2222", "--env", "prod"]);
    assert!(out.status.success() && said(&out).starts_with("saved db-admin in "), "{}", said(&out));
    assert_eq!(saved().matches("env = \"prod\"").count(), 1);
}

#[test]
fn server_inventory_and_copy_plans() {
    let env = Env::new("servers");
    env.workspace(
        "lab",
        "layout {\n    pane split_direction=\"vertical\" {\n        pane name=\"web\" command=\"ssh\" { args \"-p\" \"2222\" \"admin@web.lab.invalid\"; }\n        pane name=\"db\" command=\"ssh\" { args \"admin@db.lab.invalid\"; }\n    }\n}\n",
    );
    let list = env.run(&["servers"]);
    assert!(
        list.contains("web") && list.contains("admin@web.lab.invalid:2222") && list.contains("workspace lab"),
        "{list}"
    );
    env.run(&["servers", "add", "ops@bastion.invalid", "--group", "infra"]);
    env.run(&["servers", "import"]);
    let list = env.run(&["servers", "list", "--group", "lab"]);
    assert_eq!(list.lines().count(), 2, "{list}");
    assert!(list.contains("saved"), "imported servers are saved: {list}");
    let json = env.run(&["servers", "list", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 3);
    // Which kraal session's agents to list: given to a server saved already, never a bad name.
    env.run(&["servers", "add", "ops@bastion.invalid", "--agents", "main"]);
    let json = env.run(&["servers", "list", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let bastion = v.as_array().unwrap().iter().find(|s| s["name"] == "bastion").cloned().unwrap();
    assert_eq!(bastion["agents"], "main", "{bastion}");
    assert!(!env.try_run(&["servers", "add", "ops@other.invalid", "--agents", "../x"]).status.success());

    // Copy plans, without touching the network.
    let file = env.root.join("report.txt");
    std::fs::write(&file, "x").unwrap();
    let up = env.run(&["cp", "--dry-run", file.to_str().unwrap(), "web:/tmp/"]);
    assert!(up.contains("-p 2222") && up.contains("admin@web.lab.invalid"), "{up}");
    let s2s = env.run(&["cp", "--dry-run", "web:/etc/hosts", "db:~/in/"]);
    assert!(s2s.contains("tar cf -") && s2s.contains("admin@db.lab.invalid"), "{s2s}");
    let bad = env.try_run(&["cp", "nosuch:/x", "."]);
    assert!(String::from_utf8_lossy(&bad.stderr).contains("unknown server"));

    env.run(&["servers", "rm", "bastion"]);
    assert!(!env.run(&["servers"]).contains("bastion"));
}

/// The commands that write servers.toml and accounts.toml edit the file the user wrote: its
/// comments, its order and the keys this kraal does not read are there after each of them.
#[test]
fn commands_that_write_the_inventories_keep_what_the_user_wrote() {
    let env = Env::new("keepfile");
    let config = env.root.join("config/kraal");
    let run = |args: &[&str]| {
        let out = env.cmd(args).output().unwrap();
        assert!(out.status.success(), "kraal {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    let read = |file: &Path| std::fs::read_to_string(file).unwrap();

    let servers = config.join("servers.toml");
    let by_hand = "# Ours. Ask ops first.\n\n\
                   [[server]]\nhost = \"bastion.example.com\"  # the way in\nname = \"bastion\"\ncolor = \"red\"\n";
    std::fs::write(&servers, by_hand).unwrap();
    run(&["servers", "add", "deploy@web.example.com", "--group", "web"]);
    let web = "\n[[server]]\nname = \"web\"\nhost = \"web.example.com\"\nuser = \"deploy\"\ngroups = [\"web\"]\n";
    assert_eq!(read(&servers), format!("{by_hand}{web}"));
    // What `kraal attach --host bastion` writes too: the session whose agents to list.
    run(&["servers", "add", "bastion.example.com", "--agents", "main"]);
    let watched = by_hand.replace("color = \"red\"\n", "color = \"red\"\nagents = \"main\"\n");
    assert_eq!(read(&servers), format!("{watched}{web}"));
    env.workspace("lab", "layout {\n    pane name=\"db\" command=\"ssh\" { args \"admin@db.example.com\"; }\n}\n");
    run(&["servers", "import"]);
    let db = "\n[[server]]\nname = \"db\"\nhost = \"db.example.com\"\nuser = \"admin\"\ngroups = [\"lab\"]\n";
    assert_eq!(read(&servers), format!("{watched}{web}{db}"));
    run(&["servers", "rm", "web"]);
    assert_eq!(read(&servers), format!("{watched}{db}"));
    // A file kraal cannot read is not written over.
    std::fs::write(&servers, "# mine\n[[server]\n").unwrap();
    assert!(!env.try_run(&["servers", "add", "ops@other.example.com"]).status.success());
    assert_eq!(read(&servers), "# mine\n[[server]\n");

    let accounts = config.join("accounts.toml");
    run(&["accounts", "limits", "--threshold", "80"]);
    let fresh = read(&accounts);
    assert!(fresh.starts_with("# kraal agent accounts."), "{fresh}");
    assert!(!fresh.contains("rule = []") && !fresh.contains("[claude]"), "{fresh}");

    let legacy = "# The team's accounts.\nrule = [] # no rules yet\n\n\
                  [claude] # reserved\n\n[limits]\nthreshold = 85\nfuture = 'kept'\n";
    std::fs::write(&accounts, legacy).unwrap();
    run(&["accounts", "limits", "--threshold", "80"]);
    assert_eq!(
        read(&accounts),
        legacy.replace("rule = []", "").replace("[claude]", "").replace("threshold = 85", "threshold = 80")
    );
    let broken = "# mine\nrule = []\n[claude]\n[[account]\n";
    std::fs::write(&accounts, broken).unwrap();
    assert!(!env.try_run(&["accounts", "limits", "--threshold", "80"]).status.success());
    assert_eq!(read(&accounts), broken);

    let by_hand = "# The team's accounts.\n\n[limits]\nmode = \"ask\"  # never by itself\nthreshold = 85\nwarn_at = 70\n\n\
                   # Work first.\n[[rule]]\nworkspace = \"work\"\naccounts = [\"work\", \"default\"]\n";
    std::fs::write(&accounts, by_hand).unwrap();
    run(&["accounts", "limits", "--threshold", "80"]);
    // Older servers keep the readable names; resolved identities go in their own field.
    let limited = format!(
        "{}account_ids = [\n    \"work\",\n    \"claude:@default-claude\",\n    \"codex:@default-codex\",\n]\n",
        by_hand.replace("threshold = 85", "threshold = 80")
    );
    assert_eq!(read(&accounts), limited);
    let home = env.root.join("codex-work").display().to_string();
    run(&["accounts", "add", "codex", "work", "--home", &home, "--no-login"]);
    let parsed: toml::Value = toml::from_str(&read(&accounts)).unwrap();
    let id = parsed["account"][0]["id"].as_str().unwrap();
    let account = format!("\n[[account]]\nid = {id:?}\nname = \"work\"\nagent = \"codex\"\nhome = \"{home}\"\n");
    // The previously unresolved name binds when its account is added, and stays bound.
    let limited = limited.replace("    \"work\",", &format!("    \"codex:@{id}\","));
    assert_eq!(read(&accounts), format!("{limited}{account}"));
    run(&["accounts", "rename", "codex:work", "job"]);
    let renamed = format!("{limited}{account}")
        .replace("name = \"work\"", "name = \"job\"")
        .replace("accounts = [\"work\",", "accounts = [\"job\",");
    assert_eq!(read(&accounts), renamed);
    run(&["accounts", "rm", "codex:job"]);
    assert_eq!(read(&accounts), limited.replace("accounts = [\"work\",", &format!("accounts = [\"codex:@{id}\",")));
}

#[test]
fn moves_never_lose_the_source() {
    let env = Env::new("move");
    let data = env.root.join("data");
    let out = env.root.join("out");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&out).unwrap();
    let report = data.join("report");
    std::fs::write(&report, "only copy").unwrap();
    std::os::unix::fs::symlink(&data, env.root.join("alias")).unwrap();
    let path = |p: &Path| p.to_str().unwrap().to_string();
    // Into its own directory, seen through a link: refused, nothing deleted.
    let same = env.try_run(&["cp", "--move", &path(&report), &path(&env.root.join("alias"))]);
    assert!(!same.status.success() && String::from_utf8_lossy(&same.stderr).contains("already in"));
    assert_eq!(std::fs::read_to_string(&report).unwrap(), "only copy");
    env.run(&["cp", "--move", &path(&report), &path(&out)]);
    assert_eq!(std::fs::read_to_string(out.join("report")).unwrap(), "only copy");
    assert!(!report.exists());
    // What must not be replaced stays, and so does the source.
    std::fs::write(data.join("b"), "mine").unwrap();
    std::fs::write(out.join("b"), "theirs").unwrap();
    let kept = env.try_run(&["cp", "--move", "--no-replace", &path(&data.join("b")), &path(&out)]);
    assert!(!kept.status.success());
    assert_eq!(std::fs::read_to_string(out.join("b")).unwrap(), "theirs");
    assert_eq!(std::fs::read_to_string(data.join("b")).unwrap(), "mine");
}

#[test]
fn missing_command_does_not_break_the_workspace() {
    let env = Env::new("missing");
    env.workspace(
        "w",
        "layout {\n    pane split_direction=\"vertical\" {\n        pane name=\"ok\"\n        pane name=\"bad\" command=\"kraal-test-no-such-program\"\n    }\n}\n",
    );
    env.run(&["open", "w"]);
    let json = env.run(&["panes", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let bad = v.as_array().unwrap().iter().find(|p| p["name"] == "bad").expect("the failing pane is kept");
    assert_eq!(bad["suspended"], true, "{json}");
    env.wait_for_text("bad", "cannot start");
    // The server is still alive and the workspace still usable.
    assert!(env.run(&["workspaces"]).contains("open"));
}

/// A program found but that cannot run (its interpreter is missing) is known only once the pane
/// started it: the pane then waits for Enter all the same, saying why, and the others run.
#[test]
fn a_program_that_cannot_run_after_all_waits_for_enter() {
    let env = Env::new("cantrun");
    let broken = env.root.join("broken");
    write_script(&broken, "#!/no/such/interpreter\n");
    env.workspace(
        "w",
        &format!(
            "layout {{\n    pane split_direction=\"vertical\" {{\n        pane name=\"ok\"\n        pane name=\"bad\" command={:?}\n    }}\n}}\n",
            broken.display().to_string()
        ),
    );
    env.run(&["open", "w"]);
    env.wait_for_text("bad", "cannot start");
    env.wait_for_text("bad", "No such file or directory");
    let start = Instant::now();
    loop {
        let v: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let bad = v.as_array().unwrap().iter().find(|p| p["name"] == "bad").cloned().expect("the pane is kept");
        if bad["suspended"] == true {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(5), "{bad}");
        std::thread::sleep(Duration::from_millis(50));
    }
    env.run(&["send", "-p", "ok", "-e", "echo still-here"]);
    env.wait_for_text("ok", "still-here");
    write_script(&broken, "#!/bin/sh\nprintf 'repaired-start\\n'\nexec sleep 1000\n");
    env.run(&["wait", "-p", "bad", "--until", "exit", "--timeout", "10"]);
    env.run(&["focus", "bad"]);
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_row("cannot start");
    client.send(b"\r");
    env.wait_for_text("bad", "repaired-start");
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let bad = panes.as_array().unwrap().iter().find(|p| p["name"] == "bad").unwrap();
    assert_eq!(bad["suspended"], false);
}

#[test]
fn restore_suspends_commands_but_reconnects_ssh() {
    let env = Env::new("restoresafe");
    // A fake `ssh` that just prints, to stand for a reconnecting SSH pane.
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let ssh = bin.join("ssh");
    write_script(&ssh, "#!/bin/sh\necho \"connected $*\"\nexec sleep 1000\n");
    env.workspace(
        "w",
        &format!(
            "layout {{\n    pane split_direction=\"vertical\" {{\n        pane name=\"srv\" command=\"{0}\" {{ args \"admin@db\"; }}\n        pane name=\"deploy\" command=\"sh\" {{ args \"-c\" \"echo DEPLOYED; exec sleep 1000\"; }}\n        pane name=\"remote\" command=\"{0}\" {{ args \"-t\" \"admin@db\" \"systemctl restart app\"; }}\n    }}\n}}\n",
            ssh.display()
        ),
    );
    env.run(&["open", "w"]);
    env.wait_for_text("deploy", "DEPLOYED");
    env.wait_for_text("srv", "connected admin@db");
    env.wait_for_text("remote", "connected");
    env.run(&["kill"]);
    env.run(&["open", "w"]);
    // What each pane printed before shows above the restore marker; what runs now writes below.
    env.wait_for_text_since_restore("srv", "connected admin@db");
    assert!(env.run(&["read", "-p", "deploy"]).contains("DEPLOYED"), "what it printed before shows");
    let screen = env.wait_for_text_since_restore("deploy", "restored session");
    assert!(!screen.lines().any(|l| l.trim() == "DEPLOYED"), "the command must not run again by itself:\n{screen}");
    // An ssh that runs a command on the server is a command too, not a shell to reconnect.
    let screen = env.wait_for_text_since_restore("remote", "restored session");
    assert!(!screen.contains("connected"), "the remote command must not run again by itself:\n{screen}");
}

#[test]
fn scrollback_survives_a_server_restart() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new("scrollback");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    // More lines than the screen holds, then a coloured line with wide characters.
    env.run(&["send", "-p", "shell", "--enter", "seq 1 300; printf '\\033[31m%s\\033[0m 日本\\n' red-$((6*7))"]);
    env.wait_for_text("shell", "red-42 日本");
    env.run(&["kill"]);
    // Saved with its colours, for the user's eyes only.
    let dir = env.root.join("state/kraal/scrollback-v2/scrollback");
    let files: Vec<PathBuf> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
    assert_eq!(files.len(), 1, "{files:?}");
    let saved = std::fs::read_to_string(&files[0]).unwrap();
    assert!(saved.contains("\x1b[0;31mred-42\x1b[0m 日本"), "{saved:?}");
    assert_eq!(std::fs::metadata(&files[0]).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    // Back after the restart, history included, with what the new shell writes below a marker.
    env.run(&["open", "w"]);
    let screen = env.run(&["read", "-p", "shell"]);
    assert!(screen.contains("red-42 日本") && screen.contains("kraal: restored from"), "{screen}");
    let all = env.run(&["read", "-p", "shell", "--lines", "1000"]);
    assert!(all.lines().any(|l| l == "1"), "{all}");
    env.run(&["send", "-p", "shell", "--enter", "echo after-$((1+1))"]);
    env.wait_for_text_since_restore("shell", "after-2");
    // Restarted again: both restores show, in order.
    env.run(&["kill"]);
    env.run(&["open", "w"]);
    let all = env.run(&["read", "-p", "shell", "--lines", "1000"]);
    let at = |text: &str, from: usize| match all[from..].find(text) {
        Some(i) => from + i,
        None => panic!("no {text:?} after byte {from} of:\n{all}"),
    };
    let first = at("kraal: restored from", at("red-42 日本", at("300", 0)));
    at("kraal: restored from", at("after-2", first));
    // `kraal forget` deletes it with the layout.
    env.run(&["kill"]);
    env.run(&["forget"]);
    assert!(!dir.exists() && !env.state_file().exists());
    // Turned off, what was saved goes, and nothing is saved any more.
    env.run(&["open", "w"]);
    env.run(&["send", "-p", "shell", "--enter", "echo secret-$((2+2))"]);
    env.wait_for_text("shell", "secret-4");
    env.run(&["kill"]);
    assert!(dir.exists());
    let config = env.root.join("config/kraal/config.toml");
    let base = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{base}save_scrollback = false\n")).unwrap();
    env.run(&["open", "w"]);
    let screen = env.run(&["read", "-p", "shell"]);
    assert!(!screen.contains("secret-4") && !screen.contains("restored from"), "{screen}");
    env.run(&["kill"]);
    assert!(!dir.exists());
}

/// A pane whose scrollback could not be written is saved again once it can be, though it
/// printed nothing since; `kraal kill` says when it still cannot be.
#[test]
fn scrollback_that_could_not_be_written_is_saved_later() {
    let env = Env::new("scrollfail");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    // A file where the session's scrollback directory goes: nothing can be written there.
    let dir = env.root.join("state/kraal/scrollback-v2/scrollfail");
    std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
    std::fs::write(&dir, "in the way").unwrap();
    env.run(&["open", "w"]);
    env.run(&["send", "-p", "shell", "--enter", "echo once-$((5*5))"]);
    env.wait_for_text("shell", "once-25");
    // Long enough for a save of everything it printed to be tried, and to fail.
    std::thread::sleep(Duration::from_secs(11));
    assert!(std::fs::read_to_string(env.state_file()).unwrap().contains(".ans\""), "the state names its file");
    std::fs::remove_file(&dir).unwrap();
    env.run(&["kill"]);
    env.run(&["open", "w"]);
    let screen = env.run(&["read", "-p", "shell"]);
    assert!(screen.contains("once-25") && screen.contains("kraal: restored from"), "{screen}");
    // Still impossible when the server stops: said.
    std::fs::rename(&dir, env.root.join("aside")).unwrap();
    std::fs::write(&dir, "in the way").unwrap();
    env.run(&["send", "-p", "shell", "--enter", "echo twice-$((6*6))"]);
    env.wait_for_text_since_restore("shell", "twice-36");
    let out = env.try_run(&["kill"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("the scrollback of a pane could not be saved"), "{err}");
}

/// A server that dies without stopping (a crash) saved what its panes showed shortly before.
#[cfg(target_os = "linux")]
#[test]
fn scrollback_survives_a_crash() {
    let env = Env::new("crash");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    env.run(&["send", "-p", "shell", "--enter", "echo before-$((3*3))"]);
    env.wait_for_text("shell", "before-9");
    // Written while the pane prints, then named by the state.
    let dir = env.root.join("state/kraal/scrollback-v2/crash");
    let saved = || {
        let files = std::fs::read_dir(&dir).into_iter().flatten().flatten();
        files.filter(|e| std::fs::read_to_string(e.path()).unwrap_or_default().contains("before-9")).count() == 1
    };
    let named = || std::fs::read_to_string(env.state_file()).unwrap_or_default().contains(".ans\"");
    let start = Instant::now();
    while !(saved() && named()) {
        assert!(start.elapsed() < Duration::from_secs(10), "the scrollback was not saved while the server ran");
        std::thread::sleep(Duration::from_millis(50));
    }
    let pid = env.server_pid().expect("the server's pid");
    // SAFETY: plain syscall.
    unsafe { libc::kill(pid, libc::SIGKILL) };
    wait_gone(&PathBuf::from(format!("/proc/{pid}")));
    env.run(&["open", "w"]);
    let screen = env.run(&["read", "-p", "shell"]);
    assert!(screen.contains("before-9") && screen.contains("kraal: restored from"), "{screen}");
}

#[test]
fn config_check_reports_tool_problems() {
    let env = Env::new("toolcheck");
    let cfg = env.root.join("config/kraal/config.toml");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str("\n[tools.mine]\ncommand = \"tig\"\nkey = \"g\"\n[tools.both]\ncommand = \"true\"\nwhen = [\"server\", \"local\"]\n");
    std::fs::write(&cfg, text).unwrap();
    let out = env.try_run(&["config", "check"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("tools.mine: key \"R g\" is already \"Git and worktrees\""), "{err}");
    assert!(err.contains("tools.both: when"), "{err}");
    // A workspace file that cannot be read is an error, not a warning.
    env.workspace("broken", "layout {\n    pane {\n");
    let out = env.try_run(&["config", "check"]);
    assert!(!out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(String::from_utf8_lossy(&out.stderr).contains("broken.kdl"));
}

/// `kraal config check` run in `dir`: whether it passed, and what it said on stderr.
fn config_check(env: &Env, dir: &Path) -> (bool, String) {
    let out = env.cmd(&["config", "check"]).current_dir(dir).output().unwrap();
    (out.status.success(), String::from_utf8(out.stderr).unwrap())
}

/// A workspace named like a command of kraal: its name alone runs the command, so the check
/// names the file and the way to open it; `kraal open` takes it like any other.
#[test]
fn the_check_names_a_workspace_that_a_command_hides() {
    let env = Env::new("wsname");
    for name in ["files", "shop"] {
        env.workspace(name, "layout {\n    pane name=\"main\"\n}\n");
    }
    let (ok, said) = config_check(&env, &env.root);
    assert!(ok, "{said}");
    let advice =
        "workspaces/files.kdl: `kraal files` is a command of kraal: open this workspace with `kraal attach files`";
    assert!(said.lines().count() == 1 && said.trim_end().ends_with(advice), "{said}");
    env.run(&["open", "files"]);
    let workspaces: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    let open = |name: &str| workspaces.as_array().unwrap().iter().any(|w| w["name"] == name && w["open"] == true);
    assert!(open("files") && !open("shop"), "{workspaces}");
}

/// What `kraal config default` prints is a file its own check has nothing to say about, in
/// either preset.
#[test]
fn the_default_config_passes_its_own_check() {
    let env = Env::new("defaultcheck");
    let default = env.run(&["config", "default"]);
    for preset in ["kraal", "classic"] {
        let config = default.replace("preset = \"kraal\"", &format!("preset = \"{preset}\""));
        assert!(config.contains(&format!("preset = \"{preset}\"")), "{config}");
        std::fs::write(env.root.join("config/kraal/config.toml"), config).unwrap();
        assert_eq!(config_check(&env, &env.root), (true, String::new()), "preset {preset}");
    }
}

/// A key kraal does not read is a warning wherever configuration problems are told (the check,
/// a session's start, a reload) and never an error: the rest of the file applies, since the
/// file may also serve a kraal that reads that key.
#[test]
fn unknown_config_keys_are_warnings_and_never_stop_a_session() {
    for preset in ["kraal", "classic"] {
        let env = Env::new(&format!("unknown-{preset}"));
        std::fs::create_dir_all(env.root.join("state/kraal")).unwrap();
        std::fs::write(env.root.join("state/kraal/welcomed"), "").unwrap();
        let path = env.root.join("config/kraal/config.toml");
        let base = std::fs::read_to_string(&path).unwrap();
        let write = |more: &str| {
            // A tool with a key of another kraal's is in the file too: it loads without it.
            let tool = "[tools.pods]\ncommand = \"true\"\nread_onyl = true\n";
            let text = format!("{base}future_setting = 1\n[keys]\npreset = \"{preset}\"\n{tool}{more}");
            std::fs::write(&path, text).unwrap();
        };
        write("");
        let unknown = "warning: future_setting: unknown key\n\
                       warning: tools.pods.read_onyl: unknown key (did you mean read_only?)\n";
        assert_eq!(config_check(&env, &env.root), (true, unknown.into()));

        // The session starts with the shell the file sets, and says it to who attaches.
        env.workspace("w", "layout {\n    pane name=\"first\"\n}\n");
        env.run(&["open", "w"]);
        env.wait_for_text("first", "$");
        let client = RawClient::attach(&env, "w");
        client.wait_for_screen("future_setting: unknown key");
        let rows = || {
            let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
            panes[0]["rows"].as_u64().unwrap()
        };
        let with_bar = rows();

        // A reload says it too, and applies the rest: the panes get the bottom bar's row.
        write("[bar]\nkeys = \"off\"\nkyes = \"context\"\n");
        let out = env.try_run(&["config", "reload"]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(
            String::from_utf8_lossy(&out.stderr),
            format!("warning: bar.kyes: unknown key (did you mean keys?)\n{unknown}")
        );
        wait_until(Duration::from_secs(10), "the reload did not apply", || rows() == with_bar + 1);
    }
}

/// The check reads every file kraal reads: the inventories, where a key it does not read is a
/// warning too, your runbooks, and what the repository it runs in ships.
#[test]
fn config_check_reads_the_inventories_the_runbooks_and_the_repository() {
    let env = Env::new("checkall");
    let config = env.root.join("config/kraal");
    let (servers, accounts) = (config.join("servers.toml"), config.join("accounts.toml"));
    std::fs::write(&servers, "[[server]]\nname = \"web\"\nhost = \"web.example.com\"\njmp = \"bastion\"\n").unwrap();
    std::fs::write(&accounts, "[limits]\ntreshold = 80\n").unwrap();
    let runbooks = config.join("runbooks");
    std::fs::create_dir_all(&runbooks).unwrap();
    std::fs::write(runbooks.join("reload.toml"), "steps = [\"true\"]\n").unwrap();
    std::fs::write(runbooks.join("notes.md"), "# Notes\n\n```sh\ntrue\n```\n").unwrap();
    let repo = env.root.join("repo");
    let _git = git_repo(
        &repo,
        &[(".kraal/worktree.toml", "copy = [\".env\"]\n"), (".kraal/runbooks/deploy.toml", "steps = [\"true\"]\n")],
    );
    let warnings = format!(
        "warning: {}: limits.treshold: unknown key (did you mean threshold?)\n\
         warning: {}: server[0].jmp: unknown key (did you mean jump?)\n",
        accounts.display(),
        servers.display()
    );
    assert_eq!(config_check(&env, &repo), (true, warnings.clone()));

    // A file kraal would refuse is an error: one of your runbooks, wherever the check runs.
    std::fs::write(runbooks.join("broken.toml"), "stpes = [\"true\"]\n").unwrap();
    let (ok, said) = config_check(&env, &env.root);
    assert!(!ok && said.contains("runbooks/broken.toml") && said.contains("unknown field `stpes`"), "{said}");
    std::fs::remove_file(runbooks.join("broken.toml")).unwrap();

    // The repository's files, in that repository only.
    std::fs::write(repo.join(".kraal/worktree.toml"), "setpu = [\"npm ci\"]\n").unwrap();
    std::fs::write(repo.join(".kraal/runbooks/deploy.toml"), "steps = []\n").unwrap();
    let (ok, said) = config_check(&env, &repo);
    assert!(!ok && said.contains(".kraal/worktree.toml: ") && said.contains("unknown field `setpu`"), "{said}");
    assert!(said.contains(".kraal/runbooks/deploy.toml: no steps"), "{said}");
    assert!(said.ends_with("kraal: 2 files could not be read\n"), "{said}");
    assert_eq!(config_check(&env, &env.root), (true, warnings));
}

#[test]
fn events_stream_and_hooks() {
    let env = Env::new("events");
    let cfg = env.root.join("config/kraal/config.toml");
    let marker = env.root.join("hook.txt");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str(&format!(
        "\n[hooks]\n\"pane.failed\" = 'echo \"$KRAAL_EVENT $KRAAL_CODE $KRAAL_NAME\" > {}'\n",
        marker.display()
    ));
    std::fs::write(&cfg, text).unwrap();
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    // From the start of the session: nothing is missed, however late the subscription is live.
    env.run(&["run", "--name", "job", "--", "sh", "-c", "exit 3"]);
    let (mut events, lines) = env.follow(&["events", "--since", "0", "pane.*"]);
    let mut seen = Vec::new();
    while seen.len() < 3 {
        let line = lines.recv_timeout(Duration::from_secs(10)).expect("an event in time");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        if v["name"] != "job" {
            continue;
        }
        seen.push(v["type"].as_str().unwrap().to_string());
        if v["type"] == "pane.exited" {
            assert_eq!(v["code"], 3, "{line}");
            assert_eq!(v["command"][0], "sh", "{line}");
        }
    }
    assert_eq!(seen, vec!["pane.opened", "pane.exited", "pane.failed"]);
    let _ = events.kill();
    let _ = events.wait();
    let start = Instant::now();
    while !marker.exists() && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "pane.failed 3 job");
    // Live events come too, after the replay.
    let (mut live, lines) = env.follow(&["events", "--since", "0", "pane.failed"]);
    let line = lines.recv_timeout(Duration::from_secs(10)).expect("replayed event");
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!((v["type"].as_str(), v["name"].as_str()), (Some("pane.failed"), Some("job")), "{line}");
    assert!(v["seq"].as_u64().is_some() && v["boot"].as_u64().is_some(), "{line}");
    env.run(&["run", "--name", "again", "--", "sh", "-c", "exit 4"]);
    let line = lines.recv_timeout(Duration::from_secs(10)).expect("live event");
    assert!(line.contains("\"again\""), "{line}");
    let _ = live.kill();
    let _ = live.wait();
    // A replay longer than a client's queue still arrives whole.
    for i in 0..40 {
        env.run(&["run", "--name", &format!("many{i}"), "-x", "--", "true"]);
    }
    let (mut all, lines) = env.follow(&["events", "--since", "0", "pane.opened"]);
    let mut opened = 0;
    while opened < 40 {
        let line = lines.recv_timeout(Duration::from_secs(10)).expect("the whole replay");
        if line.contains("\"many") {
            opened += 1;
        }
    }
    let _ = all.kill();
    let _ = all.wait();
    let out = env.try_run(&["events", "pane.exitd"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no such event"));
}

/// The lines of `out` as they come, on the receiver: the first one, then the others once
/// something is sent to the sender. Until then, what writes to `out` has a reader that reads
/// nothing.
fn lines_when_asked(
    out: impl std::io::Read + Send + 'static,
) -> (std::sync::mpsc::Receiver<String>, std::sync::mpsc::Sender<()>) {
    use std::io::BufRead;
    let (tx, lines) = std::sync::mpsc::channel();
    let (go, asked) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut out = std::io::BufReader::new(out).lines().map_while(Result::ok);
        if let Some(first) = out.next() {
            let _ = tx.send(first);
        }
        let _ = asked.recv();
        for line in out {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    (lines, go)
}

/// `kraal notify`, asked through the session's socket: no process to start for each one.
fn notify(env: &Env, title: &str, body: &str) {
    use std::io::Read;
    let socket = env.root.join("run/kraal").join(format!("{}.sock", env.session));
    let mut stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
    let request = serde_json::json!({ "cmd": "notify", "title": title, "body": body });
    let message = serde_json::json!({ "type": "request", "version": 2, "request": request });
    raw_frame(&mut stream, 0, message.to_string().as_bytes());
    // Answered: the event is out.
    stream.read_exact(&mut [0u8; 5]).unwrap();
}

/// A `kraal events` reader that falls too far behind is dropped, and told: after the events it
/// was sent, a last line says so and where to go on from, and the command fails with the reason.
/// Going on from there, it has missed nothing.
#[test]
fn a_reader_too_slow_for_the_events_is_told_and_misses_nothing() {
    use std::process::Stdio;
    use std::sync::mpsc::RecvTimeoutError;
    let env = Env::new("slowreader");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    notify(&env, "first", "");
    let mut reader = env
        .cmd(&["events", "--since", "0", "notification"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (lines, go) = lines_when_asked(reader.stdout.take().unwrap());
    // It follows the session (it read the event from before), then reads nothing while far more
    // happens than the system and kraal keep waiting for a reader.
    let first = lines.recv_timeout(Duration::from_secs(10)).expect("the event from before");
    assert!(first.contains(r#""title":"first""#), "{first}");
    const SENT: usize = 300;
    let body = "x".repeat(16 << 10);
    for i in 0..SENT {
        notify(&env, &i.to_string(), &body);
    }
    go.send(()).unwrap();
    let mut read: Vec<serde_json::Value> = Vec::new();
    loop {
        match lines.recv_timeout(Duration::from_secs(20)) {
            Ok(line) => read.push(serde_json::from_str(&line).unwrap()),
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => panic!("the stream never ended, {} lines read", read.len()),
        }
    }
    // The events it was sent, in order, then the last line.
    let last = read.pop().expect("a last line");
    let titles: Vec<&str> = read.iter().filter_map(|e| e["title"].as_str()).collect();
    assert_eq!(titles, (0..read.len()).map(|i| i.to_string()).collect::<Vec<_>>());
    assert!(read.len() < SENT, "never dropped");
    assert_eq!((last["type"].as_str(), last["reason"].as_str()), (Some("events.dropped"), Some("too slow")), "{last}");
    let (since, boot) = (last["since"].as_u64().unwrap(), last["boot"].as_u64().unwrap());
    let before = read.last().expect("some events before it was dropped");
    assert!(before["seq"].as_u64().unwrap() <= since && before["boot"] == boot, "{last}");
    let out = finish(reader, Duration::from_secs(10));
    let said = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{said}");
    assert!(said.contains("too slow") && said.contains(&format!("--since {since} --boot {boot}")), "{said}");
    // From where the last line says: every event it was not sent.
    let (since, boot) = (since.to_string(), boot.to_string());
    let (mut rest, lines) = env.follow(&["events", "--since", &since, "--boot", &boot, "notification"]);
    for i in read.len()..SENT {
        let line = lines.recv_timeout(Duration::from_secs(10)).unwrap_or_else(|_| panic!("event {i} never came"));
        let event: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(event["title"], i.to_string(), "{} {}", event["type"], event["seq"]);
    }
    let _ = rest.kill();
    let _ = rest.wait();
}

/// What else ends a `kraal events` stream ends it as before, without a word and with success:
/// the session stopped. Detaching or closing its last pane ends no stream.
#[test]
fn the_event_stream_ends_quietly_with_the_session() {
    use std::process::Stdio;
    let env = Env::new("eventsend");
    env.workspace("w", "layout {\n    pane name=\"only\"\n}\n");
    for end in [&["kill"][..], &["close", "only"]] {
        env.run(&["open", "w"]);
        let mut reader =
            env.cmd(&["events", "notification"]).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        let (lines, go) = lines_when_asked(reader.stdout.take().unwrap());
        go.send(()).unwrap();
        let mut client = RawClient::attach(&env, "w");
        client.wait_for_message("\"attached\"");
        raw_frame(&mut client.stream, 0, br#"{"type":"detach"}"#);
        client.wait_for_message("detached");
        // The reader may not be following yet: said until it reads it.
        let start = Instant::now();
        loop {
            notify(&env, "still", "there");
            if lines.recv_timeout(Duration::from_millis(100)).is_ok() {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "no event after a terminal detached");
        }
        env.run(end);
        if end[0] == "close" {
            assert!(reader.try_wait().unwrap().is_none(), "the empty session still owns the stream");
            env.run(&["kill"]);
        }
        let out = finish(reader, Duration::from_secs(10));
        let said = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success() && said.is_empty(), "after {end:?}: {:?} {said}", out.status);
        assert!(lines.iter().all(|line| line.contains(r#""title":"still""#)), "after {end:?}");
    }
}

/// Each `ssh.started` has its `ssh.ended`, as a reader keeping count of the connections needs:
/// when the connection drops (the pane stays, Enter connects again) and when the pane closes.
#[test]
fn every_ssh_that_starts_ends() {
    let env = Env::new("sshends");
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    // A connection to `drop` drops after a moment; the others stay.
    let ssh = bin.join("ssh");
    write_script(
        &ssh,
        "#!/bin/sh\necho \"connected $*\"\n[ \"$1\" = admin@drop ] && sleep 1 && exit 255\nexec sleep 1000\n",
    );
    let ssh = ssh.display().to_string();
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let (mut events, lines) = env.follow(&["events", "--since", "0", "ssh.*"]);
    let mut seen: Vec<String> = Vec::new();
    let mut wait = |what: &str| {
        while !seen.iter().any(|s| s == what) {
            let line = lines.recv_timeout(Duration::from_secs(10)).unwrap_or_else(|_| panic!("no {what}: {seen:?}"));
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            seen.push(format!("{} {}", v["type"].as_str().unwrap(), v["name"].as_str().unwrap()));
        }
    };
    env.run(&["run", "--name", "dropped", "--", &ssh, "admin@drop"]);
    wait("ssh.started dropped");
    wait("ssh.ended dropped");
    env.wait_for_text("dropped", "connected admin@drop");
    env.run(&["run", "--name", "closed", "--", &ssh, "admin@db"]);
    wait("ssh.started closed");
    env.run(&["close", "closed"]);
    wait("ssh.ended closed");
    let _ = events.kill();
    let _ = events.wait();
}

/// A directory a quiet pane is in that becomes a Terraform one (`terraform init` run from
/// elsewhere) is guarded as soon as it is: the change is told, not looked for now and then.
#[test]
fn a_directory_that_becomes_a_terraform_one_is_guarded() {
    let env = Env::new("tfnew");
    let tf = env.root.join("infra");
    std::fs::create_dir_all(&tf).unwrap();
    env.workspace("w", &format!("layout {{\n    pane name=\"t\" cwd={:?}\n}}\n", tf.display().to_string()));
    env.run(&["open", "w"]);
    env.wait_for_text("t", "$");
    let production = || {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let t = panes.as_array().unwrap().iter().find(|p| p["name"] == "t").cloned().unwrap();
        t["production"].as_str().map(String::from)
    };
    // Known not to be a Terraform directory, and quiet.
    std::thread::sleep(Duration::from_secs(4));
    assert_eq!(production(), None);
    std::fs::create_dir_all(tf.join(".terraform")).unwrap();
    std::fs::write(tf.join(".terraform/environment"), "prod\n").unwrap();
    let start = Instant::now();
    while production().is_none() {
        assert!(start.elapsed() < PATIENCE, "never seen as production");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(production().as_deref(), Some("Terraform workspace prod"));
    let out = env.try_run(&["send", "-p", "t", "echo", "hi"]);
    assert!(!out.status.success(), "sent to production");
}

#[test]
fn production_panes_refuse_unasked_cli_input() {
    let env = Env::new("prodguard");
    let tf = env.root.join("infra");
    std::fs::create_dir_all(tf.join(".terraform")).unwrap();
    std::fs::write(tf.join(".terraform/environment"), "prod\n").unwrap();
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    env.run(&["run", "--cwd", &tf.display().to_string(), "--name", "ops", "--", "sh"]);
    // The Terraform workspace is seen by the directory scan, every few seconds.
    let start = Instant::now();
    loop {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let ops = panes.as_array().unwrap().iter().find(|p| p["name"] == "ops").cloned().unwrap();
        if ops["production"].is_string() {
            assert_eq!(ops["production"], "Terraform workspace prod");
            assert_eq!(ops["contexts"][0], "tf:prod");
            break;
        }
        assert!(start.elapsed() < PATIENCE, "never seen as production: {ops}");
        std::thread::sleep(Duration::from_millis(100));
    }
    let out = env.try_run(&["send", "-p", "ops", "echo", "hi"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("is production"), "{}", String::from_utf8_lossy(&out.stderr));
    env.run(&["send", "-p", "ops", "--production", "--enter", "echo", "SENT-ON-PURPOSE"]);
    env.wait_for_text("ops", "SENT-ON-PURPOSE");
    env.run(&["send", "-p", "main", "--enter", "echo", "not-production"]);
}

#[cfg(unix)]
#[test]
fn runbooks_run_locally_and_on_servers() {
    let env = Env::new("runbooks");
    // A fake ssh: runs the remote script here, and fails for the server "bad".
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(
        &bin.join("ssh"),
        "#!/bin/sh\nfor a; do dest=$cmd; cmd=$a; done\ncase $dest in *bad*) echo unreachable >&2; exit 255;; esac\nexec sh -c \"$cmd\"\n",
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let run = |args: &[&str]| env.cmd(args).env("PATH", &path).output().unwrap();
    std::fs::write(
        env.root.join("config/kraal/servers.toml"),
        "[[server]]\nname = \"a\"\nhost = \"a.lan\"\ngroups = [\"web\"]\n\n[[server]]\nname = \"bad\"\nhost = \"bad.lan\"\ngroups = [\"web\"]\n\n[[server]]\nname = \"c\"\nhost = \"c.lan\"\ngroups = [\"web\"]\n",
    )
    .unwrap();
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    assert!(run(&["open", "w"]).status.success());

    let out = run(&["runbook", "new", "greet"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let file = env.root.join("config/kraal/runbooks/greet.toml");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), file.display().to_string());
    std::fs::write(
        &file,
        "description = \"say hello\"\nsteps = [\"echo hello $WHO\", \"echo bye\"]\n[[param]]\nname = \"WHO\"\ndefault = \"world\"\n",
    )
    .unwrap();
    let list = String::from_utf8(run(&["runbook", "list"]).stdout).unwrap();
    assert!(list.contains("greet") && list.contains("say hello"), "{list}");

    let out = run(&["runbook", "run", "greet", "--dry-run", "-p", "WHO=kraal"]);
    let plan = String::from_utf8_lossy(&out.stdout);
    assert!(
        plan.contains("Run greet on this machine?")
            && plan.contains("WHO=kraal")
            && plan.contains("1. echo hello $WHO"),
        "{plan}"
    );
    let out = run(&["runbook", "run", "greet", "--yes", "--wait", "-p", "WHO=kraal"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}{}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout.contains("finished: 1 ok, 0 failed, 0 skipped"), "{stdout}");
    let log = stdout.rsplit("log ").next().unwrap().trim().to_string();
    let log_text = std::fs::read_to_string(&log).unwrap();
    assert!(
        log_text.contains("param     WHO=kraal") && log_text.contains("hello kraal") && log_text.contains("1 ok"),
        "{log_text}"
    );

    // On a group, one server at a time: the failing one stops the run.
    std::fs::write(
        env.root.join("config/kraal/runbooks/check.toml"),
        "on = \"server\"\nsteps = [\"echo checked $KRAAL_SERVER\"]\n",
    )
    .unwrap();
    // A failing line inside a step fails it, even when later lines succeed.
    std::fs::write(
        env.root.join("config/kraal/runbooks/multi.toml"),
        "steps = [\"false\\necho after-false\", \"echo second-step\"]\n",
    )
    .unwrap();
    let out = run(&["runbook", "run", "multi", "--yes", "--wait"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success() && stdout.contains("finished: 0 ok, 1 failed"), "{stdout}");
    let log = std::fs::read_to_string(stdout.rsplit("log ").next().unwrap().trim()).unwrap();
    let output = log.split("\n--- ").nth(1).unwrap_or_default();
    assert!(output.contains("step 1 failed"), "{log}");
    assert!(!output.contains("after-false") && !output.contains("second-step"), "{log}");

    let out = run(&["runbook", "run", "check", "--yes"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("--on"), "servers must be named");
    let out = run(&["runbook", "run", "check", "--on", "@web", "--yes", "--wait"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "{stdout}");
    assert!(stdout.contains("3 servers (a, bad, c), one at a time, stopping at the first failure"), "{stdout}");
    assert!(stdout.contains("finished: 1 ok, 1 failed, 1 skipped"), "{stdout}");
    let screen = String::from_utf8(run(&["read", "-p", "a"]).stdout).unwrap();
    assert!(screen.contains("checked a"), "{screen}");
    let screen = String::from_utf8(run(&["read", "-p", "c (skipped)"]).stdout).unwrap();
    assert!(screen.contains("skipped: an earlier target failed") && !screen.contains("checked c"), "{screen}");

    // All at once: one fails at once while the others still run. The run must wait for them,
    // not spin (this once overflowed the server's stack).
    std::fs::write(
        env.root.join("config/kraal/runbooks/wide.toml"),
        "on = \"server\"\nrolling = 0\nsteps = [\"sleep 0.5\", \"echo wide $KRAAL_SERVER\"]\n",
    )
    .unwrap();
    let out = run(&["runbook", "run", "wide", "--on", "@web", "--yes", "--wait"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("all at once"), "{stdout}");
    assert!(stdout.contains("finished: 2 ok, 1 failed, 0 skipped"), "{stdout}");
    assert!(run(&["ls"]).status.success(), "the server is still alive");

    // A target over its timeout is stopped, even when it ignores SIGTERM.
    std::fs::write(
        env.root.join("config/kraal/runbooks/slow.toml"),
        "timeout = \"1\"\nsteps = [\"trap '' TERM; sleep 60\"]\n",
    )
    .unwrap();
    let start = Instant::now();
    let out = run(&["runbook", "run", "slow", "--yes", "--wait"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("finished: 0 ok, 1 failed"), "{stdout}");
    assert!(start.elapsed() < Duration::from_secs(20), "stopped by SIGKILL after the grace period");
    let log = std::fs::read_to_string(stdout.rsplit("log ").next().unwrap().trim()).unwrap();
    assert!(log.contains("over its timeout") && log.contains("timed out, stopped"), "{log}");

    // What a timed-out target left running in its process group is stopped too, before the
    // target counts as timed out.
    let pidfile = env.root.join("left.pid");
    std::fs::write(
        env.root.join("config/kraal/runbooks/leaves.toml"),
        format!(
            "timeout = \"1\"\nsteps = [\"(trap '' TERM HUP; exec sleep 60) & echo $! > {}; wait\"]\n",
            pidfile.display()
        ),
    )
    .unwrap();
    let start = Instant::now();
    let out = run(&["runbook", "run", "leaves", "--yes", "--wait"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("finished: 0 ok, 1 failed"), "{stdout}");
    assert!(start.elapsed() < Duration::from_secs(20));
    let left: libc::pid_t = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
    // SAFETY: signal 0 only checks that the process exists.
    assert_ne!(unsafe { libc::kill(left, 0) }, 0, "the background sleep {left} still runs");

    // The log has all of a target's output, not only what its pane still shows.
    std::fs::write(env.root.join("config/kraal/runbooks/long.toml"), "steps = [\"seq 1 3000\"]\n").unwrap();
    let out = run(&["runbook", "run", "long", "--yes", "--wait"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let log = std::fs::read_to_string(stdout.rsplit("log ").next().unwrap().trim()).unwrap();
    assert!(log.contains("\n1\n2\n3\n") && log.contains("\n3000\n"), "{log}");

    // A server that dies during a run: the next one says so in the run's log.
    std::fs::write(env.root.join("config/kraal/runbooks/cut.toml"), "steps = [\"echo cut-started\", \"sleep 60\"]\n")
        .unwrap();
    assert!(run(&["runbook", "run", "cut", "--yes"]).status.success());
    let runs = env.root.join("state/kraal/runs");
    let log = std::fs::read_dir(&runs)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.to_string_lossy().contains("-cut-") && p.extension().is_some_and(|x| x == "log"))
        .expect("the run's log");
    // Its output reaches the disk while it runs.
    let start = Instant::now();
    while !std::fs::read_to_string(log.with_extension("0.out")).unwrap_or_default().contains("cut-started") {
        assert!(start.elapsed() < Duration::from_secs(10), "no output streamed");
        std::thread::sleep(Duration::from_millis(50));
    }
    let pid = env.server_pid().expect("the server's pid");
    // SAFETY: plain syscall.
    unsafe { libc::kill(pid, libc::SIGKILL) };
    wait_gone(&PathBuf::from(format!("/proc/{pid}")));
    let _ = std::fs::remove_file(env.root.join("run/kraal/runbooks.sock"));
    // Killed while it wrote its run ledger, it would leave the temporary file of that write.
    std::fs::write(runs.join(format!("..active-runbooks.json.{pid}.38.tmp")), "[]").unwrap();
    env.run(&["open", "w"]);
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(
        text.contains("cut-started") && text.contains("interrupted") && text.contains("unfinished: this machine"),
        "{text}"
    );
    // The restarted server may be writing its run ledger (a temporary file, then a rename).
    let start = Instant::now();
    loop {
        let left: Vec<_> = std::fs::read_dir(&runs).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name()).collect();
        if left.iter().all(|n| n.to_string_lossy().ends_with(".log")) {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(5), "{left:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// `kraal runbook run` takes for a parameter what the list of runbooks would: one of its
/// choices when it has some, written in its file or listed by its command, and anything
/// otherwise. A refusal names the choices and comes in place of the plan, so `--dry-run` fails
/// too. The command of a repository runbook nobody approved does not run: its plan says which
/// value waits, its run is refused, and once the runbook is approved a run of that plan holds
/// the value to the list.
#[test]
fn a_runbook_value_is_one_its_question_offers() {
    let env = Env::new("rbchoice");
    let (ran, listed) = (env.root.join("ran"), env.root.join("listed"));
    let runbooks = env.root.join("config/kraal/runbooks");
    std::fs::create_dir_all(&runbooks).unwrap();
    let step = format!("steps = [\"echo $V > {}\"]\n", ran.display());
    let write = |name: &str, param: &str| {
        let text = format!("{step}[[param]]\nname = \"V\"\n{param}");
        std::fs::write(runbooks.join(format!("{name}.toml")), text).unwrap();
    };
    write("site", "choices = [\"shop\", \"blog\"]\ndefault = \"shop\"\n");
    write("odd", "choices = [\"shop\", \"blog\"]\ndefault = \"docs\"\n");
    write("tag", &format!("choices_command = \"echo x >> {}; echo v1; echo v2\"\n", listed.display()));
    std::fs::write(
        runbooks.join("flaky.toml"),
        "steps = [\"true\"]\n[[param]]\nname = \"A\"\nchoices_command = \"echo no >&2; exit 3\"\n[[param]]\nname = \"B\"\nchoices_command = \"true\"\n",
    )
    .unwrap();
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    // Whether it succeeded, what it printed, and its error.
    let kraal = |dir: &Path, args: &[&str]| {
        let mut c = env.cmd(&[&["runbook", "run"], args].concat());
        let out = c.current_dir(dir).stdin(std::process::Stdio::null()).output().unwrap();
        let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
        (out.status.success(), text(&out.stdout), text(&out.stderr))
    };

    // Choices written in the file: no plan for another value, and nothing runs.
    let (ok, plan, why) = kraal(&env.root, &["site", "--dry-run", "-p", "V=nope"]);
    assert!(!ok && plan.is_empty(), "{plan}");
    assert!(why.contains("V=nope is not one of its choices: shop, blog"), "{why}");
    let (ok, _, why) = kraal(&env.root, &["site", "--yes", "--wait", "-p", "V=nope"]);
    assert!(!ok && why.contains("V=nope is not one of its choices") && !ran.exists(), "{why}");
    for (args, value) in [(&["-p", "V=blog"][..], "V=blog"), (&[], "V=shop")] {
        let (ok, plan, why) = kraal(&env.root, &[&["site", "--dry-run"], args].concat());
        assert!(ok && plan.contains(value), "{args:?}: {plan}{why}");
    }
    // A default is a value like another.
    let (ok, _, why) = kraal(&env.root, &["odd", "--dry-run"]);
    assert!(!ok && why.contains("V=docs is not one of its choices: shop, blog"), "{why}");

    // Choices listed by a command, once for a plan: its run does not list them again.
    let (ok, _, why) = kraal(&env.root, &["tag", "--dry-run", "-p", "V=v3"]);
    assert!(!ok && why.contains("V=v3 is not one of its choices: v1, v2"), "{why}");
    let (ok, said, why) = kraal(&env.root, &["tag", "--yes", "--wait", "-p", "V=v2"]);
    assert!(ok && said.contains("finished: 1 ok"), "{said}{why}");
    assert_eq!(std::fs::read_to_string(&ran).unwrap(), "v2\n");
    assert_eq!(std::fs::read_to_string(&listed).unwrap(), "x\nx\n");
    // A command that fails, or lists nothing, leaves the question a field: the plan says which
    // value was not checked.
    let (ok, plan, why) = kraal(&env.root, &["flaky", "--dry-run", "-p", "A=any", "-p", "B=thing"]);
    assert!(ok, "{why}");
    assert!(plan.contains("A is taken as given: \"echo no >&2; exit 3\" failed: no"), "{plan}");
    assert!(!plan.contains("B is taken"), "{plan}");

    // A repository's runbook, not approved: its command does not run. The plan says so, and a
    // run is refused for the approval it lacks.
    let repo = env.root.join("app");
    let deploy =
        format!("{step}[[param]]\nname = \"V\"\nchoices_command = \"echo x >> {}; echo v1\"\n", listed.display());
    let _ = git_repo(&repo, &[(".kraal/runbooks/deploy.toml", &deploy)]);
    let file = repo.join(".kraal/runbooks/deploy.toml");
    for left in [&ran, &listed] {
        std::fs::remove_file(left).unwrap();
    }
    // What any program can send: a plan asked for, then run by its token.
    let plan = |value: &str| {
        let params = serde_json::json!([["V", value]]);
        let request =
            serde_json::json!({ "cmd": "runbook_plan", "path": file, "target": null, "params": params, "pane": null });
        let plan = raw_request(&env, request);
        plan["Ok"]["data"]["token"].as_str().unwrap_or_else(|| panic!("no plan: {plan}")).to_string()
    };
    let run = |token: &str| raw_request(&env, serde_json::json!({ "cmd": "runbook_run", "token": token }));
    let (ok, shown, why) = kraal(&repo, &["deploy", "--dry-run", "-p", "V=nope"]);
    assert!(ok, "{why}");
    assert!(shown.contains("V is taken as given: its choices are listed once the runbook is approved"), "{shown}");
    let answer = run(&plan("v1"));
    assert!(answer["Err"].as_str().is_some_and(|e| e.contains("is not approved")), "{answer}");
    let (other, known) = (plan("nope"), plan("v1"));
    assert!(!listed.exists() && !ran.exists());
    // A refusal quotes the choices its file gives: a terminal acts on none of it.
    let erasing = "steps = [\"true\"]\n[[param]]\nname = \"V\"\nchoices = [\"a\\u001b[2J\"]\n";
    std::fs::write(repo.join(".kraal/runbooks/pick.toml"), erasing).unwrap();
    let (ok, _, why) = kraal(&repo, &["pick", "--dry-run", "-p", "V=b"]);
    assert!(!ok && why.contains("V=b is not one of its choices: a\\u{1b}[2J") && !why.contains('\x1b'), "{why:?}");

    // Approved: its list holds from now on, for the plans made before too.
    let trust = env.cmd(&["runbook", "trust", "deploy"]).current_dir(&repo).output().unwrap();
    assert!(trust.status.success(), "{}", String::from_utf8_lossy(&trust.stderr));
    let (ok, _, why) = kraal(&repo, &["deploy", "--dry-run", "-p", "V=nope"]);
    assert!(!ok && why.contains("V=nope is not one of its choices: v1"), "{why}");
    let answer = run(&other);
    assert!(answer["Err"].as_str().is_some_and(|e| e.contains("V=nope is not one of its choices: v1")), "{answer}");
    assert!(!ran.exists());
    let answer = run(&known);
    assert!(answer["Ok"].is_object(), "{answer}");
    wait_until(Duration::from_secs(10), "the plan made before the approval never ran", || {
        std::fs::read_to_string(&ran).is_ok_and(|said| said == "v1\n")
    });
    assert_eq!(std::fs::read_to_string(&listed).unwrap(), "x\nx\nx\n");
}

/// Keys typed while the runbooks are being listed go to the list, not to the shell under it.
#[cfg(target_os = "linux")]
#[test]
fn keys_typed_while_runbooks_load_stay_in_the_list() {
    let env = Env::new("rbkeys");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    std::fs::create_dir_all(env.root.join("config/kraal/runbooks")).unwrap();
    std::fs::write(env.root.join("config/kraal/runbooks/hello.toml"), "steps = [\"echo hello\"]\n").unwrap();
    env.run(&["open", "w"]);
    let mut client = env.client();
    // ctrl+b R b opens the runbooks, and the rest comes in the same breath.
    std::io::Write::write_all(client.stdin.as_mut().unwrap(), b"\x02Rbecho leaked\r").unwrap();
    std::thread::sleep(Duration::from_millis(800));
    std::io::Write::write_all(client.stdin.as_mut().unwrap(), b"\x1b").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let screen = env.run(&["read", "-p", "shell"]);
    assert!(!screen.contains("leaked"), "typed into the shell: {screen}");
    let _ = client.kill();
    let _ = client.wait();
}

/// The session's answer to `request`, sent through its socket as written: what a kraal of
/// another release, or any other program, can send.
fn raw_request(env: &Env, request: serde_json::Value) -> serde_json::Value {
    use std::io::Read;
    let socket = env.root.join("run/kraal").join(format!("{}.sock", env.session));
    let mut stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let message = serde_json::json!({ "type": "request", "version": 2, "request": request }).to_string();
    raw_frame(&mut stream, 0, message.as_bytes());
    let mut header = [0u8; 5];
    stream.read_exact(&mut header).unwrap();
    let mut payload = vec![0u8; u32::from_le_bytes(header[..4].try_into().unwrap()) as usize];
    stream.read_exact(&mut payload).unwrap();
    serde_json::from_slice::<serde_json::Value>(&payload).unwrap()["result"].take()
}

/// A repository's runbook is a file someone else wrote: nothing of it runs before this version
/// of it is approved, and approving is an act of its own. Listing it, showing it, planning it
/// and `--yes` (which answers the plan) approve nothing; `kraal runbook trust` does, for the
/// file as it is.
#[test]
fn a_repository_runbook_runs_only_once_approved() {
    let env = Env::new("rbtrust");
    let repo = env.root.join("app");
    let (ran, listed) = (env.root.join("ran"), env.root.join("listed"));
    let file = repo.join(".kraal/runbooks/deploy.toml");
    let version = |word: &str| {
        format!(
            "description = \"deploy\"\nsteps = [\"echo {word} > {}\"]\n[[param]]\nname = \"V\"\ndefault = \"1\"\nchoices_command = \"touch {}\"\n",
            ran.display(),
            listed.display()
        )
    };
    let _ = git_repo(&repo, &[(".kraal/runbooks/deploy.toml", &version("one"))]);
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let kraal = |args: &[&str]| {
        let mut c = env.cmd(&[&["runbook"], args].concat());
        c.current_dir(&repo).stdin(std::process::Stdio::null()).output().unwrap()
    };
    let text =
        |out: &Output| format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let store = env.root.join("state/kraal/trusted-runbooks.json");
    // What a program other than this kraal's command line can do: ask for a plan, then run it.
    let plan = || {
        let request =
            serde_json::json!({ "cmd": "runbook_plan", "path": file, "target": null, "params": [], "pane": null });
        let plan = raw_request(&env, request);
        plan["Ok"]["data"]["token"].as_str().unwrap_or_else(|| panic!("no plan: {plan}")).to_string()
    };
    let run_plan = |token: &str| raw_request(&env, serde_json::json!({ "cmd": "runbook_run", "token": token }));
    let refused = |answer: &serde_json::Value| answer["Err"].as_str().is_some_and(|e| e.contains("is not approved"));

    // Reading it runs nothing and approves nothing.
    for args in [&["list"][..], &["list", "--json"], &["show", "deploy"], &["run", "deploy", "--dry-run"]] {
        let out = kraal(args);
        assert!(out.status.success() && text(&out).contains("deploy"), "{args:?}: {}", text(&out));
        assert!(!ran.exists() && !listed.exists() && !store.exists(), "{args:?}");
    }
    // `--yes` answers the plan, and without a terminal nobody answers: neither approves.
    for args in [&["run", "deploy", "--yes", "--wait"][..], &["run", "deploy"]] {
        let out = kraal(args);
        let said = text(&out);
        assert!(!out.status.success(), "{args:?}: {said}");
        assert!(said.contains("is not approved") && said.contains("`kraal runbook trust deploy`"), "{args:?}: {said}");
        assert!(!ran.exists() && !listed.exists() && !store.exists(), "{args:?}");
    }
    let answer = run_plan(&plan());
    assert!(refused(&answer), "{answer}");
    assert!(!ran.exists() && !store.exists());

    // Approved as it is now: it runs without a question, and so does a plan made before.
    let before = plan();
    let out = kraal(&["trust", "deploy"]);
    assert!(out.status.success() && text(&out).contains("is approved as it is now"), "{}", text(&out));
    let out = kraal(&["run", "deploy", "--yes", "--wait"]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(std::fs::read_to_string(&ran).unwrap(), "one\n");
    assert!(listed.exists(), "approved, it lists the choices its value is held to");
    std::fs::remove_file(&ran).unwrap();
    let answer = run_plan(&before);
    assert!(answer["Ok"].is_object(), "{answer}");
    wait_until(Duration::from_secs(10), "the plan made before the approval never ran", || {
        std::fs::read_to_string(&ran).is_ok_and(|t| t == "one\n")
    });

    // Changed since: that version was never approved, and lists no more than it runs.
    std::fs::write(&file, version("two")).unwrap();
    std::fs::remove_file(&listed).unwrap();
    let out = kraal(&["run", "deploy", "--yes", "--wait"]);
    assert!(!out.status.success() && text(&out).contains("`kraal runbook trust deploy`"), "{}", text(&out));
    let answer = run_plan(&plan());
    assert!(refused(&answer), "{answer}");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(std::fs::read_to_string(&ran).unwrap(), "one\n");
    assert!(!listed.exists());
    let out = kraal(&["trust", "deploy"]);
    assert!(out.status.success(), "{}", text(&out));
    let out = kraal(&["run", "deploy", "--yes", "--wait"]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(std::fs::read_to_string(&ran).unwrap(), "two\n");

    // What these commands print of a repository's files, a terminal acts on none of it: an
    // escape sequence in a description, in a step, or in a file that cannot be read.
    let odd = "description = \"clear \\u001b[2J\"\nsteps = [\"echo \\u001b[2K\"]\n";
    std::fs::write(repo.join(".kraal/runbooks/odd.toml"), odd).unwrap();
    std::fs::write(repo.join(".kraal/runbooks/bad.toml"), "steps = \x1b[2J\n").unwrap();
    let said = text(&kraal(&["list"]));
    assert!(said.contains("clear \\u{1b}[2J") && said.contains("steps = \\u{1b}[2J"), "{said:?}");
    assert!(!said.contains('\x1b'), "{said:?}");
    let said = text(&kraal(&["run", "odd", "--dry-run"]));
    assert!(said.contains("1. echo \\u{1b}[2K") && !said.contains('\x1b'), "{said:?}");
    assert!(!text(&kraal(&["show", "odd"])).contains('\x1b'));

    // The user's own runbooks need no approval.
    std::fs::create_dir_all(env.root.join("config/kraal/runbooks")).unwrap();
    std::fs::write(env.root.join("config/kraal/runbooks/mine.toml"), "steps = [\"true\"]\n").unwrap();
    let out = kraal(&["trust", "mine"]);
    assert!(out.status.success() && text(&out).contains("needs no approval"), "{}", text(&out));
    let out = kraal(&["run", "mine", "--yes", "--wait"]);
    assert!(out.status.success(), "{}", text(&out));
}

/// An approval is of a file in its repository, however the way there is spelled: through a link
/// to the checkout or not (`/tmp` is one on macOS), on the command line or in a plan another
/// program asks for, and in a store written by 0.3.0 under the spelling used then. A link that
/// comes to lead to another repository carries no approval there.
#[test]
fn a_runbook_approval_does_not_depend_on_how_its_repository_is_reached() {
    let env = Env::new("rblink");
    let (ran, link) = (env.root.join("ran"), env.root.join("link"));
    let runbook = format!("steps = [\"echo ran >> {}\"]\n", ran.display());
    let repository = |dir: &str| {
        let repo = env.root.join(dir).join("app");
        let _ = git_repo(&repo, &[(".kraal/runbooks/deploy.toml", &runbook)]);
        repo
    };
    let (first, second) = (repository("first"), repository("second"));
    std::os::unix::fs::symlink(env.root.join("first"), &link).unwrap();
    // The file where its repository really is, and as a path through the link spells it.
    let file = |repo: &Path| repo.join(".kraal/runbooks/deploy.toml").display().to_string();
    let (real, spelled) = (file(&first.canonicalize().unwrap()), file(&link.join("app")));
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let kraal = |dir: &Path, args: &[&str]| {
        let mut c = env.cmd(&[&["runbook"], args].concat());
        let out = c.current_dir(dir).stdin(std::process::Stdio::null()).output().unwrap();
        (
            out.status.success(),
            format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)),
        )
    };
    // A plan of the file at `path`, asked for and run as another program would.
    let run_plan = |path: &str| {
        let request =
            serde_json::json!({ "cmd": "runbook_plan", "path": path, "target": null, "params": [], "pane": null });
        let plan = raw_request(&env, request);
        let token = plan["Ok"]["data"]["token"].as_str().unwrap_or_else(|| panic!("no plan: {plan}")).to_string();
        raw_request(&env, serde_json::json!({ "cmd": "runbook_run", "token": token }))
    };
    let store = env.root.join("state/kraal/trusted-runbooks.json");
    let approved = || -> Vec<String> {
        let store: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&store).unwrap()).unwrap();
        store.as_object().unwrap().keys().cloned().collect()
    };

    // Approved in the checkout: approved through the link too, and recorded once.
    let (ok, said) = kraal(&first, &["trust", "deploy"]);
    assert!(ok, "{said}");
    assert_eq!(approved(), std::slice::from_ref(&real));
    let answer = run_plan(&spelled);
    assert!(answer["Ok"].is_object(), "{answer}");
    let (ok, said) = kraal(&link.join("app"), &["run", "deploy", "--yes", "--wait"]);
    assert!(ok, "{said}");

    // A store written by 0.3.0 under the spelling through the link still approves the file; the
    // next approval leaves the file one entry, under its real place.
    let mut written = serde_json::Map::new();
    written.insert(spelled.clone(), serde_json::Value::String(runbook.clone()));
    std::fs::write(&store, serde_json::Value::Object(written).to_string()).unwrap();
    let (ok, said) = kraal(&first, &["run", "deploy", "--yes", "--wait"]);
    assert!(ok, "{said}");
    let answer = run_plan(&real);
    assert!(answer["Ok"].is_object(), "{answer}");
    let (ok, said) = kraal(&first, &["trust", "deploy"]);
    assert!(ok, "{said}");
    assert_eq!(approved(), std::slice::from_ref(&real));

    // The link comes to lead to another repository, with the same file: nothing is approved
    // there.
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(second.parent().unwrap(), &link).unwrap();
    let answer = run_plan(&spelled);
    assert!(answer["Err"].as_str().is_some_and(|e| e.contains("is not approved")), "{answer}");
    let (ok, said) = kraal(&link.join("app"), &["run", "deploy", "--yes", "--wait"]);
    assert!(!ok && said.contains("is not approved"), "{said}");
}

/// In a terminal, `kraal runbook run` asks before a repository runbook nobody approved: the
/// question shows the file, with what a terminal would hide written out, names the approval,
/// and comes before the plan's.
#[cfg(target_os = "linux")]
#[test]
fn a_repository_runbook_is_approved_by_a_question_of_its_own() {
    let env = Env::new("rbask");
    let repo = env.root.join("app");
    let ran = env.root.join("ran");
    // A zero-width space, which a terminal shows nothing of.
    let runbook = format!("steps = [\"echo ran > {} #\u{200b}\"]\n", ran.display());
    let _ = git_repo(&repo, &[(".kraal/runbooks/deploy.toml", &runbook)]);
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let store = env.root.join("state/kraal/trusted-runbooks.json");
    let run = |input: &[u8]| {
        let mut c = env.cmd(&["runbook", "run", "deploy", "--wait"]);
        c.current_dir(&repo);
        in_terminal(c, input)
    };

    let shown = run(b"n\n");
    assert!(shown.contains("Trust this version?") && shown.contains("#\\u{200b}"), "{shown}");
    assert!(!shown.contains("Run it?"), "{shown}");
    assert!(!ran.exists() && !store.exists());
    // Approved, then its plan declined: nothing runs, and it stays approved.
    let shown = run(b"y\nn\n");
    assert!(shown.contains("Trust this version?") && shown.contains("Run it?"), "{shown}");
    assert!(!ran.exists() && store.exists());
    let shown = run(b"y\n");
    assert!(!shown.contains("Trust this version?") && shown.contains("finished: 1 ok"), "{shown}");
    assert!(ran.exists());
}

/// From the list of runbooks, a repository runbook asks to be trusted before anything of it
/// runs, its `choices_command` included; a no leaves it as it was, and a change asks again. A
/// Markdown one asks before its first block is typed.
#[test]
fn the_runbook_list_asks_before_a_repository_runbook_does_anything() {
    let env = Env::new("rbpick");
    let repo = env.root.join("app");
    let (ran, listed) = (env.root.join("ran"), env.root.join("listed"));
    let file = repo.join(".kraal/runbooks/deploy.toml");
    let version = |word: &str| {
        format!(
            "steps = [\"echo {word} $V > {}\"]\n[[param]]\nname = \"V\"\nchoices_command = \"touch {}; echo first\"\n",
            ran.display(),
            listed.display()
        )
    };
    let _ = git_repo(
        &repo,
        &[
            (".kraal/runbooks/deploy.toml", &version("one")),
            (".kraal/runbooks/notes.md", "# Notes\n\n```sh\necho from-\"notes\"\n```\n"),
        ],
    );
    env.workspace("w", &format!("layout {{\n    pane name=\"shell\" cwd={:?}\n}}\n", repo.display().to_string()));
    env.run(&["open", "w"]);
    let store = env.root.join("state/kraal/trusted-runbooks.json");
    let mut client = RawClient::attach(&env, "w");
    // The list has the repository's runbooks once git has seen it.
    client.wait_for_screen("⎇ main");
    let pick = |client: &mut RawClient, name: &str| {
        env.run(&["focus", "shell"]);
        client.send(b"\x02Rb");
        client.wait_for_row("Runbooks");
        client.send(name.as_bytes());
        client.wait_for_row("repository app");
        client.send(b"\r");
    };
    let gone = |client: &RawClient, text: &str| {
        client.wait_for_rows(&format!("without {text:?}"), |rows| !rows.iter().any(|r| r.contains(text)));
    };

    pick(&mut client, "deploy");
    client.wait_for_row("Trust the runbook deploy");
    std::thread::sleep(Duration::from_millis(300));
    assert!(!listed.exists() && !ran.exists() && !store.exists(), "something ran before the answer");
    client.send(b"n");
    gone(&client, "Trust the runbook deploy");
    std::thread::sleep(Duration::from_millis(300));
    assert!(!listed.exists() && !ran.exists() && !store.exists(), "a no approved or ran something");

    // Approved: its choices are listed, then its plan is shown.
    pick(&mut client, "deploy");
    client.wait_for_row("Trust the runbook deploy");
    client.send(b"y");
    gone(&client, "Trust the runbook deploy");
    client.wait_for_row("first");
    assert!(listed.exists() && !ran.exists());
    client.send(b"\r");
    client.wait_for_row("Run deploy on this machine?");
    client.send(b"y");
    wait_until(Duration::from_secs(10), "the approved runbook never ran", || {
        std::fs::read_to_string(&ran).is_ok_and(|t| t == "one first\n")
    });

    // Changed since: asked again, before its choices are listed.
    std::fs::remove_file(&listed).unwrap();
    std::fs::write(&file, version("two")).unwrap();
    pick(&mut client, "deploy");
    client.wait_for_row("Trust the runbook deploy");
    std::thread::sleep(Duration::from_millis(300));
    assert!(!listed.exists());
    client.send(b"n");
    gone(&client, "Trust the runbook deploy");

    // A document: its first block waits for the answer; approved from the command line, the
    // block is typed without a question.
    pick(&mut client, "notes");
    client.wait_for_row("1. Notes");
    client.send(b"\r");
    client.wait_for_row("From the repository");
    client.send(b"n");
    gone(&client, "From the repository");
    assert!(!env.run(&["read", "-p", "shell"]).contains("from-notes"));
    let out = env.cmd(&["runbook", "trust", "notes"]).current_dir(&repo).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    env.run(&["focus", "shell"]);
    client.send(b"\x02Rb");
    client.wait_for_row("1. Notes");
    client.send(b"\r");
    env.wait_for_text("shell", "from-notes");
}

/// A step as a question quotes it to be approved: under its number, each line as it is written.
fn numbered(number: usize, step: &str) -> Vec<String> {
    let label = format!("  {number}. ");
    let under = " ".repeat(label.len());
    step.lines().enumerate().map(|(i, line)| format!("{}{line}", if i == 0 { &label } else { &under })).collect()
}

/// Lines as [`RawClient::read_question`] gives them: a terminal's row does not tell the spaces
/// that end a line from the room after it.
fn without_trailing_spaces(lines: Vec<String>) -> Vec<String> {
    lines.iter().map(|line| line.trim_end().to_string()).collect()
}

/// The question that asks to trust a repository's runbook shows all that a yes lets run,
/// whatever its length and the terminal's size: every line of every step with its spacing, a
/// long line whole, what a terminal would hide written out, the shell, and the commands that
/// list choices. Reading it runs and approves nothing, and Enter cancels. A document's shows the
/// step in full, then its other steps.
#[test]
fn all_that_a_repository_runbook_runs_can_be_read_before_it_is_trusted() {
    let env = Env::new("rbread");
    let repo = env.root.join("app");
    let (ran, listed) = (env.root.join("ran"), env.root.join("listed"));
    let steps: Vec<String> = (1..=40)
        .map(|i| match i {
            7 => "echo safe \u{1b}[2K\u{200b}".to_string(),
            40 => format!("echo {} > {}", "0123456789".repeat(20), ran.display()),
            _ => format!("if test -n \"$V\"; then\n    echo  step {i}\n\nfi"),
        })
        .collect();
    let choices = format!("touch {}\n  echo first", listed.display());
    let quoted: Vec<String> = steps.iter().map(|step| serde_json::json!(step).to_string()).collect();
    let deploy = format!(
        "shell = \"bash\"\nsteps = [{}]\n[[param]]\nname = \"V\"\nchoices_command = {}\n",
        quoted.join(", "),
        serde_json::json!(choices)
    );
    let build = "for f in a b; do\n    echo  from-\"notes\" \"$f\"\n\ndone\n".to_string() + &"echo more\n".repeat(8);
    let notes = format!(
        "# Notes\n\n## Build\n\n```sh\n{build}```\n\n## Deploy\n\n```sh\necho one\n  echo two\n```\n\n\
         ## Check\n\n```console\n$ echo three\nthree\n```\n"
    );
    let _ = git_repo(&repo, &[(".kraal/runbooks/deploy.toml", &deploy), (".kraal/runbooks/notes.md", &notes)]);
    env.workspace("w", &format!("layout {{\n    pane name=\"shell\" cwd={:?}\n}}\n", repo.display().to_string()));
    env.run(&["open", "w"]);
    let store = env.root.join("state/kraal/trusted-runbooks.json");
    let pick = |client: &mut RawClient, name: &str| {
        env.run(&["focus", "shell"]);
        client.send(b"\x02Rb");
        client.wait_for_row("Runbooks");
        client.send(name.as_bytes());
        client.wait_for_row("repository app");
        client.send(b"\r");
    };
    let gone = |client: &RawClient, text: &str| {
        client.wait_for_rows(&format!("without {text:?}"), |rows| !rows.iter().any(|r| r.contains(text)));
    };
    // A step as the question quotes it: under its number, each line where it is in the file.
    let mut expected: Vec<String> = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        expected.extend(numbered(i + 1, &step.replace('\u{1b}', "\\u{1b}").replace('\u{200b}', "\\u{200b}")));
    }
    expected.push("To list the choices of V, it runs:".to_string());
    expected.extend(choices.lines().map(|line| format!("     {line}")));
    let expected = without_trailing_spaces(expected);
    // The sidebar is there from 84 columns on; a page key moves by the rows the box has.
    for (size, key) in [((124, 36), &b"\x1b[6~"[..]), ((80, 24), b" "), ((60, 12), b"\x1b[6~")] {
        let mut client = RawClient::attach_sized(&env, Some("w"), size);
        if size.0 == 124 {
            // The list has the repository's runbooks once git has seen it.
            client.wait_for_screen("⎇ main");
        }
        pick(&mut client, "deploy");
        client.wait_for_row("Trust the runbook deploy");
        let lines = client.read_question(key);
        let first = lines.iter().position(|line| line.starts_with("  1. ")).unwrap();
        assert_eq!(lines[first..], expected[..], "at {size:?}");
        let said = lines[..first].join(" ");
        assert!(said.contains("It runs these steps with bash:"), "at {size:?}: {said}");
        assert!(!listed.exists() && !ran.exists() && !store.exists(), "reading it ran or approved something");
        // Cancel is the button selected.
        client.send(b"\r");
        gone(&client, "Trust the runbook deploy");
        std::thread::sleep(Duration::from_millis(300));
        assert!(!listed.exists() && !store.exists(), "Enter approved it");
    }

    // A document: the step that would be typed, whole, then the others a yes approves with it.
    let mut client = RawClient::attach_sized(&env, Some("w"), (80, 24));
    pick(&mut client, "notes");
    client.wait_for_row("1. Build");
    client.send(b"\r");
    client.wait_for_row("Run step 1 in");
    let lines = client.read_question(b"j");
    let mut expected: Vec<String> = build.lines().map(|line| format!("  {line}")).collect();
    expected.push("Its other steps:".to_string());
    expected.extend(numbered(2, "echo one\n  echo two"));
    expected.extend(numbered(3, "echo three"));
    let first = lines.iter().position(|line| line.starts_with("  for f in")).unwrap();
    assert_eq!(lines[first..], without_trailing_spaces(expected)[..]);
    let said = lines[..first].join(" ");
    assert!(said.contains("A yes approves it as it is now, with all its steps."), "{said}");
    client.send(b"\r");
    gone(&client, "Run step 1 in");
    std::thread::sleep(Duration::from_millis(300));
    assert!(!env.run(&["read", "-p", "shell"]).contains("from-notes") && !store.exists());
}

/// A timed-out target is stopped to its last process, by its process group: a command started
/// again in its pane meanwhile is not touched, and closing its pane does not let what it left
/// behind escape.
#[cfg(target_os = "linux")]
#[test]
fn a_timed_out_target_is_stopped_but_not_what_replaced_it() {
    let env = Env::new("tostop");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    let alive = |pid: libc::pid_t| {
        // A zombie has no command line: it is gone for what matters here.
        std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| !c.is_empty())
    };
    let read_pid = |path: &Path| std::fs::read_to_string(path).ok().and_then(|t| t.trim().parse::<libc::pid_t>().ok());
    let wait_until = |what: &str, mut ok: Box<dyn FnMut() -> bool + '_>| {
        let start = Instant::now();
        while !ok() {
            assert!(start.elapsed() < Duration::from_secs(20), "{what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let pane_of = |tab: &str| -> String {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let p = panes.as_array().unwrap().iter().find(|p| p["tab"] == tab).expect("the run's pane");
        p["id"].to_string()
    };
    let log_of = |name: &str| -> String {
        std::fs::read_dir(env.root.join("state/kraal/runs"))
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| p.to_string_lossy().contains(&format!("-{name}-")) && p.extension().is_some_and(|x| x == "log"))
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default()
    };
    // Each start leaves a process that ignores SIGTERM and SIGHUP in the group, and says its pid.
    let step = |pidfile: &Path| {
        format!(
            "timeout = \"1\"\nsteps = [\"(trap '' TERM HUP; exec sleep 60) & echo $! > {}; wait\"]\n",
            pidfile.display()
        )
    };

    std::fs::create_dir_all(env.root.join("config/kraal/runbooks")).unwrap();
    // Started again by hand while it was being stopped: the new command is not stopped. Enter
    // in an ended pane starts it again; it takes a client, in a terminal `script` provides.
    let mut client = env.client();
    let first = env.root.join("first.pid");
    std::fs::write(env.root.join("config/kraal/runbooks/again.toml"), step(&first)).unwrap();
    env.run(&["runbook", "run", "again", "--yes"]);
    wait_until("the timeout", Box::new(|| log_of("again").contains("over its timeout")));
    let old = read_pid(&first).expect("the first start's pid");
    let pane = pane_of("▶ again");
    wait_until(
        "the command ends on SIGTERM",
        Box::new(|| {
            let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
            let id: u64 = pane.parse().unwrap();
            panes.as_array().unwrap().iter().any(|p| p["id"].as_u64() == Some(id) && !p["exit_code"].is_null())
        }),
    );
    std::fs::remove_file(&first).unwrap();
    std::io::Write::write_all(client.stdin.as_mut().unwrap(), b"\r").unwrap();
    wait_until("the second start", Box::new(|| read_pid(&first).is_some()));
    let new = read_pid(&first).unwrap();
    assert_ne!(old, new);
    wait_until("the run ends", Box::new(|| log_of("again").contains("timed out, stopped")));
    assert!(!alive(old), "what the timed-out start left ({old}) still runs");
    assert!(alive(new), "the start made by hand ({new}) was stopped");
    // SAFETY: plain syscall on our own test process.
    unsafe { libc::kill(new, libc::SIGKILL) };
    let _ = client.kill();
    let _ = client.wait();

    // Its pane closed while it is being stopped: what it left is stopped all the same.
    let second = env.root.join("second.pid");
    std::fs::write(env.root.join("config/kraal/runbooks/closed.toml"), step(&second)).unwrap();
    env.run(&["runbook", "run", "closed", "--yes"]);
    wait_until("the timeout", Box::new(|| log_of("closed").contains("over its timeout")));
    let left = read_pid(&second).expect("the start's pid");
    env.run(&["close", &pane_of("▶ closed")]);
    wait_until("what it left is stopped", Box::new(|| !alive(left)));
}

/// What a timed-out target left behind is stopped even when kraal stops meanwhile (its last
/// pane closed during the grace period), and even when the server was held up past that period.
#[cfg(target_os = "linux")]
#[test]
fn a_timed_out_target_is_stopped_whatever_happens_to_kraal_meanwhile() {
    let alive = |pid: libc::pid_t| std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| !c.is_empty());
    let read_pid = |path: &Path| std::fs::read_to_string(path).ok().and_then(|t| t.trim().parse::<libc::pid_t>().ok());
    let wait_until = |what: &str, limit: u64, mut ok: Box<dyn FnMut() -> bool + '_>| {
        let start = Instant::now();
        while !ok() {
            assert!(start.elapsed() < Duration::from_secs(limit), "{what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let log_of = |env: &Env, name: &str| -> String {
        std::fs::read_dir(env.root.join("state/kraal/runs"))
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| p.to_string_lossy().contains(&format!("-{name}-")) && p.extension().is_some_and(|x| x == "log"))
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default()
    };
    let start_run = |env: &Env, name: &str| -> libc::pid_t {
        env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
        env.run(&["open", "w"]);
        std::fs::create_dir_all(env.root.join("config/kraal/runbooks")).unwrap();
        let pidfile = env.root.join(format!("{name}.pid"));
        std::fs::write(
            env.root.join(format!("config/kraal/runbooks/{name}.toml")),
            format!(
                "timeout = \"1\"\nsteps = [\"(trap '' TERM HUP; exec sleep 60) & echo $! > {}; wait\"]\n",
                pidfile.display()
            ),
        )
        .unwrap();
        env.run(&["runbook", "run", name, "--yes"]);
        wait_until("the timeout", 20, Box::new(|| log_of(env, name).contains("over its timeout")));
        read_pid(&pidfile).expect("the pid of what the target left")
    };

    // Closing its last pane keeps the session while the timeout's grace period finishes.
    let env = Env::new("tolast");
    let left = start_run(&env, "last");
    env.run(&["close", "shell"]);
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let id = panes.as_array().unwrap()[0]["id"].to_string();
    let _ = env.try_run(&["close", &id]);
    assert!(env.server_pid().is_some());
    wait_until("what the target left is stopped", 20, Box::new(|| !alive(left)));

    // The server held up past the grace period: SIGKILL still goes before it gives up.
    let env = Env::new("toheld");
    let left = start_run(&env, "held");
    let server = env.server_pid().expect("the server");
    // SAFETY: plain syscalls on the test's own server.
    unsafe { libc::kill(server, libc::SIGSTOP) };
    std::thread::sleep(Duration::from_secs(16));
    unsafe { libc::kill(server, libc::SIGCONT) };
    wait_until("what the target left is stopped", 5, Box::new(|| !alive(left)));
    wait_until("the run ends", 20, Box::new(|| log_of(&env, "held").contains("timed out")));
    assert!(!log_of(&env, "held").contains("do not end"), "{}", log_of(&env, "held"));
    // SAFETY: as above.
    unsafe { libc::kill(left, libc::SIGKILL) };
}

/// A tool that acts goes by what the pane's kubeconfig says when it starts: switched to
/// production a moment before (`kubectx`), it asks first.
#[cfg(target_os = "linux")]
#[test]
fn a_tool_asks_when_the_kubeconfig_just_switched_to_production() {
    let env = Env::new("kubenow");
    let kube = env.root.join("kube.yaml");
    let config = |current: &str| {
        format!(
            "apiVersion: v1\nkind: Config\ncurrent-context: {current}\ncontexts:\n\
             - name: dev\n  context: {{cluster: c}}\n- name: prod-eks\n  context: {{cluster: c}}\n"
        )
    };
    std::fs::write(&kube, config("dev")).unwrap();
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(&bin.join("kubectl"), "#!/bin/sh\nsleep 1000\n");
    let marked = env.root.join("marked");
    let cfg = env.root.join("config/kraal/config.toml");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str(&format!("\n[tools.mark]\ncommand = \"touch {}\"\nkey = \"m\"\n", marked.display()));
    std::fs::write(&cfg, text).unwrap();
    // A pane where kubectl runs on that kubeconfig.
    env.workspace(
        "w",
        &format!(
            "layout {{\n    pane name=\"k\" command=\"env\" {{ args \"KUBECONFIG={}\" \"{}\" \"get\" \"pods\"; }}\n}}\n",
            kube.display(),
            bin.join("kubectl").display()
        ),
    );
    env.run(&["open", "w"]);
    let start = Instant::now();
    loop {
        let panes = env.run(&["panes", "--json"]);
        if panes.contains("dev") {
            break;
        }
        assert!(start.elapsed() < PATIENCE, "the pane's context is read: {panes}");
        std::thread::sleep(Duration::from_millis(100));
    }
    let mut client = env.client();
    // Switched to production, then the tool at once: before the next look at the file.
    std::fs::write(&kube, config("prod-eks")).unwrap();
    std::io::Write::write_all(client.stdin.as_mut().unwrap(), b"\x02Rm").unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    assert!(!marked.exists(), "the tool ran on production without asking");
    // It asked: yes runs it.
    std::io::Write::write_all(client.stdin.as_mut().unwrap(), b"y").unwrap();
    let start = Instant::now();
    while !marked.exists() {
        assert!(start.elapsed() < Duration::from_secs(10), "the tool runs once allowed");
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = client.kill();
    let _ = client.wait();
}

/// A pane that runs kubectl on `kube` for 2 s, then is a shell: its kubeconfig is known, and
/// what is typed there shows.
#[cfg(target_os = "linux")]
fn kube_shell_pane(env: &Env, name: &str, kube: &Path) -> String {
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(&bin.join("kubectl"), "#!/bin/sh\nsleep 2\nexport PS1='$ '\nexec sh\n");
    format!(
        "pane name=\"{name}\" command=\"env\" {{ args \"KUBECONFIG={}\" \"{}\" \"get\" \"pods\"; }}",
        kube.display(),
        bin.join("kubectl").display()
    )
}

#[cfg(target_os = "linux")]
fn kubeconfig_text(current: &str) -> String {
    format!(
        "apiVersion: v1\nkind: Config\ncurrent-context: {current}\ncontexts:\n\
         - name: dev\n  context: {{cluster: c}}\n- name: prod-x\n  context: {{cluster: c}}\n"
    )
}

/// Wait until the panes say a context: the pane's kubeconfig was read.
#[cfg(target_os = "linux")]
fn wait_for_context(env: &Env, context: &str) {
    let start = Instant::now();
    loop {
        let panes = env.run(&["panes", "--json"]);
        if panes.contains(context) {
            return;
        }
        assert!(start.elapsed() < PATIENCE, "the pane's context is read: {panes}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Wait until the server reads `file` again, a kubeconfig that was just changed and whose reads
/// `<file>.slow` makes last: the next scan starts that read (within 3.4 s, later on a loaded
/// machine), and what is typed from here on finds it under way.
#[cfg(target_os = "linux")]
fn wait_for_slow_read(file: &Path) {
    let reading = slow_read_marker(file);
    wait_until(PATIENCE, "the changed kubeconfig is never read again", || reading.exists());
}

/// Wait until that read of `file` has ended, then for the server to have taken what it said.
#[cfg(target_os = "linux")]
fn wait_for_slow_read_to_end(file: &Path) {
    let reading = slow_read_marker(file);
    wait_until(PATIENCE, "the read of the kubeconfig never ended", || !reading.exists());
    std::thread::sleep(Duration::from_millis(1500));
}

/// What sits next to `file` while the server reads it slowly.
#[cfg(target_os = "linux")]
fn slow_read_marker(file: &Path) -> PathBuf {
    let mut reading = file.as_os_str().to_owned();
    reading.push(".reading");
    PathBuf::from(reading)
}

/// A tool that acts waits for the pane's kubeconfig to be read again, in a list that keeps
/// the keys typed meanwhile (Esc gives up: nothing starts), and runs where it was asked for or
/// not at all.
#[cfg(target_os = "linux")]
#[test]
fn a_tool_waits_for_its_check_and_runs_only_where_it_was_asked() {
    let env = Env::new("toolwait");
    let kube = env.root.join("kube.yaml");
    std::fs::write(&kube, kubeconfig_text("dev")).unwrap();
    let marked = env.root.join("marked");
    let cfg = env.root.join("config/kraal/config.toml");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str(&format!("\n[tools.mark]\ncommand = \"touch {}\"\nkey = \"m\"\n", marked.display()));
    std::fs::write(&cfg, text).unwrap();
    env.workspace("w", &format!("layout {{\n    {}\n}}\n", kube_shell_pane(&env, "k", &kube)));
    // Reads of the kubeconfig take 3 s while `kube.yaml.slow` exists, as on a stalled disk.
    let out = env.cmd(&["open", "w"]).env("KRAAL_TEST_SLOW_KUBECONFIG_MS", "3000").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_for_context(&env, "dev");
    env.wait_for_text("k", "$");
    std::fs::write(env.root.join("kube.yaml.slow"), "").unwrap();
    // The same content, a new modification time: the next read is a real, slow one.
    let touch = || std::fs::write(&kube, kubeconfig_text("dev")).unwrap();
    let mut client = env.client();
    let mut typed = |bytes: &[u8]| std::io::Write::write_all(client.stdin.as_mut().unwrap(), bytes).unwrap();

    // Given up while it waits: the keys stay in the list, and nothing starts.
    touch();
    typed(b"\x02Rm");
    std::thread::sleep(Duration::from_millis(400));
    typed(b"zz");
    std::thread::sleep(Duration::from_millis(300));
    typed(b"\x1b");
    std::thread::sleep(Duration::from_millis(4000));
    assert!(!marked.exists(), "a tool given up on started anyway");
    let screen = env.run(&["read", "-p", "k"]);
    assert!(!screen.contains("zz"), "keys typed while the tool waited reached the pane:\n{screen}");

    // The pane moves while the tool waits: it does not start somewhere else.
    touch();
    typed(b"\x02Rm");
    std::thread::sleep(Duration::from_millis(400));
    env.run(&["send", "-p", "k", "-e", "cd /tmp"]);
    std::thread::sleep(Duration::from_millis(4500));
    assert!(!marked.exists(), "the tool started in a place it was not asked for");

    // Otherwise it runs once the kubeconfig has been read.
    touch();
    typed(b"\x02Rm");
    let start = Instant::now();
    while !marked.exists() {
        assert!(start.elapsed() < Duration::from_secs(10), "the tool runs after the check");
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = client.kill();
    let _ = client.wait();
}

/// Keys broadcast to a pane whose kubeconfig is being read again wait for the read: once it
/// says production, they are asked about before reaching it.
#[cfg(target_os = "linux")]
#[test]
fn broadcast_waits_while_a_pane_context_is_read_again() {
    let env = Env::new("bcastread");
    let kube = env.root.join("kube.yaml");
    std::fs::write(&kube, kubeconfig_text("dev")).unwrap();
    env.workspace(
        "w",
        &format!(
            "layout {{\n    pane split_direction=\"vertical\" {{\n        pane name=\"a\"\n        {}\n    }}\n}}\n",
            kube_shell_pane(&env, "b", &kube)
        ),
    );
    // A real read of the kubeconfig takes 4.8 s while `kube.yaml.slow` exists: less than the
    // 5 s after which it would count as unknown.
    let out = env.cmd(&["open", "w"]).env("KRAAL_TEST_SLOW_KUBECONFIG_MS", "4800").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_for_context(&env, "dev");
    env.wait_for_text("b", "$");
    let mut client = env.client();
    let mut typed = |bytes: &[u8]| std::io::Write::write_all(client.stdin.as_mut().unwrap(), bytes).unwrap();
    env.run(&["focus", "a"]);
    typed(b"\x02b");
    std::thread::sleep(Duration::from_millis(500));
    // `kubectx prod-x`: the next scan (within 3.4 s) starts a read that lasts 4.8 s.
    std::fs::write(env.root.join("kube.yaml.slow"), "").unwrap();
    std::fs::write(&kube, kubeconfig_text("prod-x")).unwrap();
    wait_for_slow_read(&kube);
    typed(b"echo hello-bcast");
    std::thread::sleep(Duration::from_millis(600));
    for pane in ["a", "b"] {
        let screen = env.run(&["read", "-p", pane]);
        assert!(!screen.contains("hello-bcast"), "typed while {pane}'s context was read:\n{screen}");
    }
    // The read ends, says production: the keys are asked about, not sent.
    wait_for_slow_read_to_end(&kube);
    let screen = env.run(&["read", "-p", "b"]);
    assert!(!screen.contains("hello-bcast"), "keys reached a production pane unasked:\n{screen}");
    typed(b"y");
    env.wait_for_text("b", "echo hello-bcast");
    env.wait_for_text("a", "echo hello-bcast");
    let _ = client.kill();
    let _ = client.wait();
}

/// What is typed while a broadcast pane's context is read again goes as one: when the read
/// says production and the answer is no, none of it is sent, the Enter at its end included.
#[cfg(target_os = "linux")]
#[test]
fn a_no_drops_everything_typed_while_a_context_was_read() {
    let env = Env::new("heldno");
    let kube = env.root.join("kube.yaml");
    std::fs::write(&kube, kubeconfig_text("dev")).unwrap();
    env.workspace(
        "w",
        &format!(
            "layout {{\n    pane split_direction=\"vertical\" {{\n        pane name=\"a\"\n        {}\n    }}\n}}\n",
            kube_shell_pane(&env, "b", &kube)
        ),
    );
    let out = env.cmd(&["open", "w"]).env("KRAAL_TEST_SLOW_KUBECONFIG_MS", "4800").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_for_context(&env, "dev");
    env.wait_for_text("b", "$");
    let mut client = env.client();
    let mut typed = |bytes: &[u8]| std::io::Write::write_all(client.stdin.as_mut().unwrap(), bytes).unwrap();
    env.run(&["focus", "a"]);
    typed(b"\x02b");
    std::thread::sleep(Duration::from_millis(500));
    std::fs::write(env.root.join("kube.yaml.slow"), "").unwrap();
    std::fs::write(&kube, kubeconfig_text("prod-x")).unwrap();
    wait_for_slow_read(&kube);
    // A whole command, Enter included, typed while the read goes on.
    typed(b" echo should-not-run\r");
    wait_for_slow_read_to_end(&kube);
    // The read said production: the question is about all of it. No.
    typed(b"n");
    std::thread::sleep(Duration::from_millis(1500));
    for pane in ["a", "b"] {
        let screen = env.run(&["read", "-p", pane]);
        assert!(!screen.contains("should-not-run"), "sent to {pane} after a no:\n{screen}");
    }
    let _ = client.kill();
    let _ = client.wait();
}

/// A yes is for the panes the question named, as they were then: given while one of them has
/// its context read again, it waits for the read, and nothing goes if the read changed it.
#[cfg(target_os = "linux")]
#[test]
fn a_yes_waits_for_a_context_read_and_holds_only_if_nothing_changed() {
    let env = Env::new("heldyes");
    let kube_b = env.root.join("kube-b.yaml");
    let kube_c = env.root.join("kube-c.yaml");
    std::fs::write(&kube_b, kubeconfig_text("prod-x")).unwrap();
    std::fs::write(&kube_c, kubeconfig_text("dev")).unwrap();
    env.workspace(
        "w",
        &format!(
            "layout {{\n    pane split_direction=\"vertical\" {{\n        pane name=\"a\"\n        {}\n        {}\n    }}\n}}\n",
            kube_shell_pane(&env, "b", &kube_b),
            kube_shell_pane(&env, "c", &kube_c)
        ),
    );
    let out = env.cmd(&["open", "w"]).env("KRAAL_TEST_SLOW_KUBECONFIG_MS", "4800").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_for_context(&env, "prod-x");
    wait_for_context(&env, "dev");
    env.wait_for_text("b", "$");
    env.wait_for_text("c", "$");
    let mut client = env.client();
    let mut typed = |bytes: &[u8]| std::io::Write::write_all(client.stdin.as_mut().unwrap(), bytes).unwrap();
    env.run(&["focus", "a"]);
    typed(b"\x02b");
    std::thread::sleep(Duration::from_millis(500));
    // b is production: the paste is asked about.
    typed(b"\x1b[200~echo answered-early\x1b[201~");
    std::thread::sleep(Duration::from_millis(500));
    // Meanwhile c becomes production, and its read lasts.
    std::fs::write(env.root.join("kube-c.yaml.slow"), "").unwrap();
    std::fs::write(&kube_c, kubeconfig_text("prod-x")).unwrap();
    wait_for_slow_read(&kube_c);
    // Yes, before c's read ends: it was not production when asked.
    typed(b"y");
    std::thread::sleep(Duration::from_millis(6000));
    for pane in ["a", "b", "c"] {
        let screen = env.run(&["read", "-p", pane]);
        assert!(!screen.contains("answered-early"), "sent to {pane} though c changed after the question:\n{screen}");
    }
    // Asked again (c is production now), answered during a read that changes nothing: it goes,
    // once the read has ended.
    typed(b"\x1b[200~echo answered-again\x1b[201~");
    std::thread::sleep(Duration::from_millis(500));
    std::fs::write(&kube_c, kubeconfig_text("prod-x")).unwrap();
    wait_for_slow_read(&kube_c);
    typed(b"y");
    for pane in ["a", "b", "c"] {
        env.wait_for_text(pane, "echo answered-again");
    }
    let _ = client.kill();
    let _ = client.wait();
}

/// What is typed in a tab while a broadcast pane's context is read again goes to that tab's
/// panes once the read ends, even when another tab is shown by then.
#[cfg(target_os = "linux")]
#[test]
fn input_that_waited_goes_where_it_was_typed() {
    let env = Env::new("heldtab");
    let kube = env.root.join("kube.yaml");
    std::fs::write(&kube, kubeconfig_text("dev")).unwrap();
    env.workspace(
        "w",
        &format!(
            "layout {{\n    tab name=\"one\" {{\n        pane split_direction=\"vertical\" {{\n            pane name=\"a\"\n            {}\n        }}\n    }}\n    tab name=\"two\" {{\n        pane split_direction=\"vertical\" {{\n            pane name=\"c\"\n            pane name=\"d\"\n        }}\n    }}\n}}\n",
            kube_shell_pane(&env, "b", &kube)
        ),
    );
    let out = env.cmd(&["open", "w"]).env("KRAAL_TEST_SLOW_KUBECONFIG_MS", "4800").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_for_context(&env, "dev");
    env.wait_for_text("b", "$");
    let mut client = env.client();
    let mut typed = |bytes: &[u8]| std::io::Write::write_all(client.stdin.as_mut().unwrap(), bytes).unwrap();
    env.run(&["focus", "a"]);
    typed(b"\x02b");
    std::thread::sleep(Duration::from_millis(500));
    // The same context written again: a read that changes nothing, and lasts.
    std::fs::write(env.root.join("kube.yaml.slow"), "").unwrap();
    std::fs::write(&kube, kubeconfig_text("dev")).unwrap();
    wait_for_slow_read(&kube);
    typed(b"echo typed-in-one");
    std::thread::sleep(Duration::from_millis(300));
    // Tab two is shown before the read ends.
    typed(b"\x022");
    std::thread::sleep(Duration::from_millis(6000));
    for pane in ["c", "d"] {
        let screen = env.run(&["read", "-p", pane]);
        assert!(!screen.contains("typed-in-one"), "reached {pane} in the tab shown later:\n{screen}");
    }
    env.wait_for_text("a", "echo typed-in-one");
    env.wait_for_text("b", "echo typed-in-one");
    let _ = client.kill();
    let _ = client.wait();
}

/// A yes given while a pane's kubeconfig is read again and the read does not answer is not
/// taken for the context known before the read (the file may say another one now): nothing goes.
#[cfg(target_os = "linux")]
#[test]
fn a_yes_during_a_read_that_does_not_answer_sends_nothing() {
    let env = Env::new("heldstall");
    let kube = env.root.join("kube.yaml");
    std::fs::write(&kube, kubeconfig_text("prod-x")).unwrap();
    env.workspace(
        "w",
        &format!(
            "layout {{\n    pane split_direction=\"vertical\" {{\n        pane name=\"a\"\n        {}\n    }}\n}}\n",
            kube_shell_pane(&env, "b", &kube)
        ),
    );
    // A read of the kubeconfig takes 20 s while `kube.yaml.slow` exists: after 5 s it does not
    // answer.
    let out = env.cmd(&["open", "w"]).env("KRAAL_TEST_SLOW_KUBECONFIG_MS", "20000").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_for_context(&env, "prod-x");
    env.wait_for_text("b", "$");
    let mut client = env.client();
    let mut typed = |bytes: &[u8]| std::io::Write::write_all(client.stdin.as_mut().unwrap(), bytes).unwrap();
    env.run(&["focus", "a"]);
    typed(b"\x02b");
    std::thread::sleep(Duration::from_millis(500));
    // b is production: the paste is asked about.
    typed(b"\x1b[200~echo stalled-yes\x1b[201~");
    std::thread::sleep(Duration::from_millis(500));
    // Meanwhile b's kubeconfig changes, and the read of it does not answer.
    std::fs::write(env.root.join("kube.yaml.slow"), "").unwrap();
    std::fs::write(&kube, kubeconfig_text("dev")).unwrap();
    std::thread::sleep(Duration::from_millis(9500));
    typed(b"y");
    std::thread::sleep(Duration::from_millis(1500));
    for pane in ["a", "b"] {
        let screen = env.run(&["read", "-p", pane]);
        assert!(!screen.contains("stalled-yes"), "sent to {pane} on a yes for what the file said before:\n{screen}");
    }
    let _ = client.kill();
    let _ = client.wait();
}

/// As for a kubeconfig: once the directory a Terraform run worked on is read again and does not
/// answer, the workspace read before no longer counts, production or not. A yes given for it
/// sends nothing.
#[cfg(target_os = "linux")]
#[test]
fn a_yes_during_a_terraform_read_that_does_not_answer_sends_nothing() {
    let env = Env::new("tfstall");
    let infra = env.root.join("infra");
    std::fs::create_dir_all(infra.join(".terraform")).unwrap();
    let environment = infra.join(".terraform/environment");
    std::fs::write(&environment, "prod-a\n").unwrap();
    // A Terraform run, seen while it runs: the pane's workspace is the one of the directory it
    // ran in, read off the event loop.
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(&bin.join("terraform"), "#!/bin/sh\nsleep 2\nexport PS1='$ '\nexec sh\n");
    env.workspace(
        "w",
        &format!(
            "layout {{\n    pane split_direction=\"vertical\" {{\n        pane name=\"a\"\n        \
             pane name=\"b\" cwd=\"{}\" command=\"{}\" {{ args \"plan\"; }}\n    }}\n}}\n",
            infra.display(),
            bin.join("terraform").display()
        ),
    );
    // A read of the environment file takes 20 s while `environment.slow` exists: after 5 s it
    // does not answer.
    let out = env.cmd(&["open", "w"]).env("KRAAL_TEST_SLOW_KUBECONFIG_MS", "20000").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_for_context(&env, "tf:prod-a");
    env.wait_for_text("b", "$");
    let mut client = env.client();
    let mut typed = |bytes: &[u8]| std::io::Write::write_all(client.stdin.as_mut().unwrap(), bytes).unwrap();
    env.run(&["focus", "a"]);
    typed(b"\x02b");
    std::thread::sleep(Duration::from_millis(500));
    // b is production: the paste is asked about.
    typed(b"\x1b[200~echo stalled-tf-yes\x1b[201~");
    std::thread::sleep(Duration::from_millis(500));
    // Meanwhile b's workspace changes, and the read of it does not answer.
    std::fs::write(infra.join(".terraform/environment.slow"), "").unwrap();
    std::fs::write(&environment, "prod-b\n").unwrap();
    std::thread::sleep(Duration::from_millis(9500));
    typed(b"y");
    std::thread::sleep(Duration::from_millis(1500));
    for pane in ["a", "b"] {
        let screen = env.run(&["read", "-p", pane]);
        assert!(!screen.contains("stalled-tf-yes"), "sent to {pane} on a yes for the workspace before:\n{screen}");
    }
    let _ = client.kill();
    let _ = client.wait();
}

/// The same with no Terraform run seen: the workspace is that of the directory the pane is in.
/// Once a read of it does not answer, the workspace read before no longer counts, and a yes
/// given for it sends nothing.
#[cfg(target_os = "linux")]
#[test]
fn a_yes_during_a_read_of_the_directory_a_pane_is_in_that_does_not_answer_sends_nothing() {
    let env = Env::new("tfdirstall");
    let infra = env.root.join("infra");
    std::fs::create_dir_all(infra.join(".terraform")).unwrap();
    let environment = infra.join(".terraform/environment");
    std::fs::write(&environment, "prod-a\n").unwrap();
    env.workspace(
        "w",
        &format!(
            "layout {{\n    pane split_direction=\"vertical\" {{\n        pane name=\"a\"\n        \
             pane name=\"b\" cwd=\"{}\" command=\"sh\"\n    }}\n}}\n",
            infra.display()
        ),
    );
    // A read of the environment file takes 20 s while `environment.slow` exists: after 5 s it
    // does not answer.
    let out = env.cmd(&["open", "w"]).env("KRAAL_TEST_SLOW_KUBECONFIG_MS", "20000").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_for_context(&env, "tf:prod-a");
    env.wait_for_text("b", "$");
    let mut client = env.client();
    let mut typed = |bytes: &[u8]| std::io::Write::write_all(client.stdin.as_mut().unwrap(), bytes).unwrap();
    env.run(&["focus", "a"]);
    typed(b"\x02b");
    std::thread::sleep(Duration::from_millis(500));
    // b is production: the paste is asked about.
    typed(b"\x1b[200~echo stalled-dir-yes\x1b[201~");
    std::thread::sleep(Duration::from_millis(500));
    // Meanwhile the directory's workspace changes, and the read of it does not answer.
    std::fs::write(infra.join(".terraform/environment.slow"), "").unwrap();
    std::fs::write(&environment, "prod-b\n").unwrap();
    std::thread::sleep(Duration::from_millis(9500));
    typed(b"y");
    std::thread::sleep(Duration::from_millis(1500));
    for pane in ["a", "b"] {
        let screen = env.run(&["read", "-p", pane]);
        assert!(!screen.contains("stalled-dir-yes"), "sent to {pane} on a yes for the workspace before:\n{screen}");
    }
    let _ = client.kill();
    let _ = client.wait();
}

/// A directory known to hold a production Terraform workspace stays known after the pane leaves
/// it: coming back, input is guarded at once, not only after the next read.
#[cfg(target_os = "linux")]
#[test]
fn a_production_directory_left_and_come_back_to_is_still_guarded() {
    let env = Env::new("tfback");
    let infra = env.root.join("infra");
    std::fs::create_dir_all(infra.join(".terraform")).unwrap();
    std::fs::write(infra.join(".terraform/environment"), "prod-a\n").unwrap();
    env.workspace("w", &format!("layout {{\n    pane name=\"b\" cwd=\"{}\" command=\"sh\"\n}}\n", infra.display()));
    // A read of the environment file takes ten minutes while `environment.slow` exists: longer
    // than this test waits for anything.
    let out = env.cmd(&["open", "w"]).env("KRAAL_TEST_SLOW_KUBECONFIG_MS", "600000").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_for_context(&env, "tf:prod-a");
    env.wait_for_text("b", "$");
    let refused = |text: &str| !env.cmd(&["send", "-p", "b", "-e", text]).output().unwrap().status.success();
    assert!(refused("echo here"), "b is production");
    env.run(&["send", "-p", "b", "-e", "--production", "cd /"]);
    wait_for_contexts(&env, "b", &[], None);
    // A scan (every 3 s) passes while no pane is in the directory.
    std::thread::sleep(Duration::from_millis(4000));
    assert!(!refused("echo away"), "away from the directory, b is not production");
    // The directory cannot be read again in time from here on: what is said of it once the pane
    // is back is what was known, however long the pane takes to get there.
    std::fs::write(infra.join(".terraform/environment.slow"), "").unwrap();
    env.run(&["send", "-p", "b", "-e", &format!("cd {}", infra.display())]);
    wait_for_contexts(&env, "b", &["tf:prod-a"], Some("Terraform workspace prod-a"));
    assert!(refused("echo back"), "back in the directory, b is production again");
}

/// The pane named `name`, as `kraal panes --json` lists it.
#[cfg(target_os = "linux")]
fn pane_json(env: &Env, name: &str) -> serde_json::Value {
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    panes.as_array().unwrap().iter().find(|p| p["name"] == name).cloned().unwrap()
}

/// Wait until the pane named `name` shows `contexts` and is production for `why` (or not).
#[cfg(target_os = "linux")]
fn wait_for_contexts(env: &Env, name: &str, contexts: &[&str], why: Option<&str>) {
    let start = Instant::now();
    loop {
        let pane = pane_json(env, name);
        if pane["contexts"] == serde_json::json!(contexts) && pane["production"] == serde_json::json!(why) {
            return;
        }
        assert!(start.elapsed() < PATIENCE, "never {contexts:?} ({why:?}): {pane}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A shell on a server with what `kraal integration show` prints in its startup file reports
/// what it works on: its pane shows it, and is guarded like a local one, until the session ends.
/// The `ssh` here is `script`, which gives the shell a terminal of its own and relays it as ssh
/// does; the shell finds kraal by asking it, through that relay.
#[cfg(target_os = "linux")]
fn contexts_over_ssh_hold_until_the_session_ends(shell: &str) {
    let script = ["/usr/bin/script", "/bin/script"].into_iter().map(Path::new).find(|p| p.exists());
    let Some(script) = script else { return };
    if Command::new(shell).arg("-c").arg("true").output().is_err() {
        return;
    }
    let env = Env::new(&format!("sshctx-{shell}"));
    let config = env.root.join("config/kraal/config.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{text}[servers]\nauto_add = false\n")).unwrap();
    // The server's home: a kubeconfig on production, and the shell integration.
    let remote = env.root.join("remote");
    std::fs::create_dir_all(remote.join(".kube")).unwrap();
    let kube = "contexts:\n- context:\n    namespace: payments\n  name: prod-eks\n- name: dev\n  context: {}\n\
        current-context: prod-eks\n";
    std::fs::write(remote.join(".kube/config"), kube).unwrap();
    std::fs::write(remote.join("dev.yaml"), kube.replace("current-context: prod-eks", "current-context: dev")).unwrap();
    let block = env.run(&["integration", "show", shell]);
    let r = remote.display();
    let start = match shell {
        "bash" => {
            std::fs::write(remote.join(".bashrc"), format!("PS1='remote$ '\n{block}\n")).unwrap();
            format!("bash --rcfile {r}/.bashrc -i")
        }
        "zsh" => {
            std::fs::write(remote.join(".zshrc"), format!("PS1='remote$ '\n{block}\n")).unwrap();
            format!("env ZDOTDIR={r} zsh --no-globalrcs -i")
        }
        _ => {
            let fish = remote.join(".config/fish");
            std::fs::create_dir_all(fish.join("conf.d")).unwrap();
            std::fs::write(fish.join("conf.d/kraal.fish"), format!("{block}\n")).unwrap();
            let prompt = "set -g fish_greeting\nfunction fish_prompt\n    printf 'remote$ '\nend\n";
            std::fs::write(fish.join("config.fish"), prompt).unwrap();
            format!("env XDG_CONFIG_HOME={r}/.config XDG_DATA_HOME={r}/.local/share fish -i")
        }
    };
    let export = |k: &str, v: &str| match shell {
        "fish" => format!("set -gx {k} {v}"),
        _ => format!("export {k}={v}"),
    };
    let login = env.root.join("login");
    write_script(
        &login,
        &format!(
            "#!/bin/sh\nexec env -i PATH=/usr/bin:/bin HOME={r} TERM=xterm-256color \
             SSH_CONNECTION='10.0.0.1 50000 10.0.0.5 22' {start}\n"
        ),
    );
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(script, bin.join("ssh")).unwrap();
    env.workspace("w", "layout {\n    pane name=\"ops\"\n}\n");
    env.run(&["open", "w"]);
    env.wait_for_text("ops", "$");
    // `ssh -q -f -c <cipher> bastion`, as kraal reads it; `script` writes its log to ./bastion.
    let connect = format!(
        "cd {} && SHELL=/bin/sh {} -q -f -c {} bastion",
        env.root.display(),
        bin.join("ssh").display(),
        login.display()
    );
    env.run(&["send", "-p", "ops", "--enter", &connect]);
    wait_for_contexts(&env, "ops", &["⎈ prod-eks/payments"], Some("Kubernetes context prod-eks on bastion"));
    assert_eq!(pane_json(&env, "ops")["server"], "bastion");
    env.wait_for_text("ops", "remote$");
    let out = env.try_run(&["send", "-p", "ops", "--enter", "echo hi"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("is production"), "{}", String::from_utf8_lossy(&out.stderr));
    // Another kubeconfig on the server: the next prompt says so.
    env.run(&["send", "-p", "ops", "--production", "--enter", &export("KUBECONFIG", "$HOME/dev.yaml")]);
    wait_for_contexts(&env, "ops", &["⎈ dev"], None);
    // A command that runs and ends keeps it.
    env.run(&["send", "-p", "ops", "--enter", "true"]);
    env.run(&["send", "-p", "ops", "--enter", &export("AWS_PROFILE", "prd-admin")]);
    wait_for_contexts(&env, "ops", &["⎈ dev", "aws:prd-admin"], Some("AWS profile prd-admin on bastion"));
    // The session ends: what it reported goes with it.
    env.run(&["send", "-p", "ops", "--production", "--enter", "exit"]);
    wait_for_contexts(&env, "ops", &[], None);
    assert!(pane_json(&env, "ops")["server"].is_null());
    // A local shell's report is not taken: kraal reads local contexts itself.
    env.run(&["send", "-p", "ops", "--enter", "printf '\\033]7701;context;k8s=prod-x\\007'; echo printed"]);
    env.wait_for_text("ops", "printed");
    std::thread::sleep(Duration::from_millis(1000));
    wait_for_contexts(&env, "ops", &[], None);
}

#[cfg(target_os = "linux")]
#[test]
fn contexts_bash_reports_over_ssh_hold_until_the_session_ends() {
    contexts_over_ssh_hold_until_the_session_ends("bash");
}

#[cfg(target_os = "linux")]
#[test]
fn contexts_zsh_reports_over_ssh_hold_until_the_session_ends() {
    contexts_over_ssh_hold_until_the_session_ends("zsh");
}

/// fish 4 marks its prompts itself too: twice marked, a prompt keeps what was reported.
#[cfg(target_os = "linux")]
#[test]
fn contexts_fish_reports_over_ssh_hold_until_the_session_ends() {
    contexts_over_ssh_hold_until_the_session_ends("fish");
}

/// When a long paste turns out to go into production while its last pieces wait for a context
/// read, the rest of it does not go, and neither does what was typed after it: an Enter would
/// run the part of it that went.
#[cfg(target_os = "linux")]
#[test]
fn a_long_paste_cut_for_production_takes_the_enter_after_it_along() {
    let env = Env::new("heldpiece");
    let kube = env.root.join("kube.yaml");
    std::fs::write(&kube, kubeconfig_text("dev")).unwrap();
    env.workspace("w", &format!("layout {{\n    {}\n}}\n", kube_shell_pane(&env, "k", &kube)));
    let out = env.cmd(&["open", "w"]).env("KRAAL_TEST_SLOW_KUBECONFIG_MS", "4800").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_for_context(&env, "dev");
    env.wait_for_text("k", "$");
    // What reaches the pane goes to a file, byte for byte.
    let got = env.root.join("got");
    env.run(&["send", "-p", "k", "-e", &format!("stty raw -echo; cat > {}", got.display())]);
    std::thread::sleep(Duration::from_millis(500));
    let mut client = RawClient::attach(&env, "w");
    std::thread::sleep(Duration::from_millis(500));
    // More than a paste held whole (8 MiB): it goes as it comes.
    let piece = vec![b'a'; 64 << 10];
    client.send(b"\x1b[200~");
    for _ in 0..(9 << 20) / piece.len() {
        client.send(&piece);
    }
    // `kubectx prod-x`: the next scan (within 3.4 s) starts a read that lasts 4.8 s, and the
    // rest of the paste, then an Enter, wait for it.
    std::fs::write(env.root.join("kube.yaml.slow"), "").unwrap();
    std::fs::write(&kube, kubeconfig_text("prod-x")).unwrap();
    wait_for_slow_read(&kube);
    for _ in 0..16 {
        client.send(&piece);
    }
    client.send(b"\x1b[201~");
    client.send(b"\r");
    // The read ends and says production.
    std::thread::sleep(Duration::from_millis(6000));
    let bytes = std::fs::read(&got).unwrap();
    assert!(bytes.len() >= 8 << 20, "the start of the paste went: {} bytes", bytes.len());
    assert!(!bytes.contains(&b'\r'), "the Enter typed after a cut paste reached the pane");
    assert!(bytes.len() < (9 << 20) + 16 * piece.len(), "the rest of the paste went into production");
}

#[cfg(unix)]
#[test]
fn refuses_a_runtime_dir_that_is_a_symlink() {
    let env = Env::new("symlinkrun");
    let elsewhere = env.root.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let run = env.root.join("run");
    std::fs::remove_dir_all(&run).unwrap();
    std::fs::create_dir_all(&run).unwrap();
    std::os::unix::fs::symlink(&elsewhere, run.join("kraal")).unwrap();
    let out = env.try_run(&["open", "w"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("refusing"), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn new_tab_puts_the_pane_in_a_new_tab() {
    let env = Env::new("newtab");
    env.workspace("w", "layout {\n    pane name=\"first\"\n}\n");
    env.run(&["open", "w"]);
    env.run(&["run", "--tab", "--target", "first", "--name", "second", "--", "sh"]);
    let json = env.run(&["panes", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let tabs: Vec<&str> = v.as_array().unwrap().iter().map(|p| p["tab"].as_str().unwrap()).collect();
    assert_eq!(tabs.len(), 2);
    assert_ne!(tabs[0], tabs[1], "the new pane must be alone in its new tab: {json}");
}

#[test]
fn account_rules_see_a_worktree_anywhere_as_its_repository() {
    let env = Env::new("acctrule");
    let repo = env.root.join("company/repo");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["commit", "-q", "--allow-empty", "-m", "init"]);
    // Linked worktrees far from the repository: only git can tell where they belong.
    let feature = env.root.join("elsewhere/feature");
    let other = env.root.join("elsewhere/other");
    git(&["worktree", "add", "-q", "-b", "feature", feature.to_str().unwrap()]);
    git(&["worktree", "add", "-q", "-b", "other", other.to_str().unwrap()]);
    // A stand-in for claude that says which config home it was given.
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(
        &bin.join("claude"),
        "#!/bin/sh\n[ \"$2\" = status ] && exit 1\necho \"home=${CLAUDE_CONFIG_DIR:-default}\"\nexec sleep 100\n",
    );
    let pro = env.root.join("pro-home");
    std::fs::create_dir_all(&pro).unwrap();
    let repo = std::fs::canonicalize(&repo).unwrap();
    std::fs::write(
        env.root.join("config/kraal/accounts.toml"),
        format!(
            "[[account]]\nname = \"pro\"\nagent = \"claude\"\nhome = {:?}\n\n[[rule]]\npath = {:?}\naccounts = [\"pro\"]\n",
            pro.display().to_string(),
            repo.display().to_string()
        ),
    )
    .unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let run = |args: &[&str], dir: &Path| {
        let out = env.cmd(args).env("PATH", &path).current_dir(dir).output().unwrap();
        assert!(out.status.success(), "kraal {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    // From a definition: the pane waits for git, then starts on the account the rule names.
    env.workspace("w", &format!("layout {{\n    pane name=\"ag\" agent=\"claude\" cwd={:?}\n}}\n", feature.display()));
    run(&["open", "w"], &env.root);
    env.wait_for_text("ag", &format!("home={}", pro.display()));
    // From the CLI inside the session, in the other worktree: the same.
    let socket = env.root.join("run/kraal/acctrule.sock");
    let out = env
        .cmd(&["agent", "claude", "--tab"])
        .env("PATH", &path)
        .env("KRAAL_SOCKET", &socket)
        .current_dir(&other)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    // It prints the id of the pane it opened, as `kraal run` does.
    let opened = String::from_utf8(out.stdout).unwrap();
    let pane = panes.as_array().unwrap().iter().find(|p| format!("{}\n", p["id"]) == opened);
    let dir = pane.and_then(|p| p["cwd"].as_str()).map(|dir| Path::new(dir).canonicalize().unwrap());
    assert_eq!(dir, Some(other.canonicalize().unwrap()), "{opened:?} among {panes}");
    let ids: Vec<String> = panes.as_array().unwrap().iter().map(|p| p["id"].to_string()).collect();
    let start = Instant::now();
    loop {
        let screens: Vec<String> = ids.iter().map(|id| env.run(&["read", "-p", id.as_str()])).collect();
        if screens.iter().filter(|s| s.contains(&format!("home={}", pro.display()))).count() == 2 {
            break;
        }
        assert!(
            !screens.iter().any(|s| s.contains("home=default")),
            "an agent started on an account the rule forbids: {screens:?}"
        );
        assert!(start.elapsed() < Duration::from_secs(10), "{screens:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn an_agent_waiting_for_its_account_waits_again_after_a_restart() {
    let env = Env::new("pendagent");
    let repo = env.root.join("company/repo");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["commit", "-q", "--allow-empty", "-m", "init"]);
    let feature = env.root.join("elsewhere/feature");
    git(&["worktree", "add", "-q", "-b", "feature", feature.to_str().unwrap()]);
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(
        &bin.join("claude"),
        "#!/bin/sh\n[ \"$2\" = status ] && exit 1\necho \"home=${CLAUDE_CONFIG_DIR:-default}\"\nexec sleep 100\n",
    );
    // A git that hangs on the question kraal asks while `slow` exists: the agent cannot get its
    // account before the session is stopped.
    let real_git =
        String::from_utf8(Command::new("sh").args(["-c", "command -v git"]).output().unwrap().stdout).unwrap();
    let slow = env.root.join("slow");
    write_script(
        &bin.join("git"),
        &format!(
            "#!/bin/sh\ncase \"$*\" in *--git-common-dir*) [ -e {slow:?} ] && exec sleep 8 ;; esac\nexec {} \"$@\"\n",
            real_git.trim()
        ),
    );
    std::fs::write(&slow, "").unwrap();
    let pro = env.root.join("pro-home");
    std::fs::create_dir_all(&pro).unwrap();
    let canonical = std::fs::canonicalize(&repo).unwrap();
    std::fs::write(
        env.root.join("config/kraal/accounts.toml"),
        format!(
            "[[account]]\nname = \"pro\"\nagent = \"claude\"\nhome = {:?}\n\n[[rule]]\npath = {:?}\naccounts = [\"pro\"]\n",
            pro.display().to_string(),
            canonical.display().to_string()
        ),
    )
    .unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let run = |args: &[&str]| {
        let out = env.cmd(args).env("PATH", &path).current_dir(&env.root).output().unwrap();
        assert!(out.status.success(), "kraal {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    env.workspace("w", &format!("layout {{\n    pane name=\"ag\" agent=\"claude\" cwd={:?}\n}}\n", feature.display()));
    run(&["open", "w"]);
    env.wait_for_text("ag", "waits for the account rules");
    run(&["kill"]);
    let state = std::fs::read_to_string(env.state_file()).unwrap();
    assert!(state.contains("pending_agent"), "{state}");
    // Restarted, with git answering again: the agent starts on the account the rule names.
    std::fs::remove_file(&slow).unwrap();
    run(&["open", "w"]);
    env.wait_for_text("ag", &format!("home={}", pro.display()));
}

#[test]
fn worktrees_for_agents() {
    let env = Env::new("worktrees");
    let repo = env.root.join("app");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hi\n").unwrap();
    git(&["add", "README.md"]);
    git(&["commit", "-q", "-m", "init"]);
    let wt = |args: &[&str]| env.cmd(&[&["wt"], args].concat()).current_dir(&repo).output().unwrap();
    let out = wt(&["new", "fix-login"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let path = env.root.join("app.worktrees/fix-login");
    assert!(path.join("README.md").exists());
    let list = String::from_utf8(wt(&["list"]).stdout).unwrap();
    assert!(list.contains("fix-login") && list.contains("kraal/fix-login"), "{list}");
    std::fs::write(path.join("README.md"), "changed\n").unwrap();
    assert!(!wt(&["rm", "fix-login"]).status.success(), "uncommitted changes need --force");
    assert!(wt(&["rm", "fix-login", "--force"]).status.success());
    assert!(!path.exists());
}

/// What git says of a pane's directory follows what is done in its repository from elsewhere,
/// promptly, while the pane prints nothing: a checkout, a commit.
#[test]
fn git_in_frames_follows_changes_made_elsewhere() {
    let env = Env::new("gitwatch");
    let repo = env.root.join("repo");
    let git = git_repo(&repo, &[("a.txt", "1")]);
    // An upstream, for the commits ahead of it.
    git(&["config", "remote.origin.url", "/nowhere"]);
    git(&["config", "remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"]);
    git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    env.workspace("w", &format!("layout {{\n    pane name=\"a\" cwd={:?}\n}}\n", repo.display().to_string()));
    env.run(&["open", "w"]);
    let client = RawClient::attach(&env, "w");
    client.wait_for_screen("main");
    env.wait_for_text("a", "$");
    // Long enough for the server to be idle.
    std::thread::sleep(Duration::from_secs(4));
    let start = Instant::now();
    git(&["checkout", "-q", "-b", "qqbranch", "--track", "origin/main"]);
    client.wait_for_screen("qqbranch");
    assert!(start.elapsed() < Duration::from_secs(2), "a checkout showed after {:?}", start.elapsed());
    // A commit replaces the branch's ref, not the index.
    std::thread::sleep(Duration::from_secs(2));
    let start = Instant::now();
    git(&["commit", "-q", "--allow-empty", "-m", "two"]);
    client.wait_for_screen("↑1");
    assert!(start.elapsed() < Duration::from_secs(2), "a commit showed after {:?}", start.elapsed());
}

/// An idle server sleeps: no thread of it wakes up again and again while nothing happens, not
/// even to wait for the reset of a usage window.
#[cfg(target_os = "linux")]
#[test]
fn an_idle_server_sleeps() {
    let env = Env::new("sleeps");
    std::fs::write(
        env.root.join("config/kraal/accounts.toml"),
        "[[account]]\nid = 'work'\nname = 'work'\nagent = 'claude'\nhome = '~/work'\n",
    )
    .unwrap();
    // In a directory where nothing happens (the home directory may be busy: its changes are
    // looked at).
    let quiet = env.root.join("quiet");
    std::fs::create_dir_all(&quiet).unwrap();
    env.workspace(
        "w",
        &format!(
            "layout {{\n    pane name=\"a\" cwd={0:?}\n    pane name=\"b\" cwd={0:?}\n}}\n",
            quiet.display().to_string()
        ),
    );
    // An account's usage window that resets in an hour: the server wakes for it then, once.
    let usage = env.root.join("state/kraal/usage/by-id");
    std::fs::create_dir_all(&usage).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let seen = serde_json::json!({
        "five_hour": { "used_percent": 60, "resets_at": now + 3600 },
        "observed_at": now,
        "source": "statusline",
    });
    std::fs::write(usage.join("claude-work.json"), seen.to_string()).unwrap();
    env.run(&["open", "w"]);
    let client = RawClient::attach(&env, "w");
    env.wait_for_text("a", "$");
    env.wait_for_text("b", "$");
    client.wait_for_rows("the account's usage", |rows| {
        rows.windows(2).any(|r| {
            r[0].contains("Claude · work") && r[1].split('│').nth(1).is_some_and(|detail| detail.trim() == "60% used")
        })
    });
    let pid = env.server_pid().expect("the server's pid");
    // The times each of its threads went to sleep (not those another program took the processor
    // from them while they ran: that depends on the machine's load).
    let sleeps = || -> std::collections::HashMap<std::ffi::OsString, u64> {
        let tasks = std::fs::read_dir(format!("/proc/{pid}/task")).unwrap();
        tasks
            .flatten()
            .filter_map(|t| {
                let status = std::fs::read_to_string(t.path().join("status")).ok()?;
                let line = status.lines().find(|l| l.starts_with("voluntary_ctxt_switches"))?;
                Some((t.file_name(), line.split_whitespace().last()?.parse().ok()?))
            })
            .collect()
    };
    // Past the requests above, and what they started.
    std::thread::sleep(Duration::from_secs(4));
    let start = Instant::now();
    let woke: u64 = loop {
        let before = sleeps();
        std::thread::sleep(Duration::from_secs(6));
        let after = sleeps();
        // A thread that ended meanwhile was still finishing what those requests started, as on
        // a loaded machine: the server was not idle yet, and is looked at again.
        if before.keys().all(|thread| after.contains_key(thread)) {
            break after.iter().map(|(thread, now)| now - before.get(thread).copied().unwrap_or(0)).sum();
        }
        assert!(start.elapsed() < Duration::from_secs(60), "the server's threads keep ending: {after:?}");
    };
    // At most its periodic work, and what that starts: not the readers of the panes, nor
    // anything else.
    assert!(woke <= 6, "{woke} wake-ups in 6 s");
    // Still there when something happens.
    env.run(&["send", "-p", "a", "-e", "echo still-awake"]);
    env.wait_for_text("a", "still-awake");
    drop(client);
}

#[test]
fn worktrees_are_prepared_numbered_and_pruned_from_the_command_line() {
    let env = Env::new("wtcli");
    let repo = env.root.join("app");
    let git = git_repo(
        &repo,
        &[
            (".gitignore", ".env*\nsetup.log\n"),
            (
                ".kraal/worktree.toml",
                "copy = [\".env*\"]\nsetup = ['echo \"$KRAAL_WORKTREE #$KRAAL_WORKTREE_INDEX on $PORT\" > setup.log']\n",
            ),
        ],
    );
    std::fs::write(repo.join(".env"), "A=1\n").unwrap();
    std::fs::write(env.root.join("secret"), "private\n").unwrap();
    std::os::unix::fs::symlink(env.root.join("secret"), repo.join(".env.prod")).unwrap();
    let config = env.root.join("config/kraal/config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("\n[worktrees]\nport_base = 3000\n");
    std::fs::write(&config, text).unwrap();
    let wt = |args: &[&str]| env.cmd(&[&["wt"], args].concat()).current_dir(&repo).output().unwrap();
    let text =
        |out: &Output| format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let dir = env.root.join("app.worktrees");

    // Not approved yet, and nobody to ask: nothing is made.
    let out = wt(&["new", "one"]);
    assert!(!out.status.success() && text(&out).contains("`kraal wt trust`"), "{}", text(&out));
    assert!(!dir.join("one").exists());
    // Left as git makes it.
    assert!(wt(&["new", "one", "--no-setup"]).status.success());
    assert!(!dir.join("one/.env").exists());
    // Approved: the files are copied, never through a link out of the repository, and the
    // commands run with the worktree's number and port.
    assert!(wt(&["trust"]).status.success());
    let out = wt(&["new", "two", "--yes"]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(std::fs::read_to_string(dir.join("two/setup.log")).unwrap(), "two #2 on 3020\n");
    assert_eq!(std::fs::read_to_string(dir.join("two/.env")).unwrap(), "A=1\n");
    assert!(!dir.join("two/.env.prod").exists());
    assert!(text(&out).contains("not copied: .env.prod (a link out of the repository)"), "{}", text(&out));
    // That version runs again without asking.
    let out = wt(&["new", "three"]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(std::fs::read_to_string(dir.join("three/setup.log")).unwrap(), "three #3 on 3030\n");
    // Changed: asked again. A failing command fails the command; the worktree stays.
    std::fs::write(repo.join(".kraal/worktree.toml"), "setup = [\"exit 3\"]\n").unwrap();
    git(&["commit", "-q", "-am", "fail"]);
    assert!(!wt(&["new", "four"]).status.success());
    assert!(!dir.join("four").exists());
    assert!(wt(&["trust"]).status.success());
    let out = wt(&["new", "four", "--yes"]);
    assert!(!out.status.success() && text(&out).contains("the setup of four failed (exit 3)"), "{}", text(&out));
    assert!(dir.join("four").exists());
    let list = String::from_utf8(wt(&["list"]).stdout).unwrap();
    assert!(list.lines().any(|l| l.starts_with("two") && l.contains("#2")), "{list}");

    // Merged and clean ones go; one with a change or a commit of its own stays, and says why.
    std::fs::write(dir.join("three/notes.txt"), "draft\n").unwrap();
    let in_two = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir.join("two"))
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    };
    std::fs::write(dir.join("two/feature.txt"), "x\n").unwrap();
    in_two(&["add", "feature.txt"]);
    in_two(&["commit", "-q", "-m", "feature"]);
    let out = wt(&["prune", "--dry-run"]);
    let said = text(&out);
    assert!(said.contains("would remove") && said.contains("kraal/one") && said.contains("kraal/four"), "{said}");
    assert!(said.contains("kept three: it has 1 uncommitted change"), "{said}");
    assert!(said.contains("kept two: it has 1 unmerged commit"), "{said}");
    assert!(dir.join("one").exists(), "a dry run removes nothing");
    let out = wt(&["prune"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(!dir.join("one").exists() && !dir.join("four").exists());
    assert!(dir.join("two/feature.txt").exists() && dir.join("three/notes.txt").exists());
    let list = String::from_utf8(wt(&["list"]).stdout).unwrap();
    assert!(!list.contains("one") && list.contains("two") && list.contains("three"), "{list}");
}

/// A repository's `.kraal/worktree.toml` is a file someone else wrote: kraal copies nothing it
/// names and runs none of its commands before this version of it is approved, and approving is
/// an act of its own (`kraal wt trust`). `--yes` asks nothing and approves nothing, with or
/// without an agent to start; `kraal wt setup` shows the file and does nothing else.
#[test]
fn a_worktree_setup_is_acted_on_only_once_approved() {
    let env = Env::new("wttrust");
    let repo = env.root.join("app");
    let ran = env.root.join("ran");
    let file = repo.join(".kraal/worktree.toml");
    let setup = |word: &str| format!("copy = [\".env\"] # \u{200b}\nsetup = [\"echo {word} >> {}\"]\n", ran.display());
    let first = setup("one");
    let committed = [(".gitignore", ".env\nnotes\n"), (".kraal/worktree.toml", first.as_str())];
    let git = git_repo(&repo, &committed);
    std::fs::write(repo.join(".env"), "A=1\n").unwrap();
    let wt_in = |repo: &Path, args: &[&str]| {
        let mut c = env.cmd(&[&["wt"], args].concat());
        let out = c.current_dir(repo).stdin(std::process::Stdio::null()).output().unwrap();
        (
            out.status.success(),
            format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)),
        )
    };
    let wt = |args: &[&str]| wt_in(&repo, args);
    let dir = env.root.join("app.worktrees");
    let store = env.root.join("state/kraal/trusted-runbooks.json");
    let read = |path: &Path| std::fs::read_to_string(path).unwrap_or_default();

    // Shown as a new worktree would get it, with what a terminal would hide written out.
    let (ok, shown) = wt(&["setup"]);
    assert!(ok && shown.contains(".kraal/worktree.toml as of main\n"), "{shown}");
    assert!(shown.contains("# \\u{200b}\n") && shown.contains("echo one >> "), "{shown}");
    assert!(!store.exists() && !dir.exists() && !ran.exists());

    // `--yes` asks nothing, and nobody answers without a terminal: neither approves.
    let refused: [&[&str]; 3] = [&["new", "x", "--yes"], &["new", "x"], &["new", "x", "--agent", "claude", "--yes"]];
    for args in refused {
        let (ok, said) = wt(args);
        assert!(!ok && said.contains("the worktree setup comes from the repository "), "{args:?}: {said}");
        assert!(said.contains("is not approved: read it (`kraal wt setup`)"), "{args:?}: {said}");
        assert!(said.contains("approve it (`kraal wt trust`)") && said.contains("--no-setup"), "{args:?}: {said}");
        assert!(!store.exists() && !dir.exists() && !ran.exists(), "{args:?}");
    }
    // Without the setup, the worktree is made, and nothing of the file is acted on.
    let (ok, said) = wt(&["new", "bare", "--no-setup", "--yes"]);
    assert!(ok && dir.join("bare").exists() && !dir.join("bare/.env").exists() && !ran.exists(), "{said}");

    // Approved as it is now: its files are copied and its commands run, without a question.
    let (ok, said) = wt(&["trust"]);
    assert!(ok && said.contains("is approved as it is now"), "{said}");
    let (ok, said) = wt(&["new", "x", "--yes"]);
    assert!(ok, "{said}");
    assert_eq!((read(&ran).as_str(), read(&dir.join("x/.env")).as_str()), ("one\n", "A=1\n"));

    // Changed since, be it only in what it copies: that version was never approved.
    std::fs::write(&file, "copy = [\".env\", \"notes\"]\n").unwrap();
    std::fs::write(repo.join("notes"), "private\n").unwrap();
    git(&["commit", "-q", "-am", "copy more"]);
    let (ok, said) = wt(&["new", "y", "--yes"]);
    assert!(!ok && said.contains("`kraal wt trust`") && !dir.join("y").exists(), "{said}");
    assert!(wt(&["trust"]).0);
    let (ok, said) = wt(&["new", "y", "--yes"]);
    assert!(ok && read(&dir.join("y/notes")) == "private\n", "{said}");

    // The file of another branch is another version: shown and approved by that branch's name.
    git(&["checkout", "-q", "-b", "other"]);
    std::fs::write(&file, setup("two")).unwrap();
    git(&["commit", "-q", "-am", "two"]);
    git(&["checkout", "-q", "main"]);
    let (ok, said) = wt(&["new", "z", "--base", "other", "--yes"]);
    assert!(!ok && said.contains("(`kraal wt setup --base other`)"), "{said}");
    assert!(said.contains("(`kraal wt trust --base other`)") && !dir.join("z").exists(), "{said}");
    let (ok, shown) = wt(&["setup", "--base", "other"]);
    assert!(ok && shown.contains("worktree.toml as of other\n") && shown.contains("echo two >> "), "{shown}");
    assert!(wt(&["trust", "--base", "other"]).0);
    let (ok, said) = wt(&["new", "z", "--base", "other", "--yes"]);
    assert!(ok && read(&ran) == "one\ntwo\n", "{said}");

    // A repository without the file has nothing to approve.
    let plain = env.root.join("plain");
    let _ = git_repo(&plain, &[("a.txt", "1\n")]);
    let (ok, said) = wt_in(&plain, &["setup"]);
    assert!(ok && said.starts_with("no ") && said.contains(".kraal/worktree.toml as of main"), "{said}");
    let (ok, said) = wt_in(&plain, &["trust"]);
    assert!(ok && said.contains("nothing to approve"), "{said}");
    let (ok, said) = wt_in(&plain, &["new", "n", "--yes"]);
    assert!(ok && env.root.join("plain.worktrees/n").exists(), "{said}");
}

/// In a terminal, `kraal wt new` shows the repository's setup, every command of it, and asks
/// whether to trust it before anything is made; `--yes` there asks nothing and approves
/// nothing.
#[cfg(target_os = "linux")]
#[test]
fn a_worktree_setup_is_approved_in_a_terminal_by_a_question_that_says_so() {
    let env = Env::new("wtask");
    let repo = env.root.join("app");
    let ran = env.root.join("ran");
    let setup = format!("setup = [\"echo ran >> {}\"]\n", ran.display());
    let _ = git_repo(&repo, &[(".kraal/worktree.toml", &setup)]);
    let dir = env.root.join("app.worktrees");
    let store = env.root.join("state/kraal/trusted-runbooks.json");
    let new = |args: &[&str], typed: &[u8]| {
        let mut c = env.cmd(&[&["wt", "new"], args].concat());
        c.current_dir(&repo);
        in_terminal(c, typed)
    };

    let shown = new(&["x"], b"n\n");
    assert!(
        shown.contains("Trust the worktree setup of the repository ") && shown.contains("  1. echo ran >> "),
        "{shown}"
    );
    assert!(shown.contains("Trust this version and prepare the worktree with it? [y/N]"), "{shown}");
    assert!(shown.contains("cancelled; nothing was created"), "{shown}");
    assert!(!dir.exists() && !ran.exists() && !store.exists());
    let shown = new(&["x", "--yes"], b"y\n");
    assert!(shown.contains("is not approved") && !shown.contains("[y/N]"), "{shown}");
    assert!(!dir.exists() && !ran.exists() && !store.exists());
    let shown = new(&["x"], b"y\n");
    assert!(shown.contains("[y/N]") && dir.join("x").exists() && ran.exists() && store.exists(), "{shown}");
    // Approved: the next one is made without a question.
    let shown = new(&["y"], b"");
    assert!(!shown.contains("Trust") && dir.join("y").exists(), "{shown}");
}

/// A worktree's setup runs once, when the worktree is made. A session restarted while it runs
/// brings the agent's pane back and runs nothing of the repository's again.
#[cfg(target_os = "linux")]
#[test]
fn a_worktree_setup_is_not_run_again_after_a_restart() {
    let env = Env::new("wtrestart");
    let repo = env.root.join("app");
    let ran = env.root.join("ran");
    let setup = format!("setup = [\"echo started >> {}\", \"sleep 30\"]\n", ran.display());
    let _ = git_repo(&repo, &[(".kraal/worktree.toml", &setup)]);
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(&bin.join("claude"), "#!/bin/sh\necho agent started\nexec sleep 100\n");
    let home = env.root.join("alpha-home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        env.root.join("config/kraal/accounts.toml"),
        format!(
            "[[account]]\nname = \"alpha\"\nagent = \"claude\"\nhome = {:?}\n\n[[rule]]\nworkspace = \"w\"\naccounts = [\"beta\"]\n\n[[account]]\nname=\"beta\"\nagent=\"claude\"\nhome={:?}\n",
            home.display().to_string(), env.root.join("beta-home").display().to_string()
        ),
    )
    .unwrap();
    env.workspace("w", &format!("layout {{\n    pane name=\"shell\" cwd={:?}\n}}\n", repo.display().to_string()));
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let open = || {
        let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    };
    open();
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_screen("⎇ main");
    client.send(b"\x02a");
    client.wait_for_row("Claude · alpha");
    client.send(b"\x17");
    client.wait_for_screen("Name of the worktree");
    client.send(b"feat\r");
    client.wait_for_screen("Trust the worktree setup");
    client.send(b"y");
    let started = || std::fs::read_to_string(&ran).unwrap_or_default();
    wait_until(Duration::from_secs(15), "the setup never started", || started() == "started\n");

    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let agent = panes.as_array().unwrap().iter().find(|p| p["name"] == "claude feat").unwrap();
    assert_eq!(agent["account"], "claude:alpha", "the selected account overrides the beta rule");

    // Stopped while the setup runs, and started again: the pane is back, the setup is not.
    env.run(&["kill"]);
    open();
    wait_until(Duration::from_secs(10), "the agent's pane did not come back", || {
        env.run(&["panes"]).contains("claude feat")
    });
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(started(), "started\n", "the setup ran again");
}

/// From the agent picker: the worktree is made once its repository's setup is approved, the
/// setup runs in the agent's pane, and the agent starts there with the worktree's number. A
/// setup that fails leaves the agent waiting for Enter; a no makes nothing. Merged worktrees
/// are then removed in one go, their panes closed.
#[cfg(target_os = "linux")]
#[test]
fn an_agent_worktree_is_prepared_in_its_pane_once_trusted() {
    let env = Env::new("wtsetup");
    let repo = env.root.join("app");
    let git = git_repo(
        &repo,
        &[
            (".gitignore", ".env\n"),
            (
                ".kraal/worktree.toml",
                "copy = [\".env\"]\nsetup = ['echo \"preparing $KRAAL_WORKTREE\"', \"test -f .env\"]\n",
            ),
        ],
    );
    std::fs::write(repo.join(".env"), "A=1\n").unwrap();
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(
        &bin.join("claude"),
        "#!/bin/sh\necho \"agent in $KRAAL_WORKTREE #$KRAAL_WORKTREE_INDEX port $PORT\"\nexec sleep 100\n",
    );
    let home = env.root.join("alpha-home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        env.root.join("config/kraal/accounts.toml"),
        format!(
            "[[account]]\nname = \"alpha\"\nagent = \"claude\"\nhome = {:?}\n\n[[rule]]\nworkspace = \"w\"\naccounts = [\"alpha\"]\n",
            home.display().to_string()
        ),
    )
    .unwrap();
    let config = env.root.join("config/kraal/config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("\n[worktrees]\nport_base = 3000\n");
    std::fs::write(&config, text).unwrap();
    env.workspace("w", &format!("layout {{\n    pane name=\"shell\" cwd={:?}\n}}\n", repo.display().to_string()));
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mut client = RawClient::attach(&env, "w");
    // The agent picker offers worktrees once git has seen the repository.
    client.wait_for_screen("⎇ main");
    let new_worktree = |client: &mut RawClient, name: &str| {
        env.run(&["focus", "shell"]);
        client.screen.lock().unwrap().clear();
        client.send(b"\x02a");
        client.wait_for_row("Claude · alpha");
        client.send(b"\x17");
        client.wait_for_screen("Name of the worktree");
        client.send(name.as_bytes());
        client.send(b"\r");
        client.wait_for_screen("Trust the worktree setup");
    };
    let dir = env.root.join("app.worktrees");
    // The pane comes once git has made the worktree.
    let wait_for = |pane: &str, text: &str| -> String {
        let start = Instant::now();
        loop {
            let out = env.try_run(&["read", "-p", pane]);
            let screen = String::from_utf8_lossy(&out.stdout).into_owned();
            if out.status.success() && screen.contains(text) {
                return screen;
            }
            assert!(start.elapsed() < Duration::from_secs(15), "timed out waiting for {text:?} in {pane}:\n{screen}");
            std::thread::sleep(Duration::from_millis(50));
        }
    };

    new_worktree(&mut client, "feat");
    assert!(!dir.join("feat").exists(), "nothing is made before the answer");
    client.send(b"y");
    let screen = wait_for("claude feat", "agent in feat #1 port 3010");
    assert!(screen.contains("preparing feat") && screen.contains("copied from the main checkout: .env"), "{screen}");

    std::fs::write(repo.join(".kraal/worktree.toml"), "setup = [\"echo about to fail\", \"exit 3\"]\n").unwrap();
    git(&["commit", "-q", "-am", "fail"]);
    new_worktree(&mut client, "bad");
    client.send(b"y");
    let screen = wait_for("claude bad", "Enter starts this anyway");
    assert!(screen.contains("about to fail") && !screen.contains("agent in"), "{screen}");
    env.run(&["focus", "claude bad"]);
    client.send(b"\r");
    wait_for("claude bad", "agent in bad #2 port 3020");

    std::fs::write(repo.join(".kraal/worktree.toml"), "setup = [\"echo changed\"]\n").unwrap();
    git(&["commit", "-q", "-am", "changed"]);
    new_worktree(&mut client, "no");
    client.send(b"n");
    client.wait_for_row("the worktree no was not created");
    assert!(!dir.join("no").exists());

    // Both worktrees have nothing to lose: they go, with their agents' panes.
    env.run(&["focus", "shell"]);
    client.screen.lock().unwrap().clear();
    client.send(b"\x02Rg");
    client.wait_for_screen("Remove merged worktrees");
    client.send(b"\r");
    client.wait_for_screen("Closes these panes");
    client.send(b"y");
    let start = Instant::now();
    while dir.join("feat").exists() || dir.join("bad").exists() {
        assert!(start.elapsed() < Duration::from_secs(15), "the merged worktrees are still there");
        std::thread::sleep(Duration::from_millis(100));
    }
    let panes = env.run(&["panes"]);
    assert!(!panes.contains("claude feat") && !panes.contains("claude bad"), "{panes}");

    // From the command line inside the session: the version declined above is refused there
    // too, `--yes` or not; approved, the worktree is prepared there, and the agent opened in the
    // session gets the worktree's number (the first, free again).
    let socket = env.root.join("run/kraal/wtsetup.sock");
    let new_cli = || {
        env.cmd(&["wt", "new", "cli", "--agent", "claude", "--yes"])
            .env("PATH", &path)
            .env("KRAAL_SOCKET", &socket)
            .current_dir(&repo)
            .output()
            .unwrap()
    };
    let out = new_cli();
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("`kraal wt trust`"));
    assert!(!dir.join("cli").exists());
    let trust = || {
        let out = env.cmd(&["wt", "trust"]).current_dir(&repo).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    };
    trust();
    let out = new_cli();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let said = String::from_utf8(out.stdout).unwrap();
    assert!(said.contains("changed"), "the setup ran there");
    // Its last line is the id of the agent's pane.
    let pane = said.lines().last().unwrap();
    assert!(pane.parse::<u32>().is_ok(), "{said}");
    wait_for(pane, "agent in cli #1 port 3010");

    // Files to copy alone are asked about as well: the repository names them.
    std::fs::write(repo.join(".kraal/worktree.toml"), "copy = [\".env\"]\n").unwrap();
    git(&["commit", "-q", "-am", "copy only"]);
    new_worktree(&mut client, "copy");
    client.wait_for_screen("it copies from the main checkout: .env");
    client.send(b"n");
    client.wait_for_row("the worktree copy was not created");
    assert!(!dir.join("copy").exists());
    // Approved from the command line: the picker asks nothing more, and the file is copied.
    trust();
    env.run(&["focus", "shell"]);
    client.screen.lock().unwrap().clear();
    client.send(b"\x02a");
    client.wait_for_row("Claude · alpha");
    client.send(b"\x17");
    client.wait_for_screen("Name of the worktree");
    client.send(b"copied\r");
    wait_for("claude copied", "agent in copied #2 port 3020");
    assert_eq!(std::fs::read_to_string(dir.join("copied/.env")).unwrap(), "A=1\n");
}

/// The question that asks to trust a repository's worktree setup shows all of it, however long:
/// what it copies, and every line of each command with its spacing. Reading it makes nothing,
/// and Enter cancels.
#[cfg(target_os = "linux")]
#[test]
fn all_of_a_worktree_setup_can_be_read_before_it_is_trusted() {
    let env = Env::new("wtread");
    let repo = env.root.join("app");
    let commands: Vec<String> =
        (1..=12).map(|i| format!("if test -d src; then\n    echo  prepare {i}\n\nfi")).collect();
    let quoted: Vec<String> = commands.iter().map(|command| serde_json::json!(command).to_string()).collect();
    let setup = format!("copy = [\".env\", \"config/*.local\"]\nsetup = [{}]\n", quoted.join(", "));
    let _ = git_repo(&repo, &[(".kraal/worktree.toml", &setup)]);
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(&bin.join("claude"), "#!/bin/sh\nexec sleep 100\n");
    let home = env.root.join("alpha-home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        env.root.join("config/kraal/accounts.toml"),
        format!(
            "[[account]]\nname = \"alpha\"\nagent = \"claude\"\nhome = {:?}\n\n[[rule]]\nworkspace = \"w\"\naccounts = [\"alpha\"]\n",
            home.display().to_string()
        ),
    )
    .unwrap();
    env.workspace("w", &format!("layout {{\n    pane name=\"shell\" cwd={:?}\n}}\n", repo.display().to_string()));
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let commands = commands.iter().enumerate().flat_map(|(i, command)| numbered(i + 1, command));
    let expected = without_trailing_spaces(commands.collect());

    for (size, key) in [((124, 36), &b"\x1b[6~"[..]), ((80, 24), b"\x1b[B")] {
        let mut client = RawClient::attach_sized(&env, Some("w"), size);
        if size.0 == 124 {
            // The agent picker offers worktrees once git has seen the repository.
            client.wait_for_screen("⎇ main");
        }
        env.run(&["focus", "shell"]);
        client.send(b"\x02a");
        client.wait_for_row("Claude · alpha");
        client.send(b"\x17");
        client.wait_for_screen("Name of the worktree");
        client.send(b"feat\r");
        client.wait_for_row("Trust the worktree setup");
        let lines = client.read_question(key);
        let runs = lines.iter().position(|line| line == "it runs there:").unwrap();
        assert_eq!(lines[runs - 1], "it copies from the main checkout: .env, config/*.local", "at {size:?}");
        assert_eq!(lines[runs + 1..], expected[..], "at {size:?}");
        // Cancel is the button selected.
        client.send(b"\r");
        client.wait_for_row("the worktree feat was not created");
        assert!(!env.root.join("app.worktrees").exists(), "reading the question made something");
        assert!(!env.root.join("state/kraal/trusted-runbooks.json").exists(), "Enter approved it");
    }
}

#[test]
fn closing_or_adding_tabs_keeps_the_one_shown() {
    let env = Env::new("tabs");
    env.workspace(
        "w",
        "layout {\n    tab name=\"a\" {\n        pane name=\"a\"\n    }\n    tab name=\"b\" {\n        pane name=\"b\"\n    }\n    \
         tab name=\"c\" {\n        pane name=\"c\"\n    }\n}\n",
    );
    env.run(&["open", "w"]);
    env.run(&["focus", "b"]);
    let focused = || -> String {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let shown = panes.as_array().unwrap().iter().find(|p| p["focused"] == true).cloned();
        shown.and_then(|p| p["name"].as_str().map(String::from)).unwrap_or_default()
    };
    assert_eq!(focused(), "b");
    env.run(&["close", "a"]);
    assert_eq!(focused(), "b", "closing an earlier tab keeps the one shown");
    env.run(&["run", "--tab", "--no-focus", "--", "sleep", "60"]);
    assert_eq!(focused(), "b", "a new tab that must not take the focus does not");
}

#[test]
fn an_exit_is_seen_even_when_something_left_behind_keeps_printing() {
    let env = Env::new("chatty");
    env.workspace("w", "layout {\n    pane name=\"first\"\n}\n");
    env.run(&["open", "w"]);
    // The command ends at once; what it left in the background, deaf to the hangup, prints on
    // for as long as the terminal lets it.
    env.run(&["run", "-n", "chatty", "--", "sh", "-c", "(trap '' HUP; exec yes kraal) & exit 3"]);
    let start = Instant::now();
    env.run(&["wait", "-p", "chatty", "--until", "exit", "--timeout", "15"]);
    assert!(start.elapsed() < Duration::from_secs(10), "the exit took {:?}", start.elapsed());
    let json = env.run(&["panes", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let chatty = v.as_array().unwrap().iter().find(|p| p["name"] == "chatty").cloned().unwrap();
    assert_eq!(chatty["exit_code"], 3, "{json}");
    // The server still answers, and closing the pane hangs up what was left.
    env.run(&["close", "chatty"]);
    assert!(env.run(&["panes"]).contains("first"));
}

/// `name.ext:123` reads as a file and its line, or as a host and its port. In a local pane the
/// file is looked for: a host and its port is offered to open, never as a file to edit, and a
/// file that is there opens in the editor at its line.
#[test]
fn hints_tell_a_host_and_its_port_from_a_file_and_its_line() {
    let env = Env::new("hostport");
    let dir = env.root.join("proj");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("notes.md"), "a file\n").unwrap();
    std::fs::write(dir.join("seen"), "redis.lan:6379\nnotes.md:12\n").unwrap();
    // An editor that says what it was given.
    let (editor, edited) = (env.root.join("editor"), env.root.join("edited"));
    write_script(&editor, &format!("#!/bin/sh\necho \"$@\" > {}\n", edited.display()));
    let config = env.root.join("config/kraal/config.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    // Labels in the order of the screen, from the bottom: x for the last line, y above it.
    std::fs::write(&config, format!("{text}[hints]\nalphabet = \"xy\"\n")).unwrap();
    env.workspace("w", &format!("layout {{\n    pane name=\"shell\" cwd={:?}\n}}\n", dir.display().to_string()));
    let out = env.cmd(&["open", "w"]).env("VISUAL", &editor).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    env.run(&["send", "-p", "shell", "--enter", "cat seen"]);
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_row("notes.md:12");
    let act = |client: &mut RawClient, label: &[u8]| {
        client.send(b"\x02o");
        client.wait_for_row("HINTS: COPY");
        client.send(b"\t");
        client.wait_for_row("HINTS: ACT");
        client.send(label);
    };

    act(&mut client, b"y");
    client.wait_for_message("http://redis.lan:6379");
    std::thread::sleep(Duration::from_millis(300));
    assert!(!edited.exists(), "an editor opened on {:?}", std::fs::read_to_string(&edited));
    act(&mut client, b"x");
    wait_until(Duration::from_secs(10), "the editor never opened", || {
        std::fs::read_to_string(&edited).is_ok_and(|args| args.trim() == "+12 notes.md")
    });
}

#[cfg(unix)]
#[test]
fn tunnels_are_up_when_ssh_listens_and_survive_a_restart() {
    let env = Env::new("tunnels");
    // A fake ssh: with "ok" in the destination, says it listens and makes its listener (as
    // `ssh -v` does); with "taken", says it listens but cannot bind (another program has the
    // port); else says nothing. Then it stays, as a tunnel does. Its pid goes in a file.
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let pids = env.root.join("ssh-pids");
    write_script(
        &bin.join("ssh"),
        &format!(
            "#!/bin/sh\nprev=; for a; do [ \"$prev\" = -L ] && fwd=$a; prev=$a; dest=$a; done\n\
             port=$(echo \"$fwd\" | cut -d: -f2)\necho $$ >> {}\n\
             case $dest in *ok*|*taken*) echo \"debug1: Local forwarding listening on 127.0.0.1 port $port.\" >&2;; esac\n\
             case $dest in *ok*) echo \"debug1: channel 0: new port-listener [port listener] (inactive timeout: 0)\" >&2;;\n\
             *taken*) echo \"bind [127.0.0.1]:$port: Address already in use\" >&2;; esac\n\
             exec sleep 600\n",
            pids.display()
        ),
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let run = |args: &[&str]| {
        let out = env.cmd(args).env("PATH", &path).output().unwrap();
        assert!(out.status.success(), "kraal {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let free = || std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port();
    let state_of = |port: u16| {
        run(&["tunnel", "list"])
            .lines()
            .find(|l| l.ends_with(&format!("localhost:{port}")))
            .map(|l| l.split_whitespace().next().unwrap_or("").to_string())
    };
    let wait_state = |port: u16, want: &str| {
        let start = Instant::now();
        while state_of(port).as_deref() != Some(want) {
            assert!(start.elapsed() < Duration::from_secs(10), "{port}: {:?}", run(&["tunnel", "list"]));
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    run(&["open", "w"]);

    let (up, quiet) = (free(), free());
    run(&["tunnel", "add", "ok@a.lan", "5432", "--local", &up.to_string()]);
    wait_state(up, "up");
    // Another program listening on the port of a tunnel whose ssh is still connecting does not
    // make the tunnel up.
    run(&["tunnel", "add", "quiet@b.lan", "5433", "--local", &quiet.to_string()]);
    let squatter = std::net::TcpListener::bind(("127.0.0.1", quiet)).unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(state_of(quiet).as_deref(), Some("connecting"));
    drop(squatter);
    // ssh announcing its listener is not enough: the bind may fail after it.
    let taken = free();
    run(&["tunnel", "add", "taken@e.lan", "5434", "--local", &taken.to_string()]);
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(state_of(taken).as_deref(), Some("connecting"));
    // Two tunnels without a local port never get the same one, even before either listens.
    let wanted = free();
    run(&["tunnel", "add", "quiet@c.lan", &wanted.to_string()]);
    run(&["tunnel", "add", "quiet@d.lan", &wanted.to_string()]);
    let list = run(&["tunnel", "list"]);
    assert_eq!(list.matches(&format!("localhost:{wanted}")).count(), 1, "{list}");

    // Closing a tunnel stops its ssh.
    let before: Vec<i32> =
        std::fs::read_to_string(&pids).unwrap().lines().filter_map(|l| l.trim().parse().ok()).collect();
    run(&["tunnel", "rm", &quiet.to_string()]);
    let alive = |pid: i32| unsafe { libc::kill(pid, 0) } == 0;
    let start = Instant::now();
    while before.iter().filter(|p| alive(**p)).count() > 4 {
        assert!(start.elapsed() < Duration::from_secs(5), "the closed tunnel's ssh still runs");
        std::thread::sleep(Duration::from_millis(20));
    }
    // The local port is what names a tunnel, and the list shows it: of two tunnels to the same
    // port of two servers, each is closed by its own. A tunnel has no other name.
    let to_wanted = |list: &str| -> Vec<(String, u16)> {
        let lines = list.lines().filter(|l| l.contains(&format!(":{wanted} → localhost:")));
        lines
            .map(|l| {
                let (server, local) = l.split_once(" → localhost:").unwrap();
                (server.split_whitespace().last().unwrap().to_string(), local.trim().parse().unwrap())
            })
            .collect()
    };
    let both = to_wanted(&run(&["tunnel", "list"]));
    assert_eq!(both.len(), 2, "{both:?}");
    let out = env.cmd(&["tunnel", "rm", "#1"]).env("PATH", &path).output().unwrap();
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success() && said.contains("not a local port: #1") && said.contains("kraal tunnel list"),
        "{said}"
    );
    run(&["tunnel", "rm", &both[1].1.to_string()]);
    assert_eq!(to_wanted(&run(&["tunnel", "list"])), [both[0].clone()]);

    // A tunnel through a connection kraal knows by no name comes back through that connection.
    run(&["kill"]);
    run(&["open", "w"]);
    wait_state(up, "up");
    let list = run(&["tunnel", "list"]);
    assert!(list.contains(&format!("ok@a.lan:5432 → localhost:{up}")), "{list}");
}

#[cfg(unix)]
#[test]
fn an_ssh_pane_keeps_the_account_it_was_opened_with() {
    let env = Env::new("account");
    // A fake ssh: logs its arguments; runs a remote script here, and ends at once otherwise (so
    // kraal never sees an ssh process in the pane, only what it started).
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = env.root.join("ssh.log");
    write_script(
        &bin.join("ssh"),
        &format!(
            "#!/bin/sh\necho \"$*\" >> {}\nfor a; do cmd=$a; done\ncase $cmd in *' '*) exec sh -c \"$cmd\";; esac\nexit 0\n",
            log.display()
        ),
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    // The inventory's `web` is root's.
    std::fs::write(
        env.root.join("config/kraal/servers.toml"),
        "[[server]]\nname = \"web\"\nhost = \"web.lan\"\nuser = \"root\"\n",
    )
    .unwrap();
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    let run = |args: &[&str], pane: Option<&str>| {
        let mut c = env.cmd(args);
        c.env("PATH", &path);
        if let Some(p) = pane {
            c.env("KRAAL_PANE", p).env("KRAAL_SESSION", &env.session);
        }
        let out = c.output().unwrap();
        assert!(out.status.success(), "kraal {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    run(&["open", "w"], None);
    run(&["ssh", "deploy@web.lan", "--tab"], None);
    let panes: serde_json::Value = serde_json::from_str(&run(&["panes", "--json"], None)).unwrap();
    let pane =
        panes.as_array().unwrap().iter().find(|p| p["server"] == "web").unwrap_or_else(|| panic!("{panes}"))["id"]
            .to_string();
    // A runbook on that pane (whose ssh already ended) goes as deploy, not as the inventory's root.
    let file = env.root.join("config/kraal/runbooks/where.toml");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "on = \"pane\"\nsteps = [\"echo here\"]\n").unwrap();
    let out = run(&["runbook", "run", "where", "--yes", "--wait"], Some(&pane));
    assert!(out.contains("finished: 1 ok"), "{out}");
    let log = std::fs::read_to_string(&log).unwrap();
    let remote = log.lines().filter(|l| l.contains("sh -c")).collect::<Vec<_>>();
    assert!(!remote.is_empty() && remote.iter().all(|l| l.contains("deploy@web.lan")), "{log}");
}

#[test]
fn a_long_paste_into_production_is_asked_about_and_sent_whole() {
    let env = Env::new("prodpaste");
    let tf = env.root.join("infra");
    std::fs::create_dir_all(tf.join(".terraform")).unwrap();
    std::fs::write(tf.join(".terraform/environment"), "prod\n").unwrap();
    let got = env.root.join("pasted");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    // A program that takes 2 MB as they come (no line editing), then says it has them all.
    let script = format!("stty raw -echo; head -c 2000000 > {}; echo; echo GOT-ALL; sleep 60", got.display());
    env.run(&["run", "--cwd", &tf.display().to_string(), "--name", "ops", "--", "sh", "-c", &script]);
    wait_production(&env, "ops");
    env.run(&["focus", "ops"]);
    let mut client = RawClient::attach(&env, "w");
    // 20000 lines, more than the pieces a paste is passed on in.
    let data = ("x".repeat(99) + "\n").repeat(20000);
    client.paste(data.as_bytes());
    // One question about all of it, and nothing sent before the answer.
    client.wait_for_screen("Paste 20000 lines into production");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(std::fs::metadata(&got).map(|m| m.len()).unwrap_or(0), 0);
    client.send(b"y");
    env.wait_for_text("ops", "GOT-ALL");
    assert_eq!(std::fs::metadata(&got).unwrap().len(), 2_000_000);
}

#[test]
fn a_long_paste_keeps_going_where_it_started_and_never_into_production() {
    let env = Env::new("streampaste");
    let tf = env.root.join("infra");
    std::fs::create_dir_all(tf.join(".terraform")).unwrap();
    std::fs::write(tf.join(".terraform/environment"), "prod\n").unwrap();
    let (got, prod_got) = (env.root.join("pasted"), env.root.join("prod-pasted"));
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let take = |file: &Path| format!("stty raw -echo; cat > {}", file.display());
    env.run(&["run", "--cwd", &tf.display().to_string(), "--name", "ops", "--", "sh", "-c", &take(&prod_got)]);
    wait_production(&env, "ops");
    env.run(&["run", "--name", "sink", "--", "sh", "-c", &take(&got)]);
    env.run(&["focus", "sink"]);
    let mut client = RawClient::attach(&env, "w");
    // More than kraal holds back (8 MiB): it goes out as it comes.
    let line = "x".repeat(99) + "\n";
    client.send(b"\x1b[200~");
    for chunk in line.repeat(90_000).as_bytes().chunks(64 << 10) {
        client.send(chunk);
    }
    let size = |f: &Path| std::fs::metadata(f).map(|m| m.len()).unwrap_or(0);
    let start = Instant::now();
    while size(&got) == 0 {
        assert!(start.elapsed() < Duration::from_secs(10), "the paste never started going out");
        std::thread::sleep(Duration::from_millis(50));
    }
    // The focus moves to a production pane while the rest comes: the rest still goes where the
    // paste started, never there.
    env.run(&["focus", "ops"]);
    for chunk in line.repeat(10_000).as_bytes().chunks(64 << 10) {
        client.send(chunk);
    }
    client.send(b"\x1b[201~");
    let start = Instant::now();
    while size(&got) < 10_000_000 {
        assert!(start.elapsed() < Duration::from_secs(20), "only {} bytes arrived", size(&got));
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(size(&got), 10_000_000);
    assert_eq!(size(&prod_got), 0, "nothing of the paste reached production");
}

#[test]
fn a_paste_held_until_its_end_goes_where_it_started() {
    let env = Env::new("heldpaste");
    let (got, other_got) = (env.root.join("pasted"), env.root.join("other-pasted"));
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let take = |file: &Path| format!("stty raw -echo; cat > {}", file.display());
    env.run(&["run", "--name", "sink", "--", "sh", "-c", &take(&got)]);
    env.run(&["run", "--name", "other", "--", "sh", "-c", &take(&other_got)]);
    env.run(&["focus", "sink"]);
    let mut client = RawClient::attach(&env, "w");
    // Less than kraal holds back (8 MiB): it goes whole once it ends.
    let line = "x".repeat(99) + "\n";
    client.send(b"\x1b[200~");
    for chunk in line.repeat(10_000).as_bytes().chunks(64 << 10) {
        client.send(chunk);
    }
    std::thread::sleep(Duration::from_millis(300));
    // The focus moves before its end: all of it still goes where it started.
    env.run(&["focus", "other"]);
    client.send(b"end\x1b[201~");
    let size = |f: &Path| std::fs::metadata(f).map(|m| m.len()).unwrap_or(0);
    let start = Instant::now();
    while size(&got) < 1_000_003 && size(&other_got) == 0 {
        assert!(start.elapsed() < Duration::from_secs(10), "only {} bytes arrived", size(&got));
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!((size(&got), size(&other_got)), (1_000_003, 0));
}

#[test]
fn copy_mode_releases_the_button_the_program_saw_pressed() {
    let env = Env::new("copyrelease");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    // A program that asks for SGR mouse reports and shows what it gets.
    env.run(&["run", "--name", "app", "--", "sh", "-c", "stty raw -echo; printf '\\033[?1000h\\033[?1006h'; cat -v"]);
    env.run(&["focus", "app"]);
    std::thread::sleep(Duration::from_millis(300));
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_screen("app");
    // The right button goes down over the program, copy mode starts (ctrl+b v), the wheel
    // turns: the program is told the right button is up, and nothing more.
    client.send(b"\x1b[<2;110;20M");
    env.wait_for_text("app", "<2;");
    client.send(b"\x02v");
    std::thread::sleep(Duration::from_millis(200));
    client.send(b"\x1b[<64;110;20M");
    let screen = env.wait_for_text("app", "m");
    let reports: Vec<&str> = screen.split("^[[").filter(|r| r.starts_with('<')).collect();
    assert!(reports.iter().any(|r| r.starts_with("<2;") && r.trim_end().ends_with('m')), "{screen}");
    assert!(!reports.iter().any(|r| r.starts_with("<0;")), "{screen}");
}

#[test]
fn a_pane_is_told_when_the_terminal_loses_and_regains_the_focus() {
    let env = Env::new("focusreports");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    // A program that asks for focus reports and shows what it gets.
    env.run(&["run", "--name", "app", "--", "sh", "-c", "stty raw -echo; printf '\\033[?1004h'; cat -v"]);
    std::thread::sleep(Duration::from_millis(300));
    let mut client = RawClient::attach(&env, "w");
    env.wait_for_text("app", "^[[I");
    // Out and back in, in one read: both are told, in order.
    client.send(b"\x1b[O\x1b[I");
    env.wait_for_text("app", "^[[I^[[O^[[I");
    // The same when the focus moves away and back by commands.
    env.run(&["focus", "main"]);
    env.run(&["focus", "app"]);
    env.wait_for_text("app", "^[[I^[[O^[[I^[[O^[[I");
}

/// A terminal of `cols × rows` after `bytes`.
fn emulate(
    bytes: &[u8],
    cols: usize,
    rows: usize,
) -> alacritty_terminal::Term<alacritty_terminal::event::VoidListener> {
    use alacritty_terminal::term::{Config, Term, test::TermSize};
    let mut term = Term::new(Config::default(), &TermSize::new(cols, rows), alacritty_terminal::event::VoidListener);
    let mut parser: alacritty_terminal::vte::ansi::Processor = alacritty_terminal::vte::ansi::Processor::new();
    parser.advance(&mut term, bytes);
    term
}

/// The text of each row a terminal of `cols × rows` shows after `bytes`.
fn text_after(bytes: &[u8], cols: usize, rows: usize) -> Vec<String> {
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::cell::Flags;
    let term = emulate(bytes, cols, rows);
    let grid = term.grid();
    (0..rows)
        .map(|y| {
            let row = &grid[Line(y as i32)];
            (0..cols)
                .map(|x| &row[Column(x)])
                .filter(|c| !c.flags.contains(Flags::WIDE_CHAR_SPACER))
                .map(|c| c.c)
                .collect()
        })
        .collect()
}

/// What a terminal of `cols × rows` shows after `bytes`: each row's cells (text, colours,
/// attributes, link), the cursor (its place, its shape, whether it blinks) and the modes the
/// terminal is in (the cursor shown, what it reports of the mouse and of the keys...).
fn terminal_after(bytes: &[u8], cols: usize, rows: usize) -> Vec<String> {
    use alacritty_terminal::index::{Column, Line};
    let term = emulate(bytes, cols, rows);
    let grid = term.grid();
    let mut out: Vec<String> = (0..rows)
        .map(|y| {
            let row = &grid[Line(y as i32)];
            (0..cols)
                .map(|x| {
                    let c = &row[Column(x)];
                    let link = c.hyperlink().map(|h| h.uri().to_string());
                    let text: String =
                        std::iter::once(c.c).chain(c.zerowidth().into_iter().flatten().copied()).collect();
                    format!("{text:?} {:?} {:?} {:?} {:?} {link:?}", c.fg, c.bg, c.flags, c.underline_color())
                })
                .collect::<Vec<_>>()
                .join(" | ")
        })
        .collect();
    let cursor = grid.cursor.point;
    out.push(format!("cursor {} {} {:?}", cursor.line.0, cursor.column.0, term.cursor_style()));
    out.push(format!("modes {:?}", term.mode()));
    out
}

impl RawClient {
    /// The text of each row its terminal shows.
    fn rows(&self) -> Vec<String> {
        let bytes = self.screen.lock().unwrap().clone();
        text_after(&bytes, self.size.0, self.size.1)
    }

    /// The text of each row its terminal showed after the first frame drawn for it.
    fn first_frame(&self) -> Vec<String> {
        let end = "\x1b[?2026l";
        self.wait_for_screen(end);
        let screen = self.screen.lock().unwrap();
        let drawn = screen.windows(end.len()).position(|w| w == end.as_bytes()).unwrap() + end.len();
        text_after(&screen[..drawn], self.size.0, self.size.1)
    }

    /// Wait until its terminal shows `text` on a row; the rows then. Unlike
    /// [`RawClient::wait_for_screen`], this finds text drawn over cells that already had some of
    /// it, which a frame leaves out.
    fn wait_for_row(&self, text: &str) -> Vec<String> {
        self.wait_for_rows(text, |rows| rows.iter().any(|r| r.contains(text)))
    }

    /// Close the overlay identified by `text` before sending more keys: Escape followed by
    /// another key before its timeout is Alt+that key, even in separate reads.
    fn close_overlay(&mut self, text: &str) {
        self.wait_for_row(text);
        self.send(b"\x1b");
        self.wait_for_rows(&format!("{text} closed"), |rows| !rows.iter().any(|r| r.contains(text)));
    }

    /// Wait until the rows its terminal shows are `what` says (`done`); the rows then.
    fn wait_for_rows(&self, what: &str, done: impl Fn(&[String]) -> bool) -> Vec<String> {
        let start = Instant::now();
        loop {
            let bytes = self.screen.lock().unwrap().clone();
            let rows = text_after(&bytes, self.size.0, self.size.1);
            if done(&rows) {
                self.trace_step_at(bytes.len(), ClientAction::Observed(what.into()));
                return rows;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "never {what}:\n{}", rows.join("\n"));
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The message of the question its terminal shows, read as someone would: from the top to
    /// the end, pressing `key` while the box says there is more below. Each line is as written,
    /// a line the box cut over several rows (marked in its margin) put back together.
    fn read_question(&mut self, key: &[u8]) -> Vec<String> {
        // The rows still below, when the message is longer than the box.
        let below = |rows: &[String]| {
            let said = rows.iter().find(|r| r.contains("more line") || r.contains("↑ top"))?;
            let count = said.split("↓ ").nth(1).and_then(|rest| rest.split(' ').next());
            Some(count.map_or(0, |n| n.parse::<usize>().unwrap()))
        };
        let mut rows: Vec<(bool, Vec<char>)> = Vec::new();
        let mut before = None;
        let mut first = None;
        loop {
            // The footer is drawn last: the box above it is whole.
            let screen = self.wait_for_rows("the question, further down", |rows| {
                rows.iter().any(|r| r.contains("Enter Cancel") && r.contains("Esc Cancel"))
                    && Some(below(rows)) != before
            });
            let left_to_read = below(&screen);
            // The message may itself say "Cancel and save...": the button row comes last,
            // before the footer that also names Cancel.
            let footer = screen.iter().rposition(|r| r.contains("Enter Cancel") && r.contains("Esc Cancel")).unwrap();
            let buttons = screen[..footer].iter().rposition(|r| r.contains(" Cancel ")).unwrap();
            // The Cancel button is two columns inside the box, after the margin.
            let left = screen[buttons].split("Cancel").next().unwrap().chars().count() - 3;
            let border = screen[buttons].chars().nth(left).unwrap();
            let right = left + 1 + screen[buttons].chars().skip(left + 1).position(|c| c == border).unwrap();
            let top =
                screen[..buttons].iter().rposition(|row| matches!(row.chars().nth(left), Some('╭' | '+'))).unwrap();
            // Above the buttons: an empty row and, when there is more to read, the row saying so.
            let body = &screen[top + 1..buttons - 1 - usize::from(left_to_read.is_some())];
            let at = *first.get_or_insert(left_to_read.unwrap_or(0)) - left_to_read.unwrap_or(0);
            rows.truncate(at);
            for row in body {
                let cells: Vec<char> = row.chars().collect();
                rows.push((cells[left + 1] == '↪', cells[left + 2..right - 1].to_vec()));
            }
            if left_to_read.unwrap_or(0) == 0 {
                break;
            }
            before = Some(left_to_read);
            self.send(key);
        }
        let mut lines: Vec<Vec<char>> = Vec::new();
        for (continued, text) in rows {
            match lines.last_mut().filter(|_| continued) {
                Some(line) => {
                    let indent = line.iter().take_while(|c| **c == ' ').count().min(text.len() / 2);
                    line.extend(&text[indent..]);
                }
                None => lines.push(text),
            }
        }
        lines.iter().map(|line| line.iter().collect::<String>().trim_end().to_string()).collect()
    }

    /// Wait until nothing more is drawn for a while; how much was drawn.
    fn settle(&self) -> usize {
        let mut seen = self.screen.lock().unwrap().len();
        let start = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(300));
            let (now, framed) = {
                let screen = self.screen.lock().unwrap();
                (screen.len(), screen.windows(8).any(|w| w == b"\x1b[?2026l"))
            };
            // A new connection can be quiet before its first frame arrives.
            if now == seen && framed {
                return now;
            }
            seen = now;
            assert!(start.elapsed() < Duration::from_secs(10), "the screen never settles");
        }
    }
}

/// Frames are drawn over the last one, with the rows that did not change left out: whatever
/// happens, a client ends up showing what a client attaching then is shown whole.
#[test]
fn frames_drawn_over_the_last_one_end_as_a_whole_frame() {
    let env = Env::new("incremental");
    std::fs::create_dir_all(env.root.join("state/kraal")).unwrap();
    std::fs::write(env.root.join("state/kraal/welcomed"), "").unwrap();
    // In a directory that is no repository: what git says of one comes when git has answered,
    // which would change the frames by itself between two looks at them.
    let here = env.root.display().to_string();
    for (workspace, pane) in [("w", "main"), ("w2", "other"), ("w3", "third")] {
        env.workspace(workspace, &format!("layout {{\n    cwd {here:?}\n    pane name={pane:?}\n}}\n"));
    }
    let run = |name: &str, place: &str, script: &str| {
        env.run(&["run", "--name", name, place, "--cwd", &here, "--", "sh", "-c", script]);
    };
    let frames = env.open_noting_frames("w");
    let colors = "printf '\\033[1;31mred \\033[42;4mgreen\\033[0m \\346\\227\\245\\346\\234\\254 \
                  \\033]8;;https://example.org\\033\\\\link\\033]8;;\\033\\\\ e\\314\\201 done\\n'; exec cat";
    run("colors", "--right", colors);
    run("lines", "--down", "seq 1 200; exec cat");
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_screen("done");
    let same = |client: &RawClient, step: &str| {
        client.settle();
        // What it shows before another client attaches: that draws every client again, whole.
        let shown = client.screen.lock().unwrap().clone();
        let fresh = RawClient::attach(&env, "w");
        fresh.settle();
        let whole = fresh.screen.lock().unwrap().clone();
        let (a, b) = (terminal_after(&shown, 120, 40), terminal_after(&whole, 120, 40));
        for (y, (a, b)) in a.iter().zip(&b).enumerate() {
            assert_eq!(a, b, "{step}: row {y} differs");
        }
        drop(fresh);
        client.settle();
    };
    same(&client, "start");
    // Typing into a program: its echo, one cell at a time.
    env.run(&["focus", "colors"]);
    client.settle();
    let before = client.settle();
    client.send(b"x");
    // The frame with the echo is that character and the cursor, whatever else is drawn then.
    let start = Instant::now();
    let echo = loop {
        let after = String::from_utf8_lossy(&client.screen.lock().unwrap()[before..]).into_owned();
        if let Some(frame) = after.split_inclusive("\x1b[?2026l").find(|f| f.contains('x')) {
            break frame.to_string();
        }
        assert!(start.elapsed() < Duration::from_secs(10), "the echo never came: {after:?}");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(echo.len() < 100, "a frame for one character sent {} bytes: {echo:?}", echo.len());
    // Keys typed one after the other are echoed in frames made from what the pane printed alone
    // (but the first after the server's periodic work, made whole).
    let drawn = frames().len();
    let from_output = || frames()[drawn..].iter().filter(|f| f.ends_with("output")).count();
    let start = Instant::now();
    for b in b"yz 123".iter().cycle() {
        if from_output() >= 3 {
            break;
        }
        assert!(start.elapsed() < PATIENCE, "{:?}", &frames()[drawn..]);
        client.send(&[*b]);
        std::thread::sleep(Duration::from_millis(30));
    }
    same(&client, "typing");
    // Typing while another pane prints: frames made from what one pane printed, from what both
    // did, and one after the other.
    for i in 0..8 {
        client.send(b"k");
        env.run(&["send", "-p", "lines", &format!("between {i}\r")]);
    }
    client.send(b"last");
    same(&client, "typing while another pane prints");
    // What a pane prints changes what its frame says of it (a command that failed), with
    // nothing else happening: the keys only went to its shell.
    env.run(&["focus", "main"]);
    client.settle();
    client.send(b"printf '\\033]133;A\\007$ \\033]133;B\\007x\\n\\033]133;C\\007\\033]133;D;7\\007'\r");
    client.wait_for_row("\u{2717} 7");
    same(&client, "a command failed");
    env.run(&["focus", "colors"]);
    // Output scrolling one pane.
    env.run(&["send", "-p", "lines", "more\r"]);
    same(&client, "scrolling");
    // Image placeholders without an image draw as blanks.
    run("img", "--right", "printf 'img:\\364\\216\\273\\256 end\\n'; exec cat");
    same(&client, "placeholders");
    // A floating pane over the others, then gone.
    run("float", "--floating", "echo FLOAT; exec cat");
    client.wait_for_screen("FLOAT");
    same(&client, "floating");
    env.run(&["close", "float"]);
    same(&client, "floating closed");
    // Another tab, and back.
    run("tab2", "--tab", "echo TAB2; exec cat");
    client.wait_for_screen("TAB2");
    same(&client, "new tab");
    env.run(&["focus", "colors"]);
    same(&client, "tab back");
    // A pane that is not on the screen prints, then one that is.
    env.run(&["send", "-p", "tab2", "unseen\r"]);
    client.send(b"seen");
    same(&client, "a pane of another tab printed");
    // Colors stacked with the pane below it: lines folded to a row, renamed there, then open.
    client.send(b"\x02Ps\x1b[B");
    same(&client, "stacked");
    env.run(&["rename", "-p", "lines", "folded"]);
    same(&client, "folded pane renamed");
    client.send(b"\x1b[1;3B");
    same(&client, "other pane of the stack open");
    // The menu over the panes, then closed.
    client.send(b"\x02");
    client.settle();
    client.send(b"\x1b");
    same(&client, "menu closed");
    // A second workspace, then a third: the sidebar lists them as open.
    env.run(&["open", "w2"]);
    same(&client, "sidebar");
    env.run(&["open", "w3"]);
    same(&client, "sidebar changed");
    // A pane of another workspace prints, then one of this one: the sidebar shows both.
    env.run(&["send", "-p", "other", "elsewhere\r"]);
    client.send(b"here");
    same(&client, "a pane of another workspace printed");
    // A pane closed: the others take its place.
    env.run(&["close", "img"]);
    same(&client, "pane closed");
}

/// What a pane types by itself before the thing a test looks at: a character at a time, each in
/// a frame of its own, most of them made from what the pane printed alone.
const PRINTS_A_LITTLE: &str = "i=0; while [ $i -lt 20 ]; do printf t; sleep 0.02; i=$((i+1)); done";

/// The notes of the frames of a test with one client (see [`Env::open_noting_frames`]), once
/// the pane that printed a little did the thing looked at and nothing more: some were made from
/// what it printed alone, and the last one, which drew that thing, whole.
fn drawn_whole_after_output(frames: &[String]) {
    assert!(frames.iter().any(|f| f.ends_with("output")), "{frames:?}");
    assert!(frames.last().is_some_and(|f| f.ends_with("whole")), "{frames:?}");
}

/// A bell in a tab that is not shown marks that tab in the top bar. Nothing a frame keeps of the
/// pane says so (its name, what it works on): only that something other than output happened,
/// which is what makes the frame whole.
#[test]
fn a_bell_in_another_tab_is_drawn_by_a_whole_frame() {
    let env = Env::new("bell");
    // No welcome note: the top bar has room for both tabs.
    std::fs::create_dir_all(env.root.join("state/kraal")).unwrap();
    std::fs::write(env.root.join("state/kraal/welcomed"), "").unwrap();
    let tabs = "tab name=\"one\" focus=true {\n        pane name=\"front\"\n    }\n    \
                tab name=\"two\" {\n        pane name=\"back\"\n    }";
    env.workspace("w", &format!("layout {{\n    {tabs}\n}}\n"));
    let frames = env.open_noting_frames("w");
    let client = RawClient::attach(&env, "w");
    client.wait_for_row(" 2 two ");
    // Then nothing comes after the bell: its shell is busy.
    env.run(&["send", "-p", "back", "--enter", &format!("{PRINTS_A_LITTLE}; printf '\\a'; sleep 600")]);
    client.wait_for_row("two  !");
    client.settle();
    drawn_whole_after_output(&frames());
}

/// A command that starts takes the failure of the one before off the pane's frame. Nothing is
/// signalled then, as when a command ends: only what the frame keeps of the pane says it
/// changed, which is what makes the frame whole. (Without a sidebar: with one, the frame is
/// whole already for what the sidebar may say of the pane.)
#[test]
fn a_command_that_starts_is_drawn_by_a_whole_frame() {
    let env = Env::new("starts");
    let config = env.root.join("config/kraal/config.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{text}[sidebar]\nvisible = false\n")).unwrap();
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    let frames = env.open_noting_frames("w");
    let client = RawClient::attach(&env, "w");
    // What a shell that marks its commands prints around them: one that failed, then one that
    // starts and runs on.
    let failed = "printf '\\033]133;A\\007$ \\033]133;B\\007x\\n\\033]133;C\\007\\033]133;D;7\\007'";
    let starts = "printf '\\033]133;A\\007$ \\033]133;B\\007y\\n\\033]133;C\\007'";
    env.run(&["send", "-p", "shell", "--enter", failed]);
    client.wait_for_row("\u{2717} 7");
    env.run(&["send", "-p", "shell", "--enter", &format!("{PRINTS_A_LITTLE}; {starts}; sleep 600")]);
    client.wait_for_rows("the failure off the frame", |rows| !rows.iter().any(|r| r.contains("\u{2717} 7")));
    client.settle();
    drawn_whole_after_output(&frames());
}

/// The columns a pane's frame starts and ends at on the row under the top bar, where the sidebar
/// has no corner: those of the first pane there. A frame wider than what is left of the screen
/// is closed at the last column.
fn frame_columns(rows: &[String]) -> (Option<usize>, Option<usize>) {
    let at = |corner: char| rows[1].chars().position(|c| c == corner);
    (at('╭'), at('╮'))
}

/// The sidebar is there from the first screen of a new session, with one workspace and nothing
/// else to list. The key hides it and shows it again in the terminal it is typed in, `[sidebar]
/// visible` says for the terminals that did not choose, and a terminal less than three times as
/// wide as the sidebar has none: its panes keep the whole width.
#[test]
fn the_sidebar_is_there_from_the_first_screen() {
    let env = Env::new("sidebar");
    let shown = |rows: &[String]| rows[0].starts_with("╭─ WORKSPACES");
    // Without the sidebar, the top bar names the workspace.
    let hidden = |rows: &[String]| rows[0].starts_with(" main ");
    let shows = |client: &RawClient, what: &str| client.wait_for_rows(what, shown);
    let hides = |client: &RawClient, what: &str| client.wait_for_rows(what, hidden);
    let size = || {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        (panes[0]["cols"].as_u64().unwrap(), panes[0]["rows"].as_u64().unwrap())
    };
    let config = env.root.join("config/kraal/config.toml");
    let defaults = std::fs::read_to_string(&config).unwrap();
    let configure = |sidebar: &str| {
        std::fs::write(&config, format!("{defaults}{sidebar}")).unwrap();
        env.run(&["config", "reload"]);
    };

    // A session started by a command that opens nothing: the first terminal gets the workspace
    // `main` and a shell, and its first frame has the sidebar, the pane beside it.
    env.run(&["tunnel", "list"]);
    let mut client = RawClient::attach_sized(&env, None, (120, 40));
    let rows = client.first_frame();
    assert!(shown(&rows) && rows[1].starts_with("│ ● main"), "{}", rows.join("\n"));
    assert_eq!(frame_columns(&rows), (Some(29), Some(119)), "{}", rows.join("\n"));
    assert_eq!(size(), (89, 36));
    // What has nothing to list has no section.
    client.settle();
    let rows = client.rows();
    assert!(rows[2].starts_with("│   Open · 1 tab · 1 pane"), "{}", rows.join("\n"));
    let sections = rows.iter().filter(|r| r.starts_with("├─ ")).collect::<Vec<_>>();
    assert!(sections.iter().all(|s| s.starts_with("├─ ACCOUNTS")), "{sections:?}");

    // The key hides it in the terminal it is typed in, which gives its room to the panes, and
    // shows it again. Another terminal keeps its own.
    let other = RawClient::attach(&env, "main");
    shows(&other, "the sidebar of a second terminal");
    client.send(b"\x02Vl");
    let rows = hides(&client, "the sidebar hidden with the key");
    assert_eq!(frame_columns(&rows), (Some(0), Some(119)), "{}", rows.join("\n"));
    assert_eq!(size(), (118, 36));
    other.settle();
    assert!(shown(&other.rows()), "{}", other.rows().join("\n"));
    client.send(b"\x02Vl");
    shows(&client, "the sidebar shown again with the key");
    // A zoomed pane takes the room of the panes, beside the sidebar.
    env.run(&["run", "--name", "second", "--right", "--", "sh"]);
    client.wait_for_rows("two panes side by side", |rows| frame_columns(rows).1.is_some_and(|end| end < 119));
    client.send(b"\x02z");
    let rows = client.wait_for_rows("the zoomed pane", |rows| frame_columns(rows) == (Some(29), Some(119)));
    assert!(shown(&rows), "{}", rows.join("\n"));
    env.run(&["close", "second"]);

    // `visible = false` hides it where no one chose: the terminal that asked for it keeps it, a
    // terminal attaching then has none, and the key shows it there too.
    configure("[sidebar]\nvisible = false\n");
    hides(&other, "the sidebar hidden by the configuration");
    client.settle();
    assert!(shown(&client.rows()), "{}", client.rows().join("\n"));
    let mut late = RawClient::attach(&env, "main");
    hides(&late, "no sidebar for a terminal attaching");
    late.send(b"\x02Vl");
    shows(&late, "the sidebar asked for with the key");
    // So wide that no terminal has room for it, it is left out, whoever asked.
    configure("[sidebar]\nwidth = 30000\n");
    hides(&client, "no room for the sidebar");

    // A terminal less than three times as wide as the sidebar (28 columns) has none, and its pane
    // the whole width; one column more and the sidebar is there.
    configure("");
    let mut narrow = RawClient::attach_sized(&env, Some("main"), (80, 24));
    let rows = hides(&narrow, "no sidebar in 80 columns");
    assert_eq!(frame_columns(&rows), (Some(0), Some(79)), "{}", rows.join("\n"));
    assert_eq!(size(), (78, 20));
    for (cols, frame) in [(60, (0, 59)), (83, (0, 82)), (84, (29, 83)), (80, (0, 79))] {
        narrow.resize((cols, 24));
        let what = format!("the pane of a terminal of {cols} columns");
        let rows = narrow.wait_for_rows(&what, |rows| frame_columns(rows) == (Some(frame.0), Some(frame.1)));
        assert_eq!(shown(&rows), cols == 84, "{}", rows.join("\n"));
    }

    // What a terminal chose is its own, and is not saved: after a restart, the configuration
    // says again.
    client.send(b"\x02Vl");
    hides(&client, "the sidebar hidden before a restart");
    env.run(&["restart", "--yes"]);
    let back = RawClient::attach_sized(&env, None, (120, 40));
    shows(&back, "the sidebar after a restart");
}

/// The panes have one size, that of the terminal typed in last, and every attached terminal has
/// its own sidebar: shown, hidden with the key, or left out where the terminal is too narrow for
/// it. Each terminal draws the panes beside its own sidebar: never over it, and never leaving its
/// place empty.
#[test]
fn each_terminal_draws_the_panes_beside_its_own_sidebar() {
    let env = Env::new("beside");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    // The columns of the sidebar and the space after it, above the place of the accounts (which
    // come with their usage, and that may be seen while the test runs).
    let side = |rows: &[String]| -> Vec<String> { rows[..32].iter().map(|r| r.chars().take(29).collect()).collect() };
    let shows = |client: &RawClient, columns: (usize, usize), when: &str| {
        let columns = (Some(columns.0), Some(columns.1));
        let rows = client.wait_for_rows(when, |rows| frame_columns(rows) == columns);
        // And it stays so.
        client.settle();
        assert_eq!(frame_columns(&client.rows()), columns, "{when}:\n{}", client.rows().join("\n"));
        rows
    };
    let mut wide = RawClient::attach(&env, "w");
    let sidebar = side(&shows(&wide, (29, 119), "the pane beside the sidebar"));
    assert!(sidebar[0].starts_with("╭─ WORKSPACES"), "{sidebar:?}");

    // A second terminal hides its sidebar: the panes are laid out for it, from its first column.
    // The first one keeps its sidebar, and shows of the panes what fits beside it.
    let mut bare = RawClient::attach(&env, "w");
    bare.send(b"\x02Vl");
    shows(&bare, (0, 119), "the pane from the first column");
    assert_eq!(side(&shows(&wide, (29, 119), "the pane beside the sidebar still")), sidebar);
    // Typed in, the first terminal has the panes laid out beside its sidebar: the other one
    // shows them from its first column all the same, with room left at its right.
    wide.send(b"echo typed-in-wide\r");
    shows(&bare, (0, 90), "the narrower pane from the first column");
    assert_eq!(side(&shows(&wide, (29, 119), "the pane laid out beside the sidebar")), sidebar);

    // A terminal too narrow for a sidebar has none, and the panes are laid out for it.
    let mut narrow = RawClient::attach_sized(&env, Some("w"), (80, 24));
    let rows = shows(&narrow, (0, 79), "the pane over the whole width");
    assert!(rows[0].starts_with(" w "), "the workspace is named in the top bar: {:?}", rows[0]);
    shows(&bare, (0, 79), "the narrow terminal's pane from the first column");
    assert_eq!(side(&shows(&wide, (29, 108), "the narrow terminal's pane beside the sidebar")), sidebar);
    // And it shows the panes of a wider one from its first column, as far as its last.
    wide.send(b"echo typed-again\r");
    narrow.wait_for_row("typed-again");
    shows(&narrow, (0, 79), "the wide terminal's pane from the first column");
    shows(&bare, (0, 90), "the wide terminal's pane");
    assert_eq!(side(&shows(&wide, (29, 119), "the pane beside the sidebar again")), sidebar);
    // Once wide enough, it has the sidebar too.
    narrow.resize((120, 40));
    assert_eq!(side(&shows(&narrow, (29, 119), "the sidebar once there is room")), sidebar);
}

/// An account's usage window resets with the clock, whatever else happens: the sidebar shows the
/// account without it from then on, while a pane prints (frames made from what that pane printed
/// alone, each checked against a whole one in this build) as on a screen where nothing moves.
#[test]
fn the_sidebar_follows_a_usage_window_that_resets() {
    let env = Env::new("resets");
    std::fs::write(
        env.root.join("config/kraal/accounts.toml"),
        "[[account]]\nid = 'work'\nname = 'work'\nagent = 'claude'\nhome = '~/work'\n",
    )
    .unwrap();
    let usage = env.root.join("state/kraal/usage/by-id");
    std::fs::create_dir_all(&usage).unwrap();
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_row("ACCOUNTS");
    client.wait_for_row("Claude · work");
    let now = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    // The named account's usage: its five-hour window, the fuller, and its weekly one.
    let seen = |five_hour: u64, weekly: u64| {
        let said = serde_json::json!({
            "five_hour": { "used_percent": 93, "resets_at": five_hour },
            "weekly": { "used_percent": 41, "resets_at": weekly },
            "observed_at": now(),
            "source": "statusline",
        });
        std::fs::write(usage.join("new"), said.to_string()).unwrap();
        std::fs::rename(usage.join("new"), usage.join("claude-work.json")).unwrap();
    };
    let shows = |rows: &[String], detail: &str| {
        rows.windows(2)
            .any(|r| r[0].contains("Claude · work") && r[1].split('│').nth(1).is_some_and(|text| text.trim() == detail))
    };
    // Keys typed one after the other, each echoed in a frame of its own, until the sidebar says
    // `figure` of the account.
    let typing_until = |client: &mut RawClient, figure: &str| {
        let row = format!("{figure} used");
        let start = Instant::now();
        while !shows(&client.rows(), &row) {
            assert!(start.elapsed() < Duration::from_secs(15), "never {row}:\n{}", client.rows().join("\n"));
            client.send(b"x");
            std::thread::sleep(Duration::from_millis(15));
        }
    };
    // Resets as far away as a file can say are none to wait for: the usage shows all the same.
    seen(i64::MAX as u64, i64::MAX as u64);
    typing_until(&mut client, "93%");
    // The five-hour window resets while keys are echoed, the weekly one once nothing happens.
    seen(now() + 2, now() + 5);
    typing_until(&mut client, "41%");
    client.wait_for_rows("both usage windows expired", |rows| shows(rows, "Last checked just now"));
}

/// The sidebar names panes of every workspace: what a pane prints that changes its name (a
/// title) shows at once for a client that looks at another workspace, where nothing happens.
#[test]
fn the_sidebar_follows_what_a_pane_of_another_workspace_prints() {
    let env = Env::new("elsewhere");
    env.workspace("here", "layout {\n    pane name=\"quiet\"\n}\n");
    env.workspace("there", "layout {\n    pane\n}\n");
    env.run(&["open", "there"]);
    let client = RawClient::attach(&env, "here");
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let unnamed = panes.as_array().unwrap().iter().find(|p| p["workspace"] == "there").unwrap()["id"].to_string();
    // A port the pane serves, as far as kraal can tell: the sidebar lists it with the pane's name.
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    env.run(&["send", "-p", &unnamed, "--enter", &format!("echo http://localhost:{port}/")]);
    client.wait_for_row(&format!(":{port}"));
    // The title comes when the request that typed its command is long over, and all is quiet.
    env.run(&["send", "-p", &unnamed, "--enter", "sleep 1; printf '\\033]2;renamed\\007'"]);
    client.settle();
    assert!(!client.rows().iter().any(|r| r.contains("renamed")), "{}", client.rows().join("\n"));
    client.wait_for_row("   renamed");
}

/// The rows a pane shows far back come from its compact history: a client drawn incrementally
/// while the view goes there and back shows what a client attaching then is shown whole.
#[test]
fn rows_brought_back_from_the_history_are_drawn() {
    let env = Env::new("farback");
    std::fs::create_dir_all(env.root.join("state/kraal")).unwrap();
    std::fs::write(env.root.join("state/kraal/welcomed"), "").unwrap();
    env.workspace("w", "layout {\n    pane name=\"log\"\n}\n");
    env.run(&["open", "w"]);
    env.wait_for_text("log", "$");
    env.run(&["send", "-p", "log", "seq 1 4000; echo ENDS\r"]);
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_screen("ENDS");
    // The pane's rows, between the bars (they show the client's mode), compared.
    let same = |client: &RawClient, step: &str| {
        client.settle();
        let fresh = RawClient::attach(&env, "w");
        fresh.settle();
        client.settle();
        let (a, b) = (client.screen.lock().unwrap().clone(), fresh.screen.lock().unwrap().clone());
        let (a, b) = (terminal_after(&a, 120, 40), terminal_after(&b, 120, 40));
        for (y, (a, b)) in a.iter().zip(&b).enumerate().take(39).skip(1) {
            assert_eq!(a, b, "{step}: row {y} differs");
        }
        drop(fresh);
        client.settle();
    };
    same(&client, "at the bottom");
    // Many screens back with the wheel, beyond what the emulator keeps.
    for _ in 0..80 {
        client.send(b"\x1b[<64;60;20M");
    }
    same(&client, "far back");
    // A search goes further back still, then down again.
    client.send(b"\x02/");
    client.settle();
    client.send(b"1234");
    client.send(b"\r");
    same(&client, "search far back");
    // The view is there, far beyond the rows the emulator keeps.
    let rows = terminal_after(&client.screen.lock().unwrap(), 120, 40);
    let text = |row: &String| -> String {
        row.split(" | ").map(|c| c.split(' ').next().unwrap_or("").trim_matches('"').to_string()).collect()
    };
    assert!(rows.iter().any(|r| text(r).contains("│1234│")), "{:?}", rows.iter().map(text).collect::<Vec<_>>());
    for _ in 0..30 {
        client.send(b"\x1b[<65;60;20M");
    }
    same(&client, "down again");
    client.send(b"\x1b");
    std::thread::sleep(Duration::from_millis(100));
    same(&client, "back at the bottom");
}

/// The last row shows what can be done in the focused pane, with its keys, and the menu key at
/// the right end; `ctrl+b V k` gives the row to the panes, and the menu key goes to the top bar.
#[test]
fn the_bottom_bar_shows_the_focused_panes_actions() {
    let env = Env::new("bottombar");
    std::fs::create_dir_all(env.root.join("state/kraal")).unwrap();
    std::fs::write(env.root.join("state/kraal/welcomed"), "").unwrap();
    // A fake ssh: a shell "on the server".
    let ssh = env.root.join("bin/ssh");
    std::fs::create_dir_all(ssh.parent().unwrap()).unwrap();
    write_script(&ssh, "#!/bin/sh\nexec sh\n");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let height = |name: &str| -> u64 {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        panes.as_array().unwrap().iter().find(|p| p["name"] == name).unwrap()["rows"].as_u64().unwrap()
    };
    let mut client = RawClient::attach(&env, "w");
    let shown = client.wait_for_row("alt+n new pane");
    let last = shown[39].trim();
    assert!(last.starts_with("alt+n new pane  ctrl+b P → split  ctrl+b s connect to a server"), "{last}");
    assert!(last.ends_with("ctrl+b menu") && !shown[0].contains("menu"), "{shown:?}");
    assert_eq!(height("main"), 36, "the panes end above the bar");

    env.run(&["run", "--name", "web", "--", &ssh.display().to_string(), "web-prod-1"]);
    env.run(&["focus", "web"]);
    let shown = client.wait_for_row("web-prod-1 ▸ ctrl+b f its files  ctrl+b r tools there  ctrl+b S d download");
    assert!(shown[39].trim_end().ends_with("ctrl+b menu"), "{shown:?}");

    // A click on the menu key opens the menu (mouse columns count from 1).
    let col = shown[39].chars().count() - "menu ".len() + 1;
    client.send(format!("\x1b[<0;{col};40M\x1b[<0;{col};40m").as_bytes());
    client.wait_for_row("Connect to a server");
    client.send(b"\x1b");

    // Off for the session: the panes get the row, the menu key goes to the top bar.
    client.send(b"\x02Vk");
    let shown = client.wait_for_rows("the menu key on top", |rows| rows[0].contains("ctrl+b menu"));
    assert!(!shown[39].contains("its files"), "{shown:?}");
    let start = Instant::now();
    while height("main") != 37 {
        assert!(start.elapsed() < Duration::from_secs(10), "the panes never got the row");
        std::thread::sleep(Duration::from_millis(50));
    }
    client.send(b"\x02Vk");
    client.wait_for_row("web-prod-1 ▸ ctrl+b f its files");
}

/// The menu, pickers and viewers go above the bottom bar, the menu right on it; without the bar
/// they go down to the last row.
#[test]
fn overlays_stay_above_the_bottom_bar() {
    let env = Env::new("overbar");
    std::fs::create_dir_all(env.root.join("state/kraal")).unwrap();
    std::fs::write(env.root.join("state/kraal/welcomed"), "").unwrap();
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let mut client = RawClient::attach(&env, "w");
    let bar = client.wait_for_row("alt+n new pane")[39].clone();
    let framed = |row: &str| row.contains(['│', '╭', '╮', '╰', '╯', '─']);
    for (keys, title) in [(&b"\x02"[..], "Connect to a server"), (b"\x02 ", "Commands"), (b"\x02Vj", "Messages")] {
        client.send(keys);
        let rows = client.wait_for_row(title);
        assert!(!framed(&rows[39]), "{title} over the bar:\n{}", rows.join("\n"));
        client.send(b"\x1b");
        client.wait_for_rows("the bar back", |rows| rows[39] == bar);
    }
    // The menu sits right on the bar, which keeps its keys while the menu is open.
    client.send(b"\x02");
    let rows = client.wait_for_row("Connect to a server");
    assert!(rows[38].trim_end().ends_with('╯') && rows[39] == bar, "{}", rows.join("\n"));
    client.send(b"\x1b");

    // Without the bar, down to the last row.
    client.send(b"\x02Vk");
    client.wait_for_rows("the menu key on top", |rows| rows[0].contains("ctrl+b menu"));
    client.send(b"\x02");
    let rows = client.wait_for_row("Connect to a server");
    assert!(rows[39].trim_end().ends_with('╯'), "{}", rows.join("\n"));
}

/// The script of a pane that says its size each time its terminal is resized, as a program that
/// draws itself again does.
fn says_its_size(name: &str) -> String {
    format!("trap 'echo {name} is $(stty size)' WINCH; echo {name} ready; while :; do sleep 1 & wait $!; done")
}

/// What the program of the pane `name` says once told of the size the pane has now.
fn says(env: &Env, name: &str) -> String {
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let pane = panes.as_array().unwrap().iter().find(|p| p["name"] == name).cloned().unwrap();
    format!("{name} is {} {}", pane["rows"], pane["cols"])
}

/// Tell the server the client's terminal is now `cols × rows`; how much it had drawn until then.
fn resize(client: &mut RawClient, cols: usize, rows: usize) -> usize {
    let drawn = client.screen.lock().unwrap().len();
    raw_frame(&mut client.stream, 0, format!(r#"{{"type":"resize","cols":{cols},"rows":{rows}}}"#).as_bytes());
    drawn
}

/// The frames a client was sent.
fn frames(client: &RawClient) -> usize {
    String::from_utf8_lossy(&client.screen.lock().unwrap()).matches("\x1b[?2026l").count()
}

/// What its program draws when a pane is resized is the picture the user waits for: every pane
/// shows it, at the size it was given last, however fast the resizes came, and the client ends
/// with what a client attaching then is shown whole.
#[test]
fn panes_resized_show_what_their_programs_draw_again() {
    let env = Env::new("redraws");
    std::fs::create_dir_all(env.root.join("state/kraal")).unwrap();
    std::fs::write(env.root.join("state/kraal/welcomed"), "").unwrap();
    let names = ["one", "two", "three", "four"];
    let pane = |name: &str| format!("pane name={name:?} command=\"sh\" {{ args \"-c\" {:?}; }}", says_its_size(name));
    let column = |a: &str, b: &str| format!("pane split_direction=\"horizontal\" {{\n{}\n{}\n}}", pane(a), pane(b));
    env.workspace(
        "w",
        &format!(
            "layout {{\npane split_direction=\"vertical\" {{\n{}\n{}\n}}\n}}\n",
            column("one", "two"),
            column("three", "four")
        ),
    );
    env.run(&["open", "w"]);
    let mut client = RawClient::attach(&env, "w");
    for name in names {
        client.wait_for_row(&format!("{name} ready"));
    }
    client.settle();
    // One resize, then many faster than the programs answer, then back: each ends at a size no
    // pane had before.
    let bursts: [&[(usize, usize)]; 3] = [
        &[(100, 30)],
        &[(90, 28), (140, 44), (96, 32), (132, 42), (88, 26), (150, 46), (104, 34), (126, 38)],
        &[(120, 40)],
    ];
    let mut drawn = 0;
    for sizes in bursts {
        for (cols, rows) in sizes {
            drawn = resize(&mut client, *cols, *rows);
            std::thread::sleep(Duration::from_millis(1));
        }
        let (cols, rows) = sizes[sizes.len() - 1];
        for name in names {
            wait_until(Duration::from_secs(10), &format!("the size of {name} never shown"), || {
                // The first frame after a resize draws the whole screen: what came since the
                // last one is what a terminal of that size shows.
                let shown = text_after(&client.screen.lock().unwrap()[drawn..], cols, rows);
                let said = says(&env, name);
                shown.iter().any(|row| row.contains(&said))
            });
        }
        client.settle();
    }
    let fresh = RawClient::attach(&env, "w");
    fresh.settle();
    client.settle();
    let (a, b) = (client.screen.lock().unwrap()[drawn..].to_vec(), fresh.screen.lock().unwrap().clone());
    // The cells and the cursor: the modes were set when the client attached, before the frames
    // read here.
    let shown = |bytes: &[u8]| terminal_after(bytes, 120, 40)[..41].to_vec();
    for (y, (a, b)) in shown(&a).iter().zip(&shown(&b)).enumerate() {
        assert_eq!(a, b, "row {y} differs");
    }
}

/// A pane printing without a pause is drawn at its own pace whatever is resized meanwhile: a
/// resize adds its frame and those of the panes that draw themselves again, not a frame for
/// each piece of what the other one prints.
#[test]
fn resizes_while_a_pane_floods_do_not_draw_a_storm_of_frames() {
    let env = Env::new("resizeflood");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    env.run(&["run", "--name", "quiet", "--right", "--", "sh", "-c", &says_its_size("quiet")]);
    env.run(&["run", "--name", "flood", "--down", "--", "yes"]);
    env.run(&["focus", "main"]);
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_row("quiet ready");
    std::thread::sleep(Duration::from_millis(300));
    let (before, start) = (frames(&client), Instant::now());
    for i in 0..20 {
        let (cols, rows) = if i % 2 == 0 { (100, 30) } else { (120, 40) };
        resize(&mut client, cols, rows);
        std::thread::sleep(Duration::from_millis(50));
    }
    let per_second = (frames(&client) - before) as f64 / start.elapsed().as_secs_f64();
    // The flood is drawn every 8 ms or so, and each resize adds a few frames: far from the
    // thousands a frame for each read of the flood would make.
    assert!(per_second < 400.0, "{per_second:.0} frames a second");
    // The quiet pane's picture still comes through, at a size it did not have yet.
    let drawn = resize(&mut client, 110, 36);
    wait_until(Duration::from_secs(10), "the size of the quiet pane never shown", || {
        let shown = text_after(&client.screen.lock().unwrap()[drawn..], 110, 36);
        let said = says(&env, "quiet");
        shown.iter().any(|row| row.contains(&said))
    });
}

#[test]
fn a_pane_printing_without_a_pause_does_not_starve_the_others() {
    let env = Env::new("starve");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    env.run(&["run", "--name", "other", "--", "sh", "-c", "sleep 1; echo OTHER-SPOKE; sleep 60"]);
    env.run(&["run", "--name", "flood", "--", "yes"]);
    env.run(&["focus", "flood"]);
    let _client = RawClient::attach(&env, "w");
    env.wait_for_text("other", "OTHER-SPOKE");
}

/// What a program prints first is drawn at once, up to a screenful: one that prints a little at
/// a time without a pause, from its start, is then drawn at its pace like any other, not once
/// for each piece of what it prints.
#[test]
fn a_program_printing_without_a_pause_from_its_start_draws_no_storm_of_frames() {
    let env = Env::new("firstflood");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let client = RawClient::attach(&env, "w");
    client.wait_for_row("$");
    let frames = || String::from_utf8_lossy(&client.screen.lock().unwrap()).matches("\x1b[?2026l").count();
    let (before, start) = (frames(), Instant::now());
    // A line at a time, each written by itself, thousands of them a second.
    let ticks = "BEGIN { while (1) { print \"tick\"; fflush(); for (i = 0; i < 2000; i++) {} } }";
    env.run(&["run", "--name", "ticks", "--right", "--", "awk", ticks]);
    std::thread::sleep(Duration::from_secs(1));
    let per_second = (frames() - before) as f64 / start.elapsed().as_secs_f64();
    // A frame for each of its first rows, then one every 8 ms or so.
    assert!(per_second < 400.0, "{per_second:.0} frames a second");
    client.wait_for_row("tick");
}

#[test]
fn output_parsed_while_keys_come_is_whole_and_in_order() {
    let env = Env::new("slices");
    env.workspace("w", "layout {\n    pane name=\"main\" command=\"cat\"\n}\n");
    env.run(&["open", "w"]);
    // About 280 KB in lines narrower than the pane: chunks of it are parsed while keys come,
    // each of which stops the parse for a moment.
    let script = r#"sleep 0.5; awk 'BEGIN { for (i = 1; i <= 9000; i++) printf "%06d %023d\n", i, 0 }'; echo FLOOD-DONE; exec sleep 60"#;
    env.run(&["run", "--name", "flood", "--", "sh", "-c", script]);
    env.run(&["focus", "main"]);
    let mut client = RawClient::attach(&env, "w");
    let start = Instant::now();
    while !env.run(&["read", "-p", "flood"]).contains("FLOOD-DONE") {
        assert!(start.elapsed() < Duration::from_secs(10), "the output never ended");
        for _ in 0..20 {
            client.send(b"x");
            std::thread::sleep(Duration::from_micros(500));
        }
    }
    let text = env.run(&["read", "-p", "flood", "--lines", "9500"]);
    let numbers: Vec<u32> = text.lines().filter_map(|l| l.split_whitespace().next()?.parse().ok()).collect();
    assert!(numbers == (1..=9000).collect::<Vec<u32>>(), "{} lines, first gap after {:?}", numbers.len(), {
        numbers.windows(2).find(|w| w[1] != w[0] + 1)
    });
}

#[test]
fn picker_actions_and_footers_follow_the_line_at_both_sizes() {
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::cell::Flags;

    let env = Env::new("pickerlines");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.workspace("closed", "layout {\n    pane\n}\n");
    std::fs::write(
        env.root.join("config/kraal/servers.toml"),
        "[[server]]\nname = \"web\"\nhost = \"web.example.com\"\ngroups = [\"fleet\"]\n",
    )
    .unwrap();
    env.run(&["open", "w"]);
    let click = |client: &mut RawClient, text: &str| {
        let rows = client.wait_for_row(text);
        let y = rows.iter().position(|r| r.contains(text)).unwrap();
        let x = rows[y][..rows[y].find(text).unwrap()].chars().count();
        client.send(format!("\x1b[<0;{};{}M\x1b[<0;{};{}m", x + 1, y + 1, x + 1, y + 1).as_bytes());
    };
    for size in [(124, 36), (80, 24)] {
        let mut client = RawClient::attach_sized(&env, Some("w"), size);
        client.send(b"\x02Al");
        client.wait_for_row("Enter Add account");
        let label = "+ Add account";
        let rows = client.wait_for_row(label);
        let y = rows.iter().position(|r| r.contains(label)).unwrap();
        let x = rows[y][..rows[y].find(label).unwrap()].chars().count();
        let term = emulate(&client.screen.lock().unwrap(), size.0, size.1);
        assert!(term.grid()[Line(y as i32)][Column(x)].flags.contains(Flags::BOLD), "at {size:?}");
        click(&mut client, label);
        client.wait_for_row("Name    e.g. Work or Personal");
        client.wait_for_row("Agent   Claude");
        client.close_overlay("Agent   Claude");
        client.wait_for_row("+ Add account");
        client.close_overlay("Enter Add account");

        client.send(b"\x02wclosed");
        client.wait_for_row("Enter Open · Tab Actions · Esc Close");
        client.send(b"\t");
        let rows = client.wait_for_row("Delete");
        assert!(rows.iter().any(|r| r.contains("Enter") && r.contains("Open")));
        // Match action names as words: the sidebar can still show the status "Closed".
        for absent in ["Rename", "Close"] {
            assert!(
                !rows.iter().any(|r| r.split_whitespace().any(|word| word == absent) && !r.contains("Esc Close")),
                "{size:?}: {absent}\n{}",
                rows.join("\n")
            );
        }
        client.close_overlay("Delete");
        client.send(b"\x1b[B");
        let rows = client.wait_for_row("Enter Add");
        assert!(!rows.iter().any(|r| r.contains("Tab Actions")));
        client.send(b"\x1b");
        client.wait_for_rows("the workspace picker closed", |rows| !rows.iter().any(|r| r.contains("Search  closed")));

        client.send(b"\x02s");
        client.wait_for_row("╭ Servers ");
        client.wait_for_row("Enter Open group");
        client.send(b"\t");
        let rows = client.wait_for_row("Run a command on it");
        for absent in ["Connect in a new tab", "Browse its files", "Add tunnel", "Remove saved server"] {
            assert!(!rows.iter().any(|r| r.contains(absent)), "{size:?}: {absent}");
        }
        click(&mut client, "Run a command on it");
        client.wait_for_row("Run on @fleet");
        client.send(b"\x1b");
    }
}

#[test]
fn choosing_the_current_account_keeps_running_and_exited_panes_as_they_are() {
    let env = Env::new("sameacct");
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let mut accounts = String::new();
    for agent in ["claude", "codex"] {
        write_script(
            &bin.join(agent),
            "#!/bin/sh\n[ \"$1\" = app-server ] && exit 0\n[ \"$2\" = status ] && exit 0\nprintf '%s\\n' \"$$\" >> \"$HOME/starts\"\necho READY\nwhile IFS= read -r line; do\n    [ \"$line\" = quit ] && exit 0\n    printf 'reply:%s\\n' \"$line\"\ndone\n",
        );
        for name in ["other", "work"] {
            let home = env.root.join(format!("{agent}-{name}-home"));
            std::fs::create_dir_all(&home).unwrap();
            accounts.push_str(&format!(
                "[[account]]\nname = {name:?}\nagent = {agent:?}\nhome = {:?}\n",
                home.display().to_string()
            ));
        }
    }
    std::fs::write(env.root.join("config/kraal/accounts.toml"), accounts).unwrap();
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let socket = env.root.join("run/kraal/sameacct.sock");
    for (agent, size) in [("claude", (124, 36)), ("codex", (80, 24))] {
        let out = env
            .cmd(&["agent", agent, "--account", "work", "--tab"])
            .env("PATH", &path)
            .env("KRAAL_SOCKET", &socket)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let pane = String::from_utf8(out.stdout).unwrap().trim().to_string();
        let pane_id: u32 = pane.parse().unwrap();
        env.wait_for_text(&pane, "READY");
        let starts = std::fs::read_to_string(env.root.join("home/starts")).unwrap();
        let mut client = RawClient::attach_sized(&env, Some("w"), size).traced();
        for exited in [false, true] {
            client.send(b"\x02Ac\x1b[B");
            let rows = client.wait_for_row("Enter Keep current account");
            assert!(rows.iter().any(|r| r.contains(&format!("{agent}·work")) && r.contains("current")));
            assert!(!rows.iter().any(|r| r.contains("more actions")));
            // Tab moves to the other account rather than opening launch or login actions.
            client.send(b"\t");
            let rows = client.wait_for_row("Enter Switch account");
            assert!(!rows.iter().any(|r| r.contains("more actions")));
            client.send(b"\x1b[A");
            client.wait_for_row("Enter Keep current account");
            client.send(b"\r");
            client.wait_for_rows("the account picker closed", |rows| {
                !rows.iter().any(|r| r.contains("Enter Keep current account"))
            });
            if !exited {
                client.send(b"alive\r");
                env.wait_for_text(&pane, "reply:alive");
            }
            let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
            let info = panes.as_array().unwrap().iter().find(|p| p["id"] == pane_id).unwrap();
            assert_eq!(!info["exit_code"].is_null(), exited);
            assert_eq!(std::fs::read_to_string(env.root.join("home/starts")).unwrap(), starts, "{agent} restarted");
            if !exited {
                client.send(b"quit\r");
                client.wait_for_row("exited (0)");
            }
        }
    }
}

#[test]
fn an_account_switch_never_resends_a_prompt_by_itself_and_a_failed_transfer_resumes_the_source() {
    let env = Env::new("switchacct");
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    // A stand-in for claude that says which config home and arguments it was given.
    write_script(
        &bin.join("claude"),
        r#"#!/bin/sh
[ "$2" = status ] && exit 0
echo "home=${CLAUDE_CONFIG_DIR:-default} args=$*"
if [ "$1" = --session-id ]; then
    mkdir -p "$CLAUDE_CONFIG_DIR/projects/-work"
    printf '{}\n' > "$CLAUDE_CONFIG_DIR/projects/-work/$2.jsonl"
fi
while IFS= read -r line; do :; done
"#,
    );
    let (alpha, beta) = (env.root.join("alpha-home"), env.root.join("beta-home"));
    std::fs::create_dir_all(&alpha).unwrap();
    std::fs::create_dir_all(beta.join("projects")).unwrap();
    std::fs::write(
        env.root.join("config/kraal/accounts.toml"),
        format!(
            "[[account]]\nname = \"alpha\"\nagent = \"claude\"\nhome = {:?}\n\n[[account]]\nname = \"beta\"\nagent = \"claude\"\nhome = {:?}\n",
            alpha.display().to_string(),
            beta.display().to_string()
        ),
    )
    .unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let socket = env.root.join("run/kraal/switchacct.sock");
    let agent = |args: &[&str]| -> String {
        let mut all = vec!["agent", "claude", "--account", "alpha", "--tab", "--"];
        all.extend_from_slice(args);
        let out = env.cmd(&all).env("PATH", &path).env("KRAAL_SOCKET", &socket).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let p = panes.as_array().unwrap().iter().find(|p| p["command"].to_string().contains(args[0])).unwrap();
        p["id"].to_string()
    };
    let info = |id: &str| -> serde_json::Value {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let id: u64 = id.parse().unwrap();
        panes.as_array().unwrap().iter().find(|p| p["id"] == id).unwrap().clone()
    };
    let on_alpha = format!("home={}", alpha.display());
    let on_beta = format!("home={}", beta.display());
    let mut client = RawClient::attach(&env, "w").traced();

    // 1. An option kraal does not know, then a word: its value, or a prompt. Switching stops the
    // agent and prepares it on the other account, but shows the command: Enter runs it.
    let odd = agent(&["--brand-new-flag", "Deploy now"]);
    env.wait_for_text(&odd, &on_alpha);
    env.run(&["focus", &odd]);
    client.send(b"\x02Ac");
    client.wait_for_row("Change account");
    client.send(b"beta");
    client.wait_for_row("1/");
    client.send(b"\r");
    client.wait_for_row("Stop and switch?");
    client.send(b"y");
    wait_until(Duration::from_secs(10), "the switched agent waits", || info(&odd)["suspended"] == true);
    let screen = env.wait_for_text(&odd, "cannot tell");
    assert!(!screen.contains(&on_beta), "it ran by itself: {screen}");
    client.send(b"\r");
    env.wait_for_text(&odd, &on_beta);
    let screen = env.run(&["read", "-p", &odd]).replace('\n', "");
    assert!(screen.contains("--brand-new-flag Deploy now"), "everything after the unknown option stays: {screen}");

    // 2. If copying fails after stopping, the source resumes without another keystroke.
    let plain = agent(&["--verbose"]);
    env.wait_for_text(&plain, &on_alpha);
    let command: Vec<String> =
        info(&plain)["command"].as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_string()).collect();
    let session = command.iter().skip_while(|a| *a != "--session-id").nth(1).expect("a session id").clone();
    std::fs::create_dir_all(alpha.join("projects/-work")).unwrap();
    std::fs::write(alpha.join(format!("projects/-work/{session}.jsonl")), "{}\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    // The first switch already created this project: it, not its parent, receives the copy.
    std::fs::set_permissions(beta.join("projects/-work"), std::fs::Permissions::from_mode(0o555)).unwrap();
    env.run(&["focus", &plain]);
    client.send(b"\x02Ac");
    client.wait_for_row("Change account");
    client.send(b"beta");
    client.wait_for_row("1/");
    client.send(b"\r");
    client.wait_for_row("Stop and switch?");
    client.send(b"y");
    // Lines may wrap in the pane: compare without line ends.
    let resumed = format!("{on_alpha} args=--resume {session} --verbose");
    wait_until(Duration::from_secs(10), "it resumes on alpha", || {
        env.run(&["read", "-p", &plain]).replace('\n', "").contains(&resumed)
    });
    assert_eq!(info(&plain)["suspended"], false);
    assert!(info(&plain)["exit_code"].is_null());
    assert!(!env.run(&["read", "-p", &plain]).replace('\n', "").contains(&on_beta));
    std::fs::set_permissions(beta.join("projects/-work"), std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn account_switch_agent(env: &Env, agent: &str, mode: &str) -> String {
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(
        &bin.join(agent),
        r#"#!/bin/sh
home="${CLAUDE_CONFIG_DIR:-${CODEX_HOME:-}}"
case "$*" in
    'auth status'|'login status')
        test -d "$home" || exit 1
        echo check >> "$home/checks"
        while test -f "$home/hold-check"; do sleep 0.05; done
        echo checked >> "$home/checked"
        exit "$(cat "$home/status" 2>/dev/null || echo 1)"
        ;;
    'auth login'|'login') echo login >> "$home/logins"; echo SIGNING-IN; exit 1 ;;
    app-server*) exit 0 ;;
esac
echo "$$" >> "$home/starts"
while test -f "$home/hold-start"; do sleep 0.05; done
test -f "$home/fail-start" && exit 23
if [ "$1" = --session-id ]; then
    mkdir -p "$home/projects/work"
    printf '{"message":1}\n' > "$home/projects/work/$2.jsonl"
fi
if [ -n "$CODEX_HOME" ]; then
    session="conversation-$(basename "$home")"
    [ "$1" = resume ] && session="$2"
    mkdir -p "$home/sessions/2026/10/13"
    printf '{"type":"session_meta","payload":{"id":"%s"}}\n' "$session" > "$home/sessions/2026/10/13/rollout-today-$session.jsonl"
fi
echo "READY home=$home args=$*"
while IFS= read -r line; do
    if [ -n "$CODEX_HOME" ]; then
        "$KRAAL_TEST_BIN" hook codex "{\"type\":\"agent-turn-complete\",\"thread-id\":\"$session\"}"
    fi
    printf 'reply:%s\n' "$line"
done
"#,
    );
    let mut config = format!("[limits]\nmode = {mode:?}\nthreshold = 90\n");
    for name in ["alpha", "beta"] {
        let home = env.root.join(name);
        std::fs::create_dir_all(home.join("projects/work")).unwrap();
        std::fs::write(home.join("status"), "0").unwrap();
        config.push_str(&format!(
            "[[account]]\nid = \"test-{agent}-{name}\"\nagent = {agent:?}\nname = {name:?}\nhome = {:?}\n",
            home.to_str().unwrap()
        ));
    }
    std::fs::write(env.root.join("config/kraal/accounts.toml"), config).unwrap();
    env.workspace("w", "layout { pane name=\"shell\" }\n");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let binary = env.cmd(&[]).get_program().to_os_string();
    let out = env.cmd(&["open", "w"]).env("PATH", &path).env("KRAAL_TEST_BIN", binary).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let socket = env.root.join(format!("run/kraal/{}.sock", env.session));
    let out = env
        .cmd(&["agent", agent, "--account", "alpha", "--tab"])
        .env("PATH", &path)
        .env("KRAAL_SOCKET", socket)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let pane = String::from_utf8(out.stdout).unwrap().trim().to_string();
    env.wait_for_text(&pane, "READY");
    if agent == "codex" {
        env.run(&["send", "-p", &pane, "first turn\r"]);
        env.wait_for_text(&pane, "reply:first turn");
    }
    pane
}

fn choose_switch_target(client: &mut RawClient, effect: &str) {
    client.send(b"\x02Ac");
    client.wait_for_row("Change account");
    client.send(b"beta");
    client.wait_for_rows("the target account's effect", |rows| {
        rows.iter().any(|r| r.contains("beta") && r.contains(effect) && !r.contains("checking sign-in"))
    });
    client.send(b"\r");
}

fn account_still_runs(env: &Env, pane: &str, message: &str) {
    env.run(&["send", "-p", pane, &format!("{message}\r")]);
    env.wait_for_text(pane, &format!("reply:{message}"));
    assert_eq!(std::fs::read_to_string(env.root.join("alpha/starts")).unwrap().lines().count(), 1);
    assert!(!env.root.join("beta/starts").exists());
}

#[test]
fn an_unsigned_target_offers_sign_in_without_stopping_the_agent() {
    let env = Env::new("switchlogin");
    let pane = account_switch_agent(&env, "claude", "notify");
    std::fs::write(env.root.join("beta/status"), "1").unwrap();
    let mut client = RawClient::attach_sized(&env, Some("w"), (80, 24)).traced();
    choose_switch_target(&mut client, "Sign-in required");
    client.wait_for_row("The current agent keeps running");
    client.send(b"\r");
    account_still_runs(&env, &pane, "cancel-sign-in");
    assert!(!env.root.join("beta/logins").exists());
    choose_switch_target(&mut client, "Sign-in required");
    client.wait_for_row("The current agent keeps running");
    client.send(b"y");
    wait_until(PATIENCE, "sign-in was offered and started", || env.root.join("beta/logins").exists());
    account_still_runs(&env, &pane, "during-sign-in");
}

#[test]
fn unverified_accounts_and_broken_transcripts_never_stop_the_current_agent() {
    use std::os::unix::fs::PermissionsExt;
    for reason in ["status", "missing", "unreadable", "diverged"] {
        let env = Env::new(&format!("switch-{reason}"));
        let pane = account_switch_agent(&env, "claude", "notify");
        let transcript =
            std::fs::read_dir(env.root.join("alpha/projects/work")).unwrap().next().unwrap().unwrap().path();
        let expected = match reason {
            "status" => {
                std::fs::write(env.root.join("beta/status"), "2").unwrap();
                "could not check sign-in"
            }
            "missing" => {
                std::fs::remove_file(&transcript).unwrap();
                "is missing"
            }
            "unreadable" => {
                std::fs::set_permissions(&transcript, std::fs::Permissions::from_mode(0o000)).unwrap();
                if std::fs::read(&transcript).is_ok() {
                    continue;
                }
                "reading"
            }
            _ => {
                std::fs::write(env.root.join("beta/projects/work").join(transcript.file_name().unwrap()), "diverged\n")
                    .unwrap();
                "each went on"
            }
        };
        let mut client = RawClient::attach_sized(&env, Some("w"), (124, 36)).traced();
        choose_switch_target(&mut client, "Could not verify");
        client.wait_for_screen(expected);
        account_still_runs(&env, &pane, reason);
        if reason == "status" {
            client.wait_for_row("Check again");
            std::fs::write(env.root.join("beta/status"), "0").unwrap();
            client.send(b"y");
            client.wait_for_row("Stop and switch?");
            client.send(b"\r");
            account_still_runs(&env, &pane, "after-check-again");
        }
    }
}

#[test]
fn cancelling_a_check_from_another_client_ignores_its_late_success() {
    let env = Env::new("switchcheckcancel");
    let pane = account_switch_agent(&env, "claude", "notify");
    let hold = env.root.join("beta/hold-check");
    std::fs::write(&hold, "").unwrap();
    let mut first = RawClient::attach(&env, "w").traced();
    let mut second = RawClient::attach(&env, "w").traced();
    first.send(b"\x02Ac");
    first.wait_for_row("Change account");
    first.send(b"beta");
    first.wait_for_row("checking sign-in");
    first.send(b"\r");
    wait_until(PATIENCE, "both the picker and change check sign-in", || {
        std::fs::read_to_string(env.root.join("beta/checks")).is_ok_and(|s| s.lines().count() == 2)
    });
    second.send(b"\x02Ac");
    second.wait_for_row("Checking sign-in");
    second.send(b"\r");
    second
        .wait_for_rows("the pending change is cancelled", |rows| !rows.iter().any(|r| r.contains("Checking sign-in")));
    std::fs::remove_file(hold).unwrap();
    wait_until(PATIENCE, "both checks returned", || {
        std::fs::read_to_string(env.root.join("beta/checked")).is_ok_and(|s| s.lines().count() == 2)
    });
    account_still_runs(&env, &pane, "late-success");
    first.send(b"\x02Ac");
    first.wait_for_row("Change account");
    first.send(b"beta");
    first.wait_for_row("Continue conversation");
}

#[test]
fn a_switch_waits_for_the_turn_is_cancelled_by_another_client_and_rechecks_sign_in() {
    let env = Env::new("switchturn");
    let pane = account_switch_agent(&env, "claude", "notify");
    claude_hook(&env, &pane, r#"{"hook_event_name":"UserPromptSubmit"}"#);
    let mut first = RawClient::attach_sized(&env, Some("w"), (124, 36)).traced();
    let mut second = RawClient::attach_sized(&env, Some("w"), (80, 24)).traced();
    choose_switch_target(&mut first, "Continue conversation");
    first.wait_for_row("Switch after this turn");
    first.send(b"\x02Ac");
    first.wait_for_row("Cancel account change");
    second.send(b"\x02Ac");
    second.wait_for_row("Cancel account change");
    second.send(b"\r");
    first.wait_for_rows("cancellation reaches both clients", |rows| {
        !rows.iter().any(|r| r.contains("Cancel account change"))
    });
    account_still_runs(&env, &pane, "after-cancel");

    choose_switch_target(&mut first, "Continue conversation");
    // The cancelled change's notification may still show: wait for this change's pending row.
    first.send(b"\x02Ac");
    first.wait_for_rows("the new account change waits for the turn", |rows| {
        rows.iter().any(|r| r.contains("Cancel account change") && r.contains("Switch after this turn"))
    });
    std::fs::write(env.root.join("beta/status"), "1").unwrap();
    claude_hook(&env, &pane, r#"{"hook_event_name":"Stop"}"#);
    first.wait_for_row("The current agent keeps running");
    first.send(b"\r");
    account_still_runs(&env, &pane, "expired-while-waiting");

    std::fs::write(env.root.join("beta/status"), "0").unwrap();
    choose_switch_target(&mut first, "Continue conversation");
    wait_until(PATIENCE, "the same conversation starts on beta", || env.root.join("beta/starts").exists());
    let text = env.wait_for_text(&pane, "--resume").replace('\n', "");
    assert!(text.contains(&format!("home={}", env.root.join("beta").display())));
    assert_eq!(std::fs::read_to_string(env.root.join("beta/starts")).unwrap().lines().count(), 1);
    let transcript = std::fs::read_dir(env.root.join("alpha/projects/work")).unwrap().next().unwrap().unwrap().path();
    assert_eq!(
        std::fs::read(&transcript).unwrap(),
        std::fs::read(env.root.join("beta/projects/work").join(transcript.file_name().unwrap())).unwrap()
    );
}

#[test]
fn codex_discloses_the_new_conversation_and_stop_confirmation_selects_cancel() {
    let env = Env::new("switchcodex");
    let pane = account_switch_agent(&env, "codex", "notify");
    let mut client = RawClient::attach_sized(&env, Some("w"), (80, 24)).traced();
    choose_switch_target(&mut client, "New conversation");
    client.wait_for_row("Stop and switch?");
    client.wait_for_row("New conversation");
    client.send(b"\r");
    account_still_runs(&env, &pane, "cancel-stop");
    choose_switch_target(&mut client, "New conversation");
    client.wait_for_row("Stop and switch?");
    client.send(b"y");
    wait_until(PATIENCE, "Codex starts on beta", || env.root.join("beta/starts").exists());
    env.wait_for_text(&pane, &format!("home={} args=", env.root.join("beta").display()));
    assert_eq!(std::fs::read_to_string(env.root.join("beta/starts")).unwrap().lines().count(), 1);
}

#[test]
fn frame_and_bottom_bar_open_the_same_change_and_claude_undo_continues_the_latest_history() {
    for size in [(80, 24), (124, 36)] {
        let env = Env::new(&format!("switch-undo-{}", size.0));
        let pane = account_switch_agent(&env, "claude", "notify");
        let mut client = RawClient::attach_sized(&env, Some("w"), size);
        click_account_text(&mut client, "switch account");
        client.wait_for_row("Change account");
        client.send(b"\x1b");
        // Two panes make the frame visible even with the compact single-pane setting.
        // Stack them so the account details fit at both terminal widths.
        env.run(&["run", "--down", "-n", "beside", "--", "sh"]);
        env.run(&["focus", &pane]);
        claude_hook(&env, &pane, r#"{"hook_event_name":"Stop"}"#);
        let rows = client.wait_for_rows("the account in the frame", |rows| {
            rows.iter().any(|r| r.contains("claude·alpha") && r.contains('─'))
        });
        let y = rows.iter().position(|r| r.contains("claude·alpha") && r.contains('─')).unwrap();
        let x = rows[y][..rows[y].find("claude·alpha").unwrap()].chars().count();
        client.send(format!("\x1b[<0;{};{}M\x1b[<0;{};{}m", x + 1, y + 1, x + 1, y + 1).as_bytes());
        client.wait_for_row("Change account");
        client.send(b"beta");
        client.wait_for_row("Continue conversation");
        client.send(b"\r");
        client.wait_for_row("Using beta · Undo");
        let transcript =
            std::fs::read_dir(env.root.join("beta/projects/work")).unwrap().next().unwrap().unwrap().path();
        use std::io::Write;
        std::fs::OpenOptions::new().append(true).open(&transcript).unwrap().write_all(b"{\"message\":2}\n").unwrap();
        claude_hook(&env, &pane, r#"{"hook_event_name":"Stop"}"#);
        click_account_text(&mut client, "Undo");
        client.wait_for_row("Account change undone");
        let original = env.root.join("alpha/projects/work").join(transcript.file_name().unwrap());
        assert_eq!(std::fs::read(original).unwrap(), std::fs::read(transcript).unwrap());
        let starts = std::fs::read_to_string(env.root.join("alpha/starts")).unwrap();
        assert_eq!(starts.lines().count(), 2);
        assert_eq!(std::fs::read_to_string(env.root.join("beta/starts")).unwrap().lines().count(), 1);
    }
}

#[test]
fn codex_undo_from_pane_actions_reopens_the_original_and_keeps_the_new_conversation() {
    let env = Env::new("codex-undo");
    let pane = account_switch_agent(&env, "codex", "notify");
    let mut client = RawClient::attach(&env, "w");
    choose_switch_target(&mut client, "New conversation");
    client.wait_for_row("Stop and switch?");
    client.send(b"y");
    client.wait_for_row("Using beta · Undo");
    let newer = env.root.join("beta/sessions/2026/10/13/rollout-today-conversation-beta.jsonl");
    let newer_contents = std::fs::read(&newer).unwrap();
    // The action remains after its notification is dismissed.
    client.send(b"\x02p");
    client.send(b"codex\t");
    client.wait_for_row("Undo account change");
    client.send(b"\x1b[B\x1b[B\r");
    client.wait_for_row("Stop and switch?");
    client.wait_for_row("Continue conversation");
    client.send(b"y");
    env.wait_for_text(&pane, "args=resume conversation-alpha");
    client.wait_for_row("Account change undone");
    assert_eq!(std::fs::read(newer).unwrap(), newer_contents);
    assert_eq!(std::fs::read_to_string(env.root.join("beta/starts")).unwrap().lines().count(), 1);
    for name in ["alpha", "beta"] {
        assert!(env.root.join(format!("{name}/sessions/2026/10/13/rollout-today-conversation-{name}.jsonl")).is_file());
    }
}

#[test]
fn a_replacement_that_exits_before_starting_resumes_the_source_once() {
    for recovery_fails in [false, true] {
        let env = Env::new(if recovery_fails { "switch-double-failure" } else { "switch-recovery" });
        let pane = account_switch_agent(&env, "claude", "notify");
        let mut client = RawClient::attach(&env, "w");
        std::fs::write(env.root.join("beta/fail-start"), "").unwrap();
        if recovery_fails {
            std::fs::write(env.root.join("alpha/fail-start"), "").unwrap();
        }
        claude_hook(&env, &pane, r#"{"hook_event_name":"Stop"}"#);
        choose_switch_target(&mut client, "Continue conversation");
        if recovery_fails {
            client.wait_for_row("Couldn't resume.");
        } else {
            client.wait_for_row("Resumed on alpha.");
            env.run(&["send", "-p", &pane, "still-here\r"]);
            env.wait_for_text(&pane, "reply:still-here");
        }
        assert_eq!(std::fs::read_to_string(env.root.join("alpha/starts")).unwrap().lines().count(), 2);
        assert_eq!(std::fs::read_to_string(env.root.join("beta/starts")).unwrap().lines().count(), 1);
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let info = panes.as_array().unwrap().iter().find(|p| p["id"].as_u64() == pane.parse::<u64>().ok()).unwrap();
        assert_eq!(info["account"], "claude:alpha");
        assert_eq!(!info["exit_code"].is_null(), recovery_fails);
    }
}

#[test]
fn a_notify_threshold_opens_change_for_the_pane_that_reached_it() {
    let env = Env::new("switch-notify-action");
    let pane = account_switch_agent(&env, "claude", "notify");
    let mut client = RawClient::attach_sized(&env, Some("w"), (80, 24));
    env.run(&["run", "-n", "beside", "--", "sh"]);
    write_account_usage(&env, "alpha", 95);
    client.wait_for_row("alpha is at 95% used · Change account");
    click_account_text(&mut client, "Change account");
    client.wait_for_row("Change account");
    client.wait_for_row("current · Continue conversation");
    client.send(b"\x1b");
    account_still_runs(&env, &pane, "only-notified");
}

#[test]
fn a_shell_agent_can_switch_only_with_an_exact_account_and_saved_conversation() {
    for agent in ["claude", "codex"] {
        let env = Env::new(&format!("manual-switch-{agent}"));
        account_switch_agent(&env, agent, "notify");
        let mut client = RawClient::attach(&env, "w");
        env.run(&["focus", "shell"]);
        let variable = if agent == "claude" { "CLAUDE_CONFIG_DIR" } else { "CODEX_HOME" };
        let args = if agent == "claude" { "--session-id manual-session" } else { "" };
        let command = format!(
            "{variable}='{}' '{}' {args}\r",
            env.root.join("alpha").display(),
            env.root.join(format!("bin/{agent}")).display()
        );
        env.run(&["send", "-p", "shell", &command]);
        env.wait_for_text("shell", "READY");
        if agent == "codex" {
            // The fake reports its conversation id through the hook after its first turn.
            env.run(&["send", "-p", "shell", "first turn\r"]);
            env.wait_for_text("shell", "reply:first turn");
        }
        client.wait_for_row("switch account");
        choose_switch_target(&mut client, if agent == "claude" { "Continue conversation" } else { "New conversation" });
        client.wait_for_row("Stop and switch?");
        client.send(b"y");
        client.wait_for_row("Using beta · Undo");
        env.run(&["send", "-p", "shell", "new-account\r"]);
        env.wait_for_text("shell", "reply:new-account");
        assert_eq!(
            std::fs::read_to_string(env.root.join("alpha/starts")).unwrap().lines().count(),
            2,
            "the original managed agent also keeps running"
        );
        assert_eq!(std::fs::read_to_string(env.root.join("beta/starts")).unwrap().lines().count(), 1);
    }
}

#[test]
fn an_ambiguous_shell_agent_offers_another_pane_without_stopping_it() {
    for reason in ["missing-session", "credential", "wrapper", "background-job"] {
        let env = Env::new(&format!("manual-refused-{reason}"));
        account_switch_agent(&env, "claude", "notify");
        let mut client = RawClient::attach(&env, "w");
        env.run(&["focus", "shell"]);
        let program = env.root.join("bin/claude");
        let args = if reason == "missing-session" { "" } else { "--session-id manual-session" };
        let extra = if reason == "credential" { "ANTHROPIC_API_KEY=override " } else { "" };
        if reason == "background-job" {
            env.run(&["send", "-p", "shell", "sleep 60 &\r"]);
        }
        let command = if reason == "wrapper" {
            format!(
                "CLAUDE_CONFIG_DIR='{}' sh -c '\"{}\" {args} </dev/tty & wait'\r",
                env.root.join("alpha").display(),
                program.display()
            )
        } else {
            format!("{extra}CLAUDE_CONFIG_DIR='{}' '{}' {args}\r", env.root.join("alpha").display(), program.display())
        };
        env.run(&["send", "-p", "shell", &command]);
        env.wait_for_text("shell", "READY");
        client.send(b"\x02Ac");
        client.wait_for_row("Can't switch this process.");
        client.wait_for_row("Open with another account");
        assert_eq!(std::fs::read_to_string(env.root.join("alpha/starts")).unwrap().lines().count(), 2);
        assert!(!env.root.join("beta/starts").exists());
        client.send(b"\r");
        client.wait_for_row("Open with another account");
        client.send(b"beta\r");
        env.wait_for_text("shell", "READY");
        wait_until(PATIENCE, "another pane starts on beta", || env.root.join("beta/starts").exists());
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let original = panes.as_array().unwrap().iter().find(|p| p["name"] == "shell").unwrap();
        assert!(original["exit_code"].is_null());
        assert!(original["account"].is_null());
    }
}

#[test]
fn switching_waits_for_the_replacement_to_produce_output_before_offering_undo() {
    let env = Env::new("switch-start-signal");
    let pane = account_switch_agent(&env, "claude", "notify");
    let mut client = RawClient::attach(&env, "w");
    std::fs::write(env.root.join("beta/hold-start"), "").unwrap();
    claude_hook(&env, &pane, r#"{"hook_event_name":"Stop"}"#);
    choose_switch_target(&mut client, "Continue conversation");
    wait_until(PATIENCE, "the replacement was spawned", || env.root.join("beta/starts").exists());
    std::thread::sleep(Duration::from_secs(2));
    assert!(!client.rows().iter().any(|r| r.contains("Using beta") || r.contains("Switched")));
    std::fs::remove_file(env.root.join("beta/hold-start")).unwrap();
    client.wait_for_row("Using beta · Undo");
}

fn write_account_usage(env: &Env, name: &str, percent: u8) {
    let dir = env.root.join("state/kraal/usage/by-id");
    std::fs::create_dir_all(&dir).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let file: toml::Value =
        toml::from_str(&std::fs::read_to_string(env.root.join("config/kraal/accounts.toml")).unwrap()).unwrap();
    let id = file["account"].as_array().unwrap().iter().find(|a| a["name"].as_str() == Some(name)).unwrap()["id"]
        .as_str()
        .unwrap();
    std::fs::write(
        dir.join(format!("claude-{id}.json")),
        serde_json::json!({
            "five_hour": { "used_percent": percent, "resets_at": now + 3600 },
            "weekly": null, "observed_at": now, "source": "statusline"
        })
        .to_string(),
    )
    .unwrap();
}

#[test]
fn threshold_ask_checks_the_destination_then_needs_one_confirmation() {
    let env = Env::new("switchask");
    let pane = account_switch_agent(&env, "claude", "ask");
    let mut client = RawClient::attach(&env, "w").traced();
    claude_hook(&env, &pane, r#"{"hook_event_name":"Stop"}"#);
    write_account_usage(&env, "beta", 0);
    write_account_usage(&env, "alpha", 95);
    client.wait_for_row("reached its usage threshold");
    client.wait_for_row("Stop and switch?");
    assert!(env.root.join("beta/checked").exists());
    assert!(!env.root.join("beta/starts").exists());
    client.send(b"y");
    wait_until(PATIENCE, "one confirmation starts the verified alternative", || env.root.join("beta/starts").exists());
    env.wait_for_text(&pane, "--resume");
    assert_eq!(std::fs::read_to_string(env.root.join("beta/starts")).unwrap().lines().count(), 1);
}

#[test]
fn automatic_switches_need_known_usage_and_do_not_retry_a_failed_check_each_tick() {
    let env = Env::new("switchauto");
    let pane = account_switch_agent(&env, "claude", "auto");
    let client = RawClient::attach(&env, "w").traced();
    claude_hook(&env, &pane, r#"{"hook_event_name":"Stop"}"#);
    let usage = |name, percent| write_account_usage(&env, name, percent);
    usage("alpha", 95);
    std::thread::sleep(Duration::from_secs(3));
    assert!(!env.root.join("beta/checks").exists(), "unknown usage is not free capacity");
    account_still_runs(&env, &pane, "unknown-usage");
    std::fs::write(env.root.join("beta/status"), "1").unwrap();
    usage("beta", 0);
    wait_until(PATIENCE, "the automatic switch checks sign-in", || env.root.join("beta/checks").exists());
    client.wait_for_row("The current agent keeps running");
    std::thread::sleep(Duration::from_secs(3));
    let checks = std::fs::read_to_string(env.root.join("beta/checks")).unwrap();
    assert_eq!(checks.lines().count(), 1);
    account_still_runs(&env, &pane, "failed-auto");
    std::fs::write(env.root.join("beta/status"), "0").unwrap();
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(std::fs::read_to_string(env.root.join("beta/checks")).unwrap(), checks);
    account_still_runs(&env, &pane, "no-retry");
}

/// Native login and status commands controlled by files in the isolated account home.
fn login_agent(env: &Env, agent: &str) -> (PathBuf, String) {
    let bin = env.root.join("bin");
    let home = env.root.join("account");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    write_script(
        &bin.join(agent),
        r#"#!/bin/sh
home="${CLAUDE_CONFIG_DIR:-${CODEX_HOME:-}}"
case "$*" in
    'auth status'|'login status')
        test -d "$home" || exit 1
        code=$(cat "$home/status" 2>/dev/null) || code=1
        echo check >> "$home/checks"
        while test -f "$home/hold-check"; do sleep 0.05; done
        echo checked >> "$home/checks"
        printf 'PRIVATE-LOGIN-OUTPUT\n' >&2
        exit "$code"
        ;;
    'auth login'|'login')
        echo login >> "$home/logins"
        printf 'PRIVATE-LOGIN-OUTPUT\n\033]0;PRIVATE-LOGIN-OUTPUT\007\033]777;notify;PRIVATE-LOGIN-OUTPUT;secret\007'
        while test ! -f "$home/finish-login"; do sleep 0.05; done
        exit "$(cat "$home/finish-login")"
        ;;
    *) exit 1 ;;
esac
"#,
    );
    std::fs::write(
        env.root.join("config/kraal/accounts.toml"),
        format!("[[account]]\nname = \"work\"\nagent = {agent:?}\nhome = {:?}\n", home.to_str().unwrap()),
    )
    .unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    env.workspace("w", "layout { pane name=\"shell\" }\n");
    let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    (home, path)
}

fn open_account_login(client: &mut RawClient) {
    client.send(b"\x02Al");
    client.wait_for_row("Accounts");
    client.send(b"work");
    client.wait_for_row("work");
    client.send(b"\x0c");
    client.wait_for_row("login ");
}

fn login_panes(env: &Env) -> Vec<serde_json::Value> {
    let panes: Vec<serde_json::Value> = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    panes.into_iter().filter(|p| p["name"].as_str().is_some_and(|s| s.starts_with("login "))).collect()
}

#[test]
fn login_panes_verify_success_once_and_never_save_the_terminal() {
    for agent in ["claude", "codex"] {
        let env = Env::new(&format!("login-{agent}"));
        let (home, path) = login_agent(&env, agent);
        let mut client = RawClient::attach_sized(&env, Some("w"), (124, 36)).traced();
        open_account_login(&mut client);
        client.wait_for_row("PRIVATE-LOGIN-OUTPUT");
        let first = login_panes(&env);
        assert_eq!(first.len(), 1);
        let mut second = RawClient::attach_sized(&env, Some("w"), (80, 24)).traced();
        open_account_login(&mut second);
        assert_eq!(login_panes(&env)[0]["id"], first[0]["id"], "another client must reuse the login");
        assert_eq!(std::fs::read_to_string(home.join("logins")).unwrap(), "login\n");
        env.run(&["save", "saved", "--workspace", "w"]);
        let layout = std::fs::read_to_string(env.root.join("config/kraal/workspaces/saved.kdl")).unwrap();
        // The shell's cwd contains "login" too: check the pane name and command instead.
        assert!(!layout.contains("name=\"login "), "{layout}");
        assert!(!layout.contains("command="), "{layout}");
        assert!(!layout.contains("PRIVATE-LOGIN-OUTPUT"), "{layout}");
        std::fs::write(home.join("hold-check"), "").unwrap();
        std::fs::write(home.join("status"), "0").unwrap();
        std::fs::write(home.join("finish-login"), "0").unwrap();
        second.wait_for_row("checking sign-in");
        assert_eq!(login_panes(&env).len(), 1, "exit zero alone must not close the pane");
        let duplicate = env.cmd(&["accounts", "login", &format!("{agent}:work")]).env("PATH", &path).output().unwrap();
        assert!(!duplicate.status.success());
        assert!(String::from_utf8_lossy(&duplicate.stderr).contains("sign-in already running"));
        assert_eq!(std::fs::read_to_string(home.join("logins")).unwrap(), "login\n");
        std::fs::remove_file(home.join("hold-check")).unwrap();
        second.wait_for_row(&format!("{agent}·work is signed in"));
        common::wait_until(PATIENCE, "verified login closes", || login_panes(&env).is_empty());
        second.send(b"\r");
        assert_eq!(std::fs::read_to_string(home.join("logins")).unwrap(), "login\n");
        assert!(
            !std::fs::read_to_string(env.root.join("config/kraal/accounts.toml")).unwrap().contains("sign_in_required")
        );

        // A live login must also be excluded when the server saves on exit.
        std::fs::remove_file(home.join("finish-login")).unwrap();
        open_account_login(&mut second);
        second.wait_for_row("PRIVATE-LOGIN-OUTPUT");
        second.send(b"\x02Vf\x02Vj");
        let messages = second.wait_for_row("Messages").join("\n");
        assert!(!messages.contains("PRIVATE-LOGIN-OUTPUT"), "{messages}");
        env.run(&["kill"]);
        fn private_files(dir: &Path) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    private_files(&path);
                } else {
                    let text = String::from_utf8_lossy(&std::fs::read(&path).unwrap()).into_owned();
                    assert!(!text.contains("PRIVATE-LOGIN-OUTPUT"), "{}: {text}", path.display());
                    if path.extension().is_some_and(|e| e == "json") {
                        assert!(!text.contains("login "), "{}: {text}", path.display());
                    }
                }
            }
        }
        private_files(&env.root.join("state"));
    }
}

#[test]
fn failed_and_cancelled_logins_close_on_enter_and_retry_only_when_asked() {
    let env = Env::new("login-failed");
    let (home, path) = login_agent(&env, "claude");
    let mut client = RawClient::attach_sized(&env, Some("w"), (80, 24)).traced();
    std::fs::write(home.join("finish-login"), "1").unwrap();
    open_account_login(&mut client);
    client.wait_for_row("sign-in failed");
    client.wait_for_row("enter close · r try again");
    assert!(!home.join("checks").exists(), "failure does not run a status check");
    client.send(b"\r");
    common::wait_until(PATIENCE, "Enter closes failed login", || login_panes(&env).is_empty());
    assert_eq!(std::fs::read_to_string(home.join("logins")).unwrap(), "login\n");
    client.resize((124, 36));
    client.wait_for_row("Sign-in required");
    client.send(b"\x02Al");
    client.wait_for_row("Claude · work");
    client.wait_for_row("Sign-in required");
    client.close_overlay("Enter Details");

    std::fs::remove_file(home.join("finish-login")).unwrap();
    open_account_login(&mut client);
    client.wait_for_row("PRIVATE-LOGIN-OUTPUT");
    client.send(b"\x03");
    client.wait_for_row("sign-in cancelled");
    common::wait_until(PATIENCE, "cancelled program exits", || !login_panes(&env)[0]["exit_code"].is_null());
    client.send(b"r");
    common::wait_until(PATIENCE, "explicit retry starts once", || {
        let panes = login_panes(&env);
        std::fs::read_to_string(home.join("logins")).unwrap().lines().count() == 3
            && panes.len() == 1
            && panes[0]["exit_code"].is_null()
    });
    // Running logins show the native terminal, with no banner. Acknowledge the retry's
    // redraw before finishing it, so the old cancelled banner cannot satisfy the next wait.
    client.wait_for_rows("the retried login is running", |rows| {
        rows.iter().any(|r| r.contains("PRIVATE-LOGIN-OUTPUT")) && !rows.iter().any(|r| r.contains("sign-in cancelled"))
    });
    std::fs::write(home.join("finish-login"), "130").unwrap();
    client.wait_for_row("sign-in cancelled");
    client.send(b"\r");
    common::wait_until(PATIENCE, "Enter closes cancelled login", || login_panes(&env).is_empty());
    assert_eq!(std::fs::read_to_string(home.join("logins")).unwrap().lines().count(), 3);
    assert!(!home.join("checks").exists(), "cancellation does not run a status check");
    assert!(
        std::fs::read_to_string(env.root.join("config/kraal/accounts.toml"))
            .unwrap()
            .contains("sign_in_required = true")
    );
    // Listing accounts explicitly checks sign-in: do it after checking that neither
    // failure nor cancellation ran a status command.
    let out = env.cmd(&["accounts"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|l| l.contains("claude·work") && l.contains("sign-in required"))
    );
}

#[test]
fn an_unverified_login_can_check_again_without_another_login() {
    let env = Env::new("login-check");
    let (home, _) = login_agent(&env, "codex");
    let mut client = RawClient::attach_sized(&env, Some("w"), (80, 24)).traced();
    std::fs::write(home.join("finish-login"), "0").unwrap();
    std::fs::write(home.join("status"), "1").unwrap();
    open_account_login(&mut client);
    client.wait_for_row("sign-in required");
    client.wait_for_row("c check again");
    std::fs::write(home.join("status"), "2").unwrap();
    client.send(b"c");
    client.wait_for_row("could not check sign-in");
    std::fs::write(home.join("status"), "0").unwrap();
    client.send(b"c");
    client.wait_for_row("codex·work is signed in");
    assert_eq!(std::fs::read_to_string(home.join("logins")).unwrap(), "login\n");
    common::wait_until(PATIENCE, "verified retry closes", || login_panes(&env).is_empty());
}

#[test]
fn a_verified_login_that_cannot_save_can_finish_without_logging_in_again() {
    let env = Env::new("login-save");
    let (home, _) = login_agent(&env, "claude");
    let mut client = RawClient::attach(&env, "w").traced();
    std::fs::write(home.join("status"), "0").unwrap();
    open_account_login(&mut client);
    client.wait_for_row("PRIVATE-LOGIN-OUTPUT");
    let file = env.root.join("config/kraal/accounts.toml");
    let backup = file.with_extension("kept");
    std::fs::rename(&file, &backup).unwrap();
    std::fs::create_dir(&file).unwrap();
    // Make saving fail before the login exits, without holding a check against its timeout.
    std::fs::write(home.join("finish-login"), "0").unwrap();
    client.wait_for_row("signed in, could not save account");
    assert_eq!(login_panes(&env).len(), 1);
    assert!(std::fs::read_to_string(&backup).unwrap().contains("sign_in_required = true"));
    std::fs::remove_dir(&file).unwrap();
    std::fs::rename(&backup, &file).unwrap();
    client.send(b"c");
    client.wait_for_row("claude·work is signed in");
    assert!(login_panes(&env).is_empty(), "a saved login must be closed");
    assert_eq!(std::fs::read_to_string(home.join("logins")).unwrap(), "login\n");
    assert_eq!(std::fs::read_to_string(home.join("checks")).unwrap(), "check\nchecked\ncheck\nchecked\n");
    assert!(!std::fs::read_to_string(file).unwrap().contains("sign_in_required"));
}

#[test]
fn closing_a_check_ignores_its_late_success() {
    let env = Env::new("login-late");
    let (home, _) = login_agent(&env, "claude");
    let mut client = RawClient::attach(&env, "w").traced();
    std::fs::write(home.join("finish-login"), "0").unwrap();
    std::fs::write(home.join("status"), "0").unwrap();
    std::fs::write(home.join("hold-check"), "").unwrap();
    open_account_login(&mut client);
    client.wait_for_row("checking sign-in");
    common::wait_until(PATIENCE, "check is running", || home.join("checks").exists());
    client.send(b"\r");
    common::wait_until(PATIENCE, "checking pane closes", || login_panes(&env).is_empty());
    std::fs::write(home.join("status"), "2").unwrap();
    open_account_login(&mut client);
    client.wait_for_row("checking sign-in");
    common::wait_until(PATIENCE, "the replacement check read its status", || {
        std::fs::read_to_string(home.join("checks")).is_ok_and(|s| s.lines().filter(|l| *l == "check").count() == 2)
    });
    std::fs::remove_file(home.join("hold-check")).unwrap();
    client.wait_for_row("could not check sign-in");
    assert_eq!(login_panes(&env).len(), 1);
    assert!(
        std::fs::read_to_string(env.root.join("config/kraal/accounts.toml"))
            .unwrap()
            .contains("sign_in_required = true")
    );
    assert!(!client.rows().iter().any(|r| r.contains("is signed in")));
}

fn existing_sign_in_agent(env: &Env, agent: &str) -> String {
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(
        &bin.join(agent),
        r#"#!/bin/sh
case "$0" in
    *claude) home="${CLAUDE_CONFIG_DIR:-$HOME/.claude}" ;;
    *codex) home="${CODEX_HOME:-$HOME/.codex}" ;;
esac
case "$*" in
    'auth status'|'login status')
        echo "$home" >> "$HOME/status-checks"
        test -f "$home/status" || exit 1
        while test -f "$home/hold-check"; do sleep 0.05; done
        echo checked >> "$home/check-finished"
        test ! -f "$home/public-status" || cat "$home/public-status"
        exit "$(cat "$home/status")"
        ;;
    'auth login'|'login') echo unexpected >> "$HOME/logins"; exit 2 ;;
    app-server*) exit 1 ;;
esac
echo "$$" >> "$home/starts"
if [ "$1" = --session-id ]; then
    mkdir -p "$home/projects/work"
    printf '{"message":1}\n' > "$home/projects/work/$2.jsonl"
fi
echo "READY home=$home"
while IFS= read -r line; do printf 'reply:%s\n' "$line"; done
"#,
    );
    format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default())
}

fn wait_for_account_review(client: &RawClient, agent: &str) -> Vec<String> {
    let label = format!("Existing {agent} account");
    client.wait_for_rows(&format!("{label} ready for Review"), |rows| {
        rows.iter().enumerate().any(|(i, row)| {
            row.contains(&label)
                && (row.contains("Review") || rows.get(i + 1).is_some_and(|detail| detail.contains("Review")))
        })
    })
}

#[test]
fn account_discovery_updates_each_agent_without_waiting_for_the_other() {
    let env = Env::new("accounts-concurrent");
    let path = existing_sign_in_agent(&env, "claude");
    existing_sign_in_agent(&env, "codex");
    for agent in ["claude", "codex"] {
        let home = env.root.join(format!("home/.{agent}"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("status"), "0").unwrap();
    }
    // The server unit test controls completion order by holding job results. Holding a
    // status subprocess here would race its watchdog while the client and shell start.
    env.workspace("w", "layout { pane name=\"shell\" }\n");
    let out = env.cmd(&["open", "w"]).env("PATH", path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mut client = RawClient::attach_sized(&env, Some("w"), (80, 24)).traced();
    client.send(b"\x02Al");
    client.wait_for_row("Codex · Personal");
    env.run(&["send", "-p", "shell", "printf 'server-%s\\n' responsive\r"]);
    env.wait_for_text("shell", "server-responsive");
    client.wait_for_row("Claude · Personal");
    let checks = std::fs::read_to_string(env.root.join("home/status-checks")).unwrap();
    assert_eq!(checks.lines().count(), 2);
    for _ in 0..3 {
        client.close_overlay("Enter Details");
        client.send(b"\x02Al");
        client.wait_for_row("Claude · Personal");
        client.wait_for_row("Codex · Personal");
    }
    assert_eq!(std::fs::read_to_string(env.root.join("home/status-checks")).unwrap(), checks);
    assert!(!env.root.join("home/logins").exists());
}

#[test]
fn existing_sign_ins_are_verified_adopted_and_renamed_without_login_or_credential_copy() {
    for agent in ["claude", "codex"] {
        let env = Env::new(&format!("adopt-{agent}"));
        let path = existing_sign_in_agent(&env, agent);
        let home = env.root.join(format!("home/.{agent}"));
        let run = |args: &[&str]| {
            let out = env.cmd(args).env("PATH", &path).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8(out.stdout).unwrap()
        };
        let list = || serde_json::from_str::<serde_json::Value>(&run(&["accounts", "list", "--json"])).unwrap();
        assert_eq!(list(), serde_json::json!([]), "unsigned and absent agents add no accounts");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("credential"), "PRIVATE-CREDENTIAL").unwrap();
        for (status, count) in [("1", 0), ("2", 1), ("0", 1)] {
            std::fs::write(home.join("status"), status).unwrap();
            if agent == "claude" {
                std::fs::write(home.join("public-status"), r#"{"loggedIn":true,"email":"me@example.com"}"#).unwrap();
            }
            let listed = list();
            assert_eq!(listed.as_array().unwrap().len(), count);
            if status == "2" {
                assert_eq!(listed[0]["status"], "could_not_check");
                assert!(!env.root.join("config/kraal/accounts.toml").exists());
                assert!(run(&["accounts", "list"]).contains("check again"));
            }
            if status == "0" {
                assert_eq!(listed[0]["name"], if agent == "claude" { "me@example.com" } else { "Personal" });
                assert_eq!(listed[0]["status"], "signed_in");
            }
        }
        run(&["accounts", "rename", &format!("{agent}:default"), "Mine"]);
        assert_eq!(list()[0]["name"], "Mine");
        assert_eq!(list().as_array().unwrap().len(), 1);
        assert!(!env.root.join("home/logins").exists());
        assert!(!env.root.join("home/.local/share/kraal/accounts").exists());
        assert_eq!(std::fs::read_to_string(home.join("credential")).unwrap(), "PRIVATE-CREDENTIAL");
        run(&["accounts", "rm", &format!("{agent}:Mine")]);
        assert_eq!(list(), serde_json::json!([]), "forgotten standard homes are not rediscovered");
        assert!(home.join("credential").exists());
    }
}

#[test]
fn accounts_add_adopts_a_verified_home_and_aliases_do_not_add_an_account() {
    for explicit in [false, true] {
        let env = Env::new(if explicit { "adopt-home" } else { "adopt-add" });
        let path = existing_sign_in_agent(&env, "claude");
        let home = env.root.join("home/.claude");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("status"), "0").unwrap();
        std::fs::write(home.join("credential"), "kept").unwrap();
        let mut args = vec!["accounts", "add", "claude", "Mine"];
        if explicit {
            args.extend(["--home", home.to_str().unwrap()]);
        }
        let out = env.cmd(&args).env("PATH", &path).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(!String::from_utf8_lossy(&out.stdout).contains("logging in"));
        let alias = env.root.join("alias");
        std::os::unix::fs::symlink(&home, &alias).unwrap();
        let out = env
            .cmd(&["accounts", "add", "claude", "Another", "--home", alias.to_str().unwrap()])
            .env("PATH", &path)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let out = env.cmd(&["accounts", "list", "--json"]).env("PATH", &path).output().unwrap();
        let list: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert_eq!(list[0]["name"], "Mine");
        assert!(!env.root.join("home/logins").exists());
        assert_eq!(std::fs::read_to_string(home.join("credential")).unwrap(), "kept");
        let out = env.cmd(&["accounts", "rename", "Another", "Personal"]).env("PATH", &path).output().unwrap();
        assert!(out.status.success(), "the old alias is still usable");
    }
}

#[test]
fn accounts_add_checks_the_home_chosen_by_review() {
    for signed_in in [false, true] {
        let env = Env::new(&format!("add-reviewed-{signed_in}"));
        let path = existing_sign_in_agent(&env, "claude");
        let standard = env.root.join("home/.claude");
        let work = env.root.join("work");
        for home in [&standard, &work] {
            std::fs::create_dir_all(home).unwrap();
        }
        std::fs::write(standard.join("status"), if signed_in { "1" } else { "0" }).unwrap();
        std::fs::write(work.join("status"), if signed_in { "0" } else { "1" }).unwrap();
        let file = env.root.join("config/kraal/accounts.toml");
        std::fs::write(&file, format!(
            "[[account]]\nid = 'default-claude'\nagent = 'claude'\nname = 'Personal'\nhome = '~/.claude'\nlinked_to = 'work-id'\n\n[[account]]\nid = 'work-id'\nagent = 'claude'\nname = 'work'\nhome = {:?}\nsign_in_required = true\n",
            work.to_str().unwrap()
        )).unwrap();
        let out = env
            .cmd(&["accounts", "add", "claude", "Another", "--home", "~/.claude"])
            .env("PATH", &path)
            .output()
            .unwrap();
        assert_eq!(out.status.success(), signed_in, "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(env.root.join("home/logins").exists(), !signed_in, "only a signed-out target needs login");
        assert_eq!(
            std::fs::read_to_string(env.root.join("home/status-checks")).unwrap().trim(),
            work.to_str().unwrap()
        );
        let saved: toml::Value = toml::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        let work = saved["account"].as_array().unwrap().iter().find(|a| a["id"].as_str() == Some("work-id")).unwrap();
        assert_eq!(work.get("sign_in_required").and_then(toml::Value::as_bool).unwrap_or(false), !signed_in);
    }
}

#[test]
fn accounts_add_revalidates_the_checked_identity_and_home_before_saving() {
    for change in ["identity", "home"] {
        let env = Env::new(&format!("add-checked-{change}"));
        let path = existing_sign_in_agent(&env, "claude");
        let file = env.root.join("config/kraal/accounts.toml");
        let source = "[[account]]\nid = 'default-claude'\nagent = 'claude'\nname = 'Personal'\nhome = '~/.claude'\nlinked_to = 'work-id'\n\n[[account]]\nid = 'work-id'\nagent = 'claude'\nname = 'work'\nhome = '~/work'\nsign_in_required = true\n";
        std::fs::write(&file, source).unwrap();
        let changed = if change == "identity" {
            source.replace("work-id", "replacement-id")
        } else {
            source.replace("~/work", "~/moved")
        };
        std::fs::write(env.root.join("home/replacement"), &changed).unwrap();
        write_script(
            &env.root.join("bin/claude"),
            r#"#!/bin/sh
[ "$1 $2" = 'auth status' ] || exit 2
cat "$HOME/replacement" > "$XDG_CONFIG_HOME/kraal/accounts.toml"
exit 0
"#,
        );
        let out = env
            .cmd(&["accounts", "add", "claude", "Another", "--home", "~/.claude"])
            .env("PATH", &path)
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("account changed while checking"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), changed, "the stale check saves nothing");
    }
}

#[test]
fn accounts_refresh_discovers_the_standard_codex_sign_in_and_fetches_usage() {
    let env = Env::new("refresh-existing-codex");
    let path = existing_sign_in_agent(&env, "codex");
    let home = env.root.join("home/.codex");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("status"), "0").unwrap();
    write_script(
        &env.root.join("bin/codex"),
        r#"#!/bin/sh
home="${CODEX_HOME:-$HOME/.codex}"
case "$*" in
    'login status') exit "$(cat "$home/status")" ;;
    'app-server --listen stdio://') ;;
    *) exit 2 ;;
esac
echo "$home" > "$HOME/usage-home"
IFS= read -r line
printf '%s\n' '{"id":1,"result":{}}'
IFS= read -r line
IFS= read -r line
[ "$line" = '{"id":2,"method":"account/rateLimits/read","params":null}' ] || exit 3
printf '%s\n' '{"id":2,"result":{"rateLimits":{"primary":{"usedPercent":42,"windowDurationMins":300}}}}'
"#,
    );
    assert!(!env.root.join("config/kraal/accounts.toml").exists());
    let out = env.cmd(&["accounts", "refresh"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("42%"));
    assert_eq!(std::fs::read_to_string(env.root.join("home/usage-home")).unwrap().trim(), home.to_str().unwrap());
    let out = env.cmd(&["accounts", "list", "--json"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let list: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["name"], "Personal");
    assert_eq!(list[0]["usage"]["five_hour"]["used_percent"], 42.0);
}

#[test]
fn an_uncertain_existing_sign_in_never_starts_login_or_creates_an_account() {
    let env = Env::new("adopt-uncertain");
    let path = existing_sign_in_agent(&env, "claude");
    let home = env.root.join("home/.claude");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("status"), "2").unwrap();
    for explicit in [false, true] {
        let mut args = vec!["accounts", "add", "claude", "Mine"];
        if explicit {
            args.extend(["--home", home.to_str().unwrap()]);
        }
        let out = env.cmd(&args).env("PATH", &path).output().unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("could not verify"));
        assert!(!env.root.join("config/kraal/accounts.toml").exists());
        assert!(!env.root.join("home/logins").exists());
    }
    std::fs::write(home.join("status"), "0").unwrap();
    let out = env.cmd(&["accounts", "add", "claude", "Mine"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!env.root.join("home/logins").exists());
}

#[test]
fn command_line_review_can_keep_a_different_account_without_login() {
    let env = Env::new("review-cli");
    let path = existing_sign_in_agent(&env, "claude");
    let home = env.root.join("home/.claude");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("status"), "0").unwrap();
    let file = env.root.join("config/kraal/accounts.toml");
    std::fs::write(
        &file,
        format!(
            "[[account]]\nagent = 'claude'\nname = 'work'\nhome = {:?}\n",
            env.root.join("work").display().to_string()
        ),
    )
    .unwrap();
    let out = env.cmd(&["accounts", "list"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("Existing Claude account · Review"));
    let before = std::fs::read_to_string(&file).unwrap();
    let out = env.cmd(&["accounts", "review", "claude"]).env("PATH", &path).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("in a terminal"));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
    env.workspace("w", "layout { pane name=\"shell\" }\n");
    let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success());
    let pane = env.run(&["run", "--tab", "--", env!("CARGO_BIN_EXE_kraal"), "accounts", "review", "claude"]);
    let pane = pane.trim();
    env.wait_for_text(pane, "Which account is this?");
    env.wait_for_text(pane, "2. A different account");
    env.run(&["send", "-p", pane, "2\r"]);
    env.wait_for_text(pane, "Use Personal?");
    env.run(&["send", "-p", pane, "y\r"]);
    env.wait_for_text(pane, "Account updated");
    let out = env.cmd(&["accounts", "list", "--json"]).env("PATH", &path).output().unwrap();
    let list: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 2);
    assert!(list.as_array().unwrap().iter().any(|a| a["name"] == "Personal" && a["status"] == "signed_in"));
    assert!(!std::fs::read_to_string(&file).unwrap().contains("review = true"));
    assert!(!env.root.join("home/logins").exists());
}

#[test]
fn reviewing_the_existing_account_keeps_live_agents_and_reports_usage_once() {
    for size in [(124, 36), (80, 24)] {
        let env = Env::new(&format!("account-review-{}", size.0));
        let path = existing_sign_in_agent(&env, "claude");
        existing_sign_in_agent(&env, "codex");
        let codex = env.root.join("home/.codex");
        std::fs::create_dir_all(&codex).unwrap();
        std::fs::write(codex.join("status"), "0").unwrap();
        let standard = env.root.join("home/.claude");
        let mut config = String::from("# My subscriptions.\n");
        for name in ["patron.bzh", "gmail"] {
            let home = env.root.join(name);
            std::fs::create_dir_all(&home).unwrap();
            std::fs::write(home.join("status"), "0").unwrap();
            std::fs::write(home.join("credential"), name).unwrap();
            config.push_str(&format!(
                "[[account]]\nagent = 'claude'\nname = '{name}'\nhome = {:?}\n",
                home.to_str().unwrap()
            ));
        }
        config.push_str("[[rule]]\nworkspace = 'legacy'\nagent = 'claude'\naccounts = ['default']\n");
        let file = env.root.join("config/kraal/accounts.toml");
        std::fs::write(&file, &config).unwrap();
        std::fs::create_dir_all(&standard).unwrap();
        std::fs::write(standard.join("status"), "0").unwrap();
        std::fs::write(standard.join("credential"), "standard").unwrap();
        let usage = env.root.join("state/kraal/usage");
        std::fs::create_dir_all(&usage).unwrap();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        for (name, percent, at) in [("default", 57, now), ("patron.bzh", 40, now - 1)] {
            std::fs::write(
                usage.join(format!("claude-{name}.json")),
                serde_json::json!({
                    "five_hour": { "used_percent": percent, "resets_at": now + 3600 },
                    "weekly": null, "observed_at": at, "source": "statusline"
                })
                .to_string(),
            )
            .unwrap();
        }
        env.workspace("w", "layout { pane name=\"agent\" agent=\"claude:default\" }\n");
        let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let pane = panes.as_array().unwrap().iter().find(|p| p["name"] == "agent").unwrap()["id"].to_string();
        env.wait_for_text(&pane, "READY");
        let mut client = RawClient::attach_sized(&env, Some("w"), size).traced();
        if size.0 == 124 {
            client.wait_for_row("Codex · Personal");
            // The name is also shown while checking sign-in; only Review opens the review picker.
            let rows = wait_for_account_review(&client, "Claude");
            let row = rows.iter().position(|r| r.contains("Existing Claude account")).unwrap() + 1;
            client.send(format!("\x1b[<0;10;{row}M\x1b[<0;10;{row}m").as_bytes());
        } else {
            client.send(b"\x02Al");
            client.wait_for_row("Codex · Personal");
            wait_for_account_review(&client, "Claude");
            assert!(!client.rows().iter().any(|r| r.contains("claude·default") || r.contains("codex·default")));
            client.send(b"Existing Claude");
            client.send(b"\r");
        }
        client.wait_for_row("Which account is this?");
        client.wait_for_row("A different account");
        // Adoption already links the legacy default; cancelling the review must leave the file unchanged.
        let before_review = std::fs::read_to_string(&file).unwrap();
        client.send(b"\r");
        client.wait_for_row("Use patron.bzh?");
        client.wait_for_row("Your running agents will keep working.");
        client.send(b"\r");
        client.wait_for_row("Which account is this?");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before_review, "Enter cancels first");
        client.send(b"\r");
        client.wait_for_row("Use patron.bzh?");
        client.send(b"y");
        client.wait_for_row("Account updated");
        client.send(b"\x1b");
        env.run(&["send", "-p", &pane, "still-here\r"]);
        env.wait_for_text(&pane, "reply:still-here");
        assert_eq!(std::fs::read_to_string(standard.join("starts")).unwrap().lines().count(), 1);
        assert!(!env.root.join("patron.bzh/starts").exists());
        let out = env.cmd(&["accounts", "list", "--json"]).env("PATH", &path).output().unwrap();
        let list: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(list.as_array().unwrap().len(), 3);
        let saved = std::fs::read_to_string(&file).unwrap();
        let saved: toml::Value = toml::from_str(&saved).unwrap();
        assert_eq!(saved["account"].as_array().unwrap().len(), 3);
        assert!(saved["account"].as_array().unwrap().iter().all(|a| a.get("linked_to").is_none()));
        let patron = list.as_array().unwrap().iter().find(|a| a["name"] == "patron.bzh").unwrap();
        assert_eq!(patron["usage"]["five_hour"]["used_percent"], 57.0);
        assert!(list.as_array().unwrap().iter().all(|a| a["status"] != "review_required"));
        assert_eq!(std::fs::read_to_string(file.with_extension("toml.kraal-bak")).unwrap(), config);
        for (home, credential) in
            [(&standard, "standard"), (&env.root.join("patron.bzh"), "patron.bzh"), (&env.root.join("gmail"), "gmail")]
        {
            assert_eq!(std::fs::read_to_string(home.join("credential")).unwrap(), credential);
        }
        assert!(!env.root.join("home/logins").exists());
        let socket = env.root.join(format!("run/kraal/{}.sock", env.session));
        let out = env
            .cmd(&["agent", "claude", "--account", "default", "--tab"])
            .env("PATH", &path)
            .env("KRAAL_SOCKET", socket)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let new_pane = String::from_utf8(out.stdout).unwrap().trim().to_string();
        env.wait_for_text(&new_pane, &format!("home={}", env.root.join("patron.bzh").display()));
        client.send(b"\x02a");
        client.wait_for_row("Start an agent");
        assert!(!client.rows().iter().any(|r| r.contains("Existing Claude account")));
    }
}

#[test]
fn a_missing_agent_keeps_the_add_form_and_creates_nothing() {
    let env = Env::new("login-missing");
    env.workspace("w", "layout { pane name=\"shell\" }\n");
    env.run(&["open", "w"]);
    let mut client = RawClient::attach_sized(&env, Some("w"), (80, 24)).traced();
    client.send(b"\x02An");
    client.wait_for_row("Not installed");
    client.send(b"\x15unfinished\r");
    client.wait_for_row("Install Claude");
    client.wait_for_row("Name    unfinished");
    assert!(login_panes(&env).is_empty());
    let file = env.root.join("config/kraal/accounts.toml");
    assert!(!std::fs::read_to_string(file).unwrap_or_default().contains("unfinished"));
    client.close_overlay("Name    unfinished");
    client.wait_for_row("+ Add account");
    client.close_overlay("Enter Add account");
    client.send(b"\x02An\x1b[Z\x1b[C");
    client.wait_for_row("Agent   Codex");
    client.close_overlay("Agent   Codex");
    client.close_overlay("Enter Add account");
    client.send(b"\x02An");
    client.wait_for_row("Agent   Codex");
    client.close_overlay("Agent   Codex");
    client.close_overlay("Enter Add account");
    client.send(b"\x02a");
    let rows = client.wait_for_row("No accounts ready.");
    assert!(!rows.iter().any(|r| r.contains("Add account")));
}

#[test]
fn command_line_login_verifies_and_reports_the_same_statuses() {
    let env = Env::new("login-cli");
    let (home, path) = login_agent(&env, "claude");
    std::fs::write(home.join("finish-login"), "0").unwrap();
    let file = env.root.join("config/kraal/accounts.toml");
    for (code, status, label, logged_in) in [
        ("1", "not_signed_in", "sign-in required", serde_json::json!(false)),
        ("2", "could_not_check", "could not check sign-in", serde_json::Value::Null),
        ("0", "signed_in", "is signed in", serde_json::json!(true)),
    ] {
        std::fs::write(home.join("status"), code).unwrap();
        let out = env.cmd(&["accounts", "login", "claude:work"]).env("PATH", &path).output().unwrap();
        assert_eq!(out.status.success(), code == "0");
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(text.contains(label), "{text}");
        assert_eq!(std::fs::read_to_string(&file).unwrap().contains("sign_in_required = true"), code != "0");
        let out = env.cmd(&["accounts", "list", "--json"]).env("PATH", &path).output().unwrap();
        assert!(out.status.success());
        assert!(!String::from_utf8_lossy(&out.stderr).contains("PRIVATE-LOGIN-OUTPUT"));
        let list: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let account = list.as_array().unwrap().iter().find(|a| a["name"] == "work").unwrap();
        assert_eq!(account["status"], status);
        assert_eq!(account["logged_in"], logged_in);
    }
}

/// Shell definitions need no account bindings, so account changes do not need write access
/// to their directory (nor leave lock files beside them).
#[test]
fn account_updates_allow_read_only_shell_definitions() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new("readonly-layouts");
    env.workspace("shells", "layout { pane name=\"shell\" }\n");
    let dir = env.root.join("config/kraal/workspaces");
    let file = dir.join("shells.kdl");
    let original = std::fs::read(&file).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o400)).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let added = env.try_run(&["accounts", "add", "codex", "work", "--no-login"]);
    let limits = env.try_run(&["accounts", "limits", "--threshold", "75"]);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));
    assert!(limits.status.success(), "{}", String::from_utf8_lossy(&limits.stderr));
    assert_eq!(std::fs::read(&file).unwrap(), original);
    assert_eq!(std::fs::read_dir(dir).unwrap().count(), 1, "no lock file was needed");
    let accounts: toml::Value =
        toml::from_str(&std::fs::read_to_string(env.root.join("config/kraal/accounts.toml")).unwrap()).unwrap();
    assert_eq!(accounts["account"][0]["name"].as_str(), Some("work"));
    assert_eq!(accounts["limits"]["threshold"].as_integer(), Some(75));
}

/// An account renamed from the agent picker, then from the command line: its rule and its usage
/// follow, and the agent running on it goes on under the new name.
#[test]
fn a_renamed_account_keeps_its_rules_its_usage_and_its_agents() {
    let env = Env::new("renameacct");
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(
        &bin.join("claude"),
        "#!/bin/sh\n[ \"$2\" = status ] && exit 1\necho \"home=${CLAUDE_CONFIG_DIR:-default}\"\nexec sleep 100\n",
    );
    let home = env.root.join("alpha-home");
    std::fs::create_dir_all(&home).unwrap();
    let file = env.root.join("config/kraal/accounts.toml");
    std::fs::write(
        &file,
        format!(
            "[[account]]\nname = \"alpha\"\nagent = \"claude\"\nhome = {:?}\n\n[[rule]]\nworkspace = \"w\"\naccounts = [\"alpha\"]\n",
            home.display().to_string()
        ),
    )
    .unwrap();
    let usage = env.root.join("state/kraal/usage");
    std::fs::create_dir_all(&usage).unwrap();
    std::fs::write(usage.join("claude-alpha.json"), r#"{"observed_at":0,"source":"statusline"}"#).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let socket = env.root.join("run/kraal/renameacct.sock");
    let out = env
        .cmd(&["agent", "claude", "--account", "alpha", "--tab"])
        .env("PATH", &path)
        .env("KRAAL_SOCKET", &socket)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let agent = panes.as_array().unwrap().iter().find(|p| p["command"][0] == "claude").unwrap()["id"].to_string();
    env.wait_for_text(&agent, &format!("home={}", home.display()));
    let (mut events, seen) = env.follow(&["events", "--since", "0", "agent.*"]);
    let hook = |payload: &str| {
        let mut c = env.cmd(&["hook", "claude"]);
        c.env("KRAAL_PANE", &agent).env("KRAAL_SOCKET", &socket).stdin(std::process::Stdio::piped());
        let mut child = c.spawn().unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
        assert!(child.wait().unwrap().success());
    };
    // The account the next event of type `ty` names.
    let account_said = |ty: &str| -> String {
        loop {
            let line = seen.recv_timeout(Duration::from_secs(10)).expect("an agent event");
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            if v["type"] == ty {
                return v["account"].as_str().unwrap().to_string();
            }
        }
    };
    hook(r#"{"hook_event_name":"UserPromptSubmit","session_id":"s-1"}"#);
    assert_eq!(account_said("agent.working"), "claude:alpha");
    let mut client = RawClient::attach(&env, "w").traced();

    // Tab on the account in Accounts, then Rename and the new name.
    // The client is sent what changed since the frame before: a word is looked for only where
    // it is drawn over blank cells or other letters.
    client.send(b"\x02Al");
    client.wait_for_row("Accounts");
    client.send(b"alpha");
    client.wait_for_rows("alpha selected beside Add account", |rows| {
        rows.iter().any(|r| r.contains("Search  alpha"))
            && rows.iter().any(|r| r.contains("▸ Claude · alpha"))
            && rows.iter().any(|r| r.contains("+ Add account"))
            && rows.iter().any(|r| r.contains("Enter Details") && r.contains("1/1"))
    });
    client.send(b"\t");
    client.wait_for_row("Rename");
    client.send(b"\r\x15gamma\r");
    let named = |name: &str| std::fs::read_to_string(&file).unwrap().contains(&format!("name = \"{name}\""));
    wait_until(Duration::from_secs(10), "renamed from the picker", || named("gamma"));
    let text = std::fs::read_to_string(&file).unwrap();
    let parsed: toml::Value = toml::from_str(&text).unwrap();
    let id = parsed["account"][0]["id"].as_str().unwrap();
    assert!(
        text.contains(&format!("account_ids = [\"claude:@{id}\"]"))
            && text.contains("accounts = [\"gamma\"]")
            && !text.contains("alpha\""),
        "{text}"
    );
    assert!(usage.join("by-id").join(format!("claude-{id}.json")).exists());
    hook(r#"{"hook_event_name":"Stop","session_id":"s-1"}"#);
    assert_eq!(account_said("agent.done"), "claude:gamma");

    // From the command line: the session reads accounts.toml again when it changes.
    assert_eq!(env.run(&["accounts", "rename", "claude:gamma", "zulu"]).trim(), "renamed claude·gamma to claude·zulu");
    client.wait_for_row("zulu");
    hook(r#"{"hook_event_name":"UserPromptSubmit","session_id":"s-1"}"#);
    assert_eq!(account_said("agent.working"), "claude:zulu");
    assert!(std::fs::read_to_string(&file).unwrap().contains(&format!("account_ids = [\"claude:@{id}\"]")));
    assert!(usage.join("by-id").join(format!("claude-{id}.json")).exists());
    let out = env.try_run(&["accounts", "rename", "claude:default", "mine"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("check the existing sign-in") && !out.status.success());
    let screen = env.run(&["read", "-p", &agent]);
    assert!(screen.contains(&format!("home={}", home.display())), "the agent went on: {screen}");
    let _ = events.kill();
    let _ = events.wait();
}

#[test]
fn reused_account_names_leave_running_and_saved_references_on_the_original_account() {
    let env = Env::new("account-identity");
    let pane = account_switch_agent(&env, "claude", "notify");
    let path = format!("{}:{}", env.root.join("bin").display(), std::env::var("PATH").unwrap_or_default());
    let panes = || -> serde_json::Value { serde_json::from_str(&env.run(&["panes", "--json"])).unwrap() };
    let old =
        panes().as_array().unwrap().iter().find(|p| p["id"].as_u64() == pane.parse::<u64>().ok()).unwrap().clone();
    let id = old["account_id"].as_str().unwrap();
    env.workspace("closed", "// Kept closed.\nlayout { pane agent=\"claude:alpha\" }\n");
    env.run(&["save", "saved", "--workspace", "w"]);
    env.run(&["accounts", "rename", "claude:alpha", "work"]);
    wait_until(PATIENCE, "pane follows its renamed label", || {
        panes().as_array().unwrap().iter().any(|p| p["account"] == "claude:work" && p["account_id"] == id)
    });
    let definition = std::fs::read_to_string(env.root.join("config/kraal/workspaces/closed.kdl")).unwrap();
    assert!(definition.contains("// Kept closed.") && definition.contains(&format!("account_id={id:?}")));
    env.run(&["kill"]);

    // A session of an older release carries the label and launch environment, but no identity.
    let saved = env.root.join(format!("state/kraal/sessions-v2/{}.json", env.session));
    let mut state: serde_json::Value = serde_json::from_slice(&std::fs::read(&saved).unwrap()).unwrap();
    fn old_specs(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(object) => {
                if let Some(spec) = object.get_mut("spec").and_then(serde_json::Value::as_object_mut) {
                    spec.remove("account_id");
                    if let Some(env) = spec.get_mut("env").and_then(serde_json::Value::as_array_mut) {
                        env.retain(|pair| pair[0] != "KRAAL_ACCOUNT_ID");
                    }
                }
                object.values_mut().for_each(old_specs);
            }
            serde_json::Value::Array(values) => values.iter_mut().for_each(old_specs),
            _ => {}
        }
    }
    old_specs(&mut state);
    state["version"] = serde_json::json!(1);
    state["revision"] = serde_json::json!(1);
    let legacy = env.legacy_state_file();
    let source = serde_json::to_vec(&state).unwrap();
    std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
    std::fs::write(&legacy, &source).unwrap();
    std::fs::remove_file(&saved).unwrap();
    env.run(&["accounts", "rm", "claude:work"]);
    assert_eq!(std::fs::read(&legacy).unwrap(), source);
    let migrated = std::fs::read_to_string(&saved).unwrap();
    assert!(migrated.contains(id), "closed legacy sessions are bound before removal");
    let replacement = env.root.join("replacement").display().to_string();
    env.run(&["accounts", "add", "claude", "alpha", "--home", &replacement, "--no-login"]);
    let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(panes().as_array().unwrap().iter().any(|p| p["account"] == "claude:work" && p["account_id"] == id));
    for name in ["closed", "saved"] {
        let out = env.try_run(&["open", name]);
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && error.contains("account was removed") && !error.contains(id), "{error}");
    }
    let refused = raw_request(
        &env,
        serde_json::json!({ "cmd": "open_agent", "agent": "claude",
        "account": "alpha", "account_id": id, "placement": "tab", "near": null, "cwd": null }),
    );
    assert!(refused["Err"].as_str().unwrap().contains("account was removed"));
    // A 0.2 or 0.3 client still selects the current account by name.
    let opened = raw_request(
        &env,
        serde_json::json!({ "cmd": "open_agent", "agent": "claude",
        "account": "alpha", "placement": "tab", "near": null, "cwd": null }),
    );
    assert!(opened.get("Ok").is_some(), "{opened}");
    assert!(panes().as_array().unwrap().iter().any(|p| p["account"] == "claude:alpha" && p["account_id"] != id));
    let selected = raw_request(
        &env,
        serde_json::json!({ "cmd": "open_agent", "agent": "claude",
        "account": "alpha", "account_id": "test-claude-beta", "placement": "tab", "near": null, "cwd": null }),
    );
    assert!(selected.get("Ok").is_some(), "{selected}");
    assert!(
        panes()
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["account"] == "claude:beta" && p["account_id"] == "test-claude-beta")
    );
    let live = panes().as_array().unwrap().iter().find(|p| p["account"] == "claude:alpha").unwrap().clone();
    let live_id = live["account_id"].as_str().unwrap();
    env.run(&["accounts", "rm", "claude:alpha"]);
    env.run(&["accounts", "add", "claude", "alpha", "--home", &replacement, "--no-login"]);
    let file: toml::Value =
        toml::from_str(&std::fs::read_to_string(env.root.join("config/kraal/accounts.toml")).unwrap()).unwrap();
    let replacement_id =
        file["account"].as_array().unwrap().iter().find(|a| a["name"].as_str() == Some("alpha")).unwrap()["id"]
            .as_str()
            .unwrap();
    assert_ne!(live_id, replacement_id);
    env.run(&["config", "reload"]);
    assert!(panes().as_array().unwrap().iter().any(|p| p["id"] == live["id"] && p["account_id"] == live_id));
    let mut hook = env
        .cmd(&["statusline"])
        .env("KRAAL_ACCOUNT_ID", live_id)
        .env("CLAUDE_CONFIG_DIR", &replacement)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    use std::io::Write;
    hook.stdin.take().unwrap().write_all(br#"{"rate_limits":{"five_hour":{"used_percentage":91}}}"#).unwrap();
    assert!(hook.wait().unwrap().success());
    assert!(
        !env.root.join(format!("state/kraal/usage/by-id/claude-{replacement_id}.json")).exists(),
        "a late statusline must not write usage to the replacement, even in the same home"
    );
}

/// While the keys after the prefix are awaited, the client's terminal is asked for every key
/// with its place on the keyboard (kitty keyboard protocol), and set back after: the AZERTY key
/// that types `&`, at the place of `1`, goes to tab 1. A terminal that cannot send the typed text
/// along is not asked again.
#[test]
fn the_leader_menu_reads_keys_by_their_place() {
    let env = Env::new("byplace");
    env.workspace(
        "w",
        "layout {\n    tab name=\"a\" {\n        pane name=\"a\"\n    }\n    tab name=\"b\" {\n        pane name=\"b\"\n    }\n}\n",
    );
    env.run(&["open", "w"]);
    env.run(&["focus", "b"]);
    let focused = || -> String {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let shown = panes.as_array().unwrap().iter().find(|p| p["focused"] == true).cloned();
        shown.and_then(|p| p["name"].as_str().map(String::from)).unwrap_or_default()
    };
    let count =
        |client: &RawClient, what: &str| String::from_utf8_lossy(&client.screen.lock().unwrap()).matches(what).count();
    let raised = "\x1b[=29;1u";
    let lowered = "\x1b[=5;1u";

    let mut client = RawClient::attach(&env, "w");
    client.send(b"\x02");
    client.wait_for_screen(&format!("{raised}\x1b[?u"));
    client.send(b"\x1b[?29u");
    client.send(b"\x1b[38::49;;38u");
    client.wait_for_screen(lowered);
    assert_eq!(focused(), "a", "the key at the place of 1 went to tab 1");
    // Asked once: the next time, only raised.
    client.send(b"\x02");
    let start = Instant::now();
    while count(&client, raised) < 2 {
        assert!(start.elapsed() < Duration::from_secs(10), "not raised the second time");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(count(&client, "\x1b[?u"), 1);
    client.send(b"\x1b");

    // A terminal that grants every key as a sequence but not its text is set back at once, and
    // not raised again.
    let mut other = RawClient::attach(&env, "w");
    other.send(b"\x02");
    other.wait_for_screen(&format!("{raised}\x1b[?u"));
    other.send(b"\x1b[?13u");
    other.wait_for_screen(lowered);
    other.send(b"\x1b");
    other.send(b"\x02");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(count(&other, raised), 1);
}

/// A program's kitty image reaches a terminal that shows images: sent to it once, then drawn as
/// placeholder cells where the program put it. A terminal that cannot draw them that way gets
/// neither.
#[test]
fn images_reach_a_terminal_that_shows_them() {
    let env = Env::new("images");
    env.workspace("w", "layout {\n    pane name=\"img\"\n}\n");
    env.run(&["open", "w"]);
    env.wait_for_text("img", "$");
    let script = env.root.join("img.sh");
    // A 2×2 RGB image shown over 4 columns and 2 rows.
    write_script(
        &script,
        "#!/bin/sh\nprintf '\\033_Ga=T,f=24,s=2,v=2,c=4,r=2,q=2;AAAAAAAAAAAAAAAA\\033\\\\'\necho shown\n",
    );

    let mut plain = RawClient::attach(&env, "w");
    let mut client = RawClient::attach(&env, "w");
    // The server asks each terminal; this one answers as Ghostty does, the other as a terminal
    // that speaks the protocol without drawing placeholders.
    client.wait_for_screen("a=q");
    client.send(b"\x1b_Gi=1263684657;OK\x1b\\\x1bP>|ghostty 1.1.3\x1b\\\x1b[6;20;10t");
    plain.wait_for_screen("a=q");
    plain.send(b"\x1b_Gi=1263684657;OK\x1b\\\x1bP>|WezTerm 20240203\x1b\\");
    std::thread::sleep(Duration::from_millis(300));
    env.run(&["send", "-p", "img", &format!("{}\r", script.display())]);
    env.wait_for_text("img", "shown");
    client.wait_for_screen("\x1b_Ga=t,f=100,i=");
    client.wait_for_screen("\x1b_Ga=p,U=1,i=");
    client.wait_for_screen("\u{10EEEE}");
    plain.send(b"\x1b[I");
    std::thread::sleep(Duration::from_millis(300));
    let seen = String::from_utf8_lossy(&plain.screen.lock().unwrap()).into_owned();
    assert!(seen.contains("shown"), "the other client draws the pane");
    assert!(!seen.contains("\x1b_Ga=t") && !seen.contains('\u{10EEEE}'), "no image for a terminal that cannot show it");
}

#[test]
fn images_reach_a_terminal_that_draws_sixel() {
    let env = Env::new("sixel");
    env.workspace("w", "layout {\n    pane name=\"img\"\n}\n");
    env.run(&["open", "w"]);
    env.wait_for_text("img", "$");
    let script = env.root.join("img.sh");
    write_script(
        &script,
        "#!/bin/sh\nprintf '\\033_Ga=T,f=24,s=2,v=2,c=4,r=2,q=2;AAAAAAAAAAAAAAAA\\033\\\\'\necho shown\n",
    );
    let mut client = RawClient::attach(&env, "w");
    // It answers as foot does: no kitty graphics, sixel among its device attributes.
    client.wait_for_screen("\x1b[c");
    client.send(b"\x1bP>|foot(1.21.0)\x1b\\\x1b[6;20;10t\x1b[?62;4;22c");
    std::thread::sleep(Duration::from_millis(300));
    env.run(&["send", "-p", "img", &format!("{}\r", script.display())]);
    env.wait_for_text("img", "shown");
    // 4×2 cells of 10×20 pixels, as many rows of them as whole sixel bands hold.
    client.wait_for_screen("\x1bP0;1;0q\"1;1;40;36#0;2;0;0;0#0");
    let seen = |c: &RawClient| String::from_utf8_lossy(&c.screen.lock().unwrap()).into_owned();
    assert!(!seen(&client).contains('\u{10EEEE}') && !seen(&client).contains("\x1b_Ga=t"), "no placeholders");
    // Text changing next to it does not draw it again.
    let drawn = seen(&client).matches("\x1bP0;1;0q").count();
    env.run(&["send", "-p", "img", "echo dk$((1+1))\r"]);
    env.wait_for_text("img", "dk2");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(seen(&client).matches("\x1bP0;1;0q").count(), drawn);
    // The font changed with the window: the image waits for the new cell size, then takes it.
    let asked = seen(&client).matches("\x1b[16t").count();
    client.resize((120, 40));
    let start = Instant::now();
    while seen(&client).matches("\x1b[16t").count() == asked {
        assert!(start.elapsed() < Duration::from_secs(10), "the cell size is asked again");
        std::thread::sleep(Duration::from_millis(50));
    }
    client.send(b"\x1b[6;18;9t");
    client.wait_for_screen("\x1bP0;1;0q\"1;1;36;36");
    // The screen cleared: the image's cells are erased.
    assert!(!seen(&client).contains("\x1b[4X"));
    env.run(&["send", "-p", "img", "printf '\\033[H\\033[2J'\r"]);
    client.wait_for_screen("\x1b[4X");
}

/// A large sixel image is painted off the server's loop: its cells are taken at once, where the
/// program put it, the text after it goes below it, and the terminal gets it once painted.
#[test]
fn a_large_sixel_image_reaches_the_terminal_with_the_text_after_it_below() {
    let env = Env::new("bigsixel");
    env.workspace("w", "layout {\n    pane name=\"img\"\n}\n");
    env.run(&["open", "w"]);
    env.wait_for_text("img", "$");
    // 400×240 pixels: 40 columns and 12 rows of 10×20 cells.
    let image = env.root.join("big.six");
    let mut sixel = b"\x1bPq\"1;1;400;240#1".to_vec();
    for _ in 0..40 {
        sixel.extend_from_slice(b"!400~-");
    }
    sixel.extend_from_slice(b"\x1b\\");
    std::fs::write(&image, sixel).unwrap();
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_screen("a=q");
    client.send(b"\x1b_Gi=1263684657;OK\x1b\\\x1bP>|ghostty 1.1.3\x1b\\\x1b[6;20;10t");
    std::thread::sleep(Duration::from_millis(300));
    env.run(&["send", "-p", "img", &format!("printf '\\033[H\\033[2J'; cat {}; echo below\r", image.display())]);
    let screen = env.wait_for_text("img", "below");
    assert_eq!(screen.lines().position(|l| l.starts_with("below")), Some(12), "{screen}");
    client.wait_for_screen("\x1b_Ga=t,f=100,i=");
    client.wait_for_screen("\x1b_Ga=p,U=1,i=");
}

#[test]
fn a_terminal_that_tells_its_cell_size_last_draws_sixel_too() {
    let env = Env::new("sixellate");
    env.workspace("w", "layout {\n    pane name=\"img\"\n}\n");
    env.run(&["open", "w"]);
    env.wait_for_text("img", "$");
    let script = env.root.join("img.sh");
    write_script(
        &script,
        "#!/bin/sh\nprintf '\\033_Ga=T,f=24,s=2,v=2,c=4,r=2,q=2;AAAAAAAAAAAAAAAA\\033\\\\'\necho shown\n",
    );
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_screen("\x1b[c");
    // Its device attributes before its cell size, though asked after it.
    client.send(b"\x1bP>|foot(1.21.0)\x1b\\\x1b[?62;4;22c\x1b[6;20;10t");
    std::thread::sleep(Duration::from_millis(300));
    env.run(&["send", "-p", "img", &format!("{}\r", script.display())]);
    env.wait_for_text("img", "shown");
    client.wait_for_screen("\x1bP0;1;0q");
}

#[test]
fn an_image_scrolling_with_the_output_is_not_sent_at_each_step() {
    let env = Env::new("sixelscroll");
    env.workspace("w", "layout {\n    pane name=\"img\"\n}\n");
    env.run(&["open", "w"]);
    env.wait_for_text("img", "$");
    let script = env.root.join("img.sh");
    // The pane is full first: each line after the image scrolls it up a row, 31 times, a few
    // milliseconds apart. The last word ends no line: the prompt follows it on the same row, and
    // nothing moves after.
    write_script(
        &script,
        "#!/bin/sh\nseq 60\nprintf '\\033_Ga=T,f=24,s=2,v=2,c=4,r=2,q=2;AAAAAAAAAAAAAAAA\\033\\\\'\necho\n\
         i=0\nwhile [ $i -lt 30 ]; do echo line $i; sleep 0.005; i=$((i+1)); done\nprintf stopped\n",
    );
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_screen("\x1b[c");
    client.send(b"\x1bP>|foot(1.21.0)\x1b\\\x1b[6;20;10t\x1b[?62;4;22c");
    std::thread::sleep(Duration::from_millis(300));
    env.run(&["send", "-p", "img", &format!("{}\r", script.display())]);
    // The frame that shows the last word has the image where it stopped: what erased it
    // before is in it or earlier. It is drawn there once it has stayed put, however long the
    // machine takes.
    client.wait_for_screen("stopped");
    let start = Instant::now();
    let seen = loop {
        let seen = String::from_utf8_lossy(&client.screen.lock().unwrap()).into_owned();
        let last = seen.rfind("\x1bP0;1;0q");
        if last.is_some() && last > seen.rfind("\x1b[4X") {
            break seen;
        }
        let drawn = seen.matches("\x1bP0;1;0q").count();
        assert!(start.elapsed() < Duration::from_secs(10), "not drawn where it stopped ({drawn} sixel images)");
        std::thread::sleep(Duration::from_millis(50));
    };
    // Drawn again at each step, it would be some thirty times. A slow machine may pause long
    // enough between steps for the image to be drawn where it waits, now and then.
    let drawn = seen.matches("\x1bP0;1;0q").count();
    assert!(drawn <= 10, "{drawn} sixel images for 31 steps");
}

#[test]
fn a_preview_shows_its_image_as_each_terminal_can() {
    let env = Env::new("pimg");
    let png = env.root.join("dot.png");
    image::RgbaImage::from_pixel(2, 2, image::Rgba([200, 10, 10, 255])).save(&png).unwrap();
    env.workspace("w", &format!("layout {{\n    pane name=\"pic\" preview=\"{}\"\n}}\n", png.display()));
    env.run(&["open", "w"]);
    let mut ghostty = RawClient::attach(&env, "w");
    let mut foot = RawClient::attach(&env, "w");
    let plain = RawClient::attach(&env, "w");
    ghostty.wait_for_screen("\x1b[c");
    ghostty.send(b"\x1b_Gi=1263684657;OK\x1b\\\x1bP>|ghostty 1.1.3\x1b\\\x1b[6;20;10t\x1b[?62;22c");
    foot.wait_for_screen("\x1b[c");
    foot.send(b"\x1bP>|foot(1.21.0)\x1b\\\x1b[6;20;10t\x1b[?62;4;22c");
    ghostty.wait_for_screen("\x1b_Ga=t,f=100,i=");
    ghostty.wait_for_screen("\x1b_Ga=p,U=1,i=");
    ghostty.wait_for_screen("\u{10EEEE}");
    foot.wait_for_screen("\x1bP0;1;0q\"1;1;");
    // A terminal that shows no images gets half blocks.
    plain.wait_for_screen("▀");
    let seen = String::from_utf8_lossy(&plain.screen.lock().unwrap()).into_owned();
    assert!(!seen.contains("\x1b_Ga=t") && !seen.contains("\x1bP0;1;0q") && !seen.contains('\u{10EEEE}'));
}

/// A preview follows its file, also when nothing else happens in the session.
#[test]
fn a_preview_follows_its_file_in_a_quiet_session() {
    let env = Env::new("pfollow");
    let notes = env.root.join("notes.txt");
    std::fs::write(&notes, "first-version\n").unwrap();
    env.workspace("w", &format!("layout {{\n    pane name=\"n\" preview=\"{}\"\n}}\n", notes.display()));
    env.run(&["open", "w"]);
    let client = RawClient::attach(&env, "w");
    client.wait_for_screen("first-version");
    // Long enough for the server to be idle.
    std::thread::sleep(Duration::from_secs(4));
    std::fs::write(&notes, "second-version\n").unwrap();
    let start = Instant::now();
    client.wait_for_screen("second-version");
    assert!(start.elapsed() < Duration::from_secs(3), "shown after {:?}", start.elapsed());
}

#[test]
fn agents_follow_prints_the_list_each_time_it_changes() {
    let env = Env::new("follow");
    // Before the session runs it has no agents, and the list waits for it.
    let (mut follow, lines) = env.follow(&["agents", "--follow"]);
    let next = || -> serde_json::Value {
        let line = lines.recv_timeout(Duration::from_secs(10)).expect("a list in time");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line}"))
    };
    assert_eq!(next(), serde_json::json!([]));
    env.workspace("w", "layout {\n    pane name=\"api\"\n}\n");
    env.run(&["open", "w"]);
    claude_hook(&env, "api", r#"{"hook_event_name":"SessionStart","session_id":"s-1"}"#);
    claude_hook(&env, "api", r#"{"hook_event_name":"UserPromptSubmit","session_id":"s-1"}"#);
    let mut list = next();
    while list[0]["state"] != "working" {
        list = next();
    }
    assert_eq!((list[0]["name"].as_str(), list[0]["agent"].as_str()), (Some("api"), Some("claude")), "{list}");
    claude_hook(&env, "api", r#"{"hook_event_name":"Notification","message":"Claude needs your permission"}"#);
    while list[0]["state"] != "blocked" {
        list = next();
    }
    // The session ends: no agents again.
    env.run(&["kill"]);
    while list != serde_json::json!([]) {
        list = next();
    }
    let _ = follow.kill();
    let _ = follow.wait();
    // It ends once what reads it is gone, even when it has nothing new to say.
    let mut alone = env.cmd(&["agents", "--follow"]).stdout(std::process::Stdio::piped()).spawn().unwrap();
    let mut first = [0u8; 3];
    std::io::Read::read_exact(alone.stdout.as_mut().unwrap(), &mut first).unwrap();
    assert_eq!(&first, b"[]\n");
    drop(alone.stdout.take());
    let start = Instant::now();
    while alone.try_wait().unwrap().is_none() {
        assert!(start.elapsed() < Duration::from_secs(10), "still running with nobody to read it");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A fake ssh, first in the PATH returned: it runs the command "on the server", that is here, as
/// the kraal of `remote` (with its own directories); "down.lan" does not answer. Every call is
/// logged in the file returned.
#[cfg(unix)]
fn fake_ssh(local: &Env, remote: &Env) -> (PathBuf, String) {
    let bin = local.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_kraal"), bin.join("kraal")).unwrap();
    let log = local.root.join("ssh.log");
    write_script(
        &bin.join("ssh"),
        &format!(
            "#!/bin/sh\necho \"$*\" >> {log}\nfor a; do cmd=$a; done\n\
             case \"$*\" in *down.lan*) echo 'ssh: connect to host down.lan port 22: Connection refused' >&2; exit 255;; esac\n\
             unset KRAAL KRAAL_SOCKET KRAAL_SESSION KRAAL_PANE\n\
             export HOME={r}/home XDG_RUNTIME_DIR={r}/run XDG_STATE_HOME={r}/state XDG_CONFIG_HOME={r}/config\n\
             exec sh -c \"$cmd\"\n",
            log = log.display(),
            r = remote.root.display()
        ),
    );
    (log, format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default()))
}

#[cfg(unix)]
#[test]
fn host_attach_delivers_copies_to_one_clients_terminal() {
    use base64::Engine;

    let local = Env::new("copylocal");
    let remote = Env::new("copyremote");
    for env in [&local, &remote] {
        let config = env.root.join("config/kraal/config.toml");
        let text = std::fs::read_to_string(&config).unwrap();
        std::fs::write(config, format!("{text}\n[clipboard]\nmode = 'terminal'\n")).unwrap();
    }
    // A noninteractive shell and no tty echo: only program output can satisfy the markers.
    remote.workspace(
        "w",
        "layout {\n    pane name=\"main\" command=\"sh\" { args \"-c\" \"stty -echo; echo SELECT-ME-PLEASE; cat | sh\"; }\n}\n",
    );
    remote.run(&["open", "w"]);
    remote.wait_for_text("main", "SELECT-ME-PLEASE");
    let (log, path) = fake_ssh(&local, &remote);
    let mut direct = TerminalClient::start(remote.cmd(&["attach", "w"]));
    let mut cmd = local.isolated(Path::new(env!("CARGO_BIN_EXE_kraal")));
    cmd.args(["-s", &remote.session, "attach", "--host", "copy.invalid", "w"]);
    cmd.env("PATH", &path);
    let mut host = TerminalClient::start(cmd);
    direct.wait_for("SELECT-ME-PLEASE");
    host.wait_for("SELECT-ME-PLEASE");
    assert!(std::fs::read_to_string(log).unwrap().contains("proxy -s copyremote"));

    let osc = |text: &str| format!("\x1b]52;c;{}\x07", base64::engine::general_purpose::STANDARD.encode(text));
    let copy = |client: &mut TerminalClient, text: &str| {
        std::thread::sleep(Duration::from_millis(150));
        let encoded = base64::engine::general_purpose::STANDARD.encode(text);
        client.type_keys(format!("printf '\\033]52;c;{encoded}\\007{text}\\n'\r").as_bytes());
    };
    // Each client's input precedes the copy its program emits. The other client's drawn
    // marker is a barrier after that copy, so absence there is checked after delivery.
    copy(&mut host, "FROM-HOST");
    host.wait_for(&osc("FROM-HOST"));
    direct.wait_for("FROM-HOST");
    copy(&mut direct, "FROM-LOCAL");
    direct.wait_for(&osc("FROM-LOCAL"));
    host.wait_for("FROM-LOCAL");

    // Coordinates come from the rendered cells, not a particular sidebar width or frame.
    let rows = text_after(&host.written.lock().unwrap(), 100, 30);
    let (y, row) = rows.iter().enumerate().find(|(_, row)| row.contains("SELECT-ME-PLEASE")).unwrap();
    let x = row.split("SELECT-ME-PLEASE").next().unwrap().chars().count() + 1;
    let end = x + "SELECT-ME-PLEASE".len() - 1;
    let y = y + 1;
    std::thread::sleep(Duration::from_millis(150));
    host.type_keys(format!("\x1b[<0;{x};{y}M\x1b[<32;{end};{y}M\x1b[<0;{end};{y}m").as_bytes());
    host.wait_for(&osc("SELECT-ME-PLEASE"));
    host.wait_for("copy sent to terminal via OSC 52");
    assert_eq!(remote.run(&["buffer", "show"]), "SELECT-ME-PLEASE");
    // A later frame on both clients also bounds the check for an unwanted broadcast.
    copy(&mut direct, "LOCAL-AGAIN");
    direct.wait_for(&osc("LOCAL-AGAIN"));
    host.wait_for("LOCAL-AGAIN");
    let count = |client: &TerminalClient, sequence: &str| {
        client.written.lock().unwrap().windows(sequence.len()).filter(|w| *w == sequence.as_bytes()).count()
    };
    for (text, direct_count, host_count) in
        [("FROM-HOST", 0, 1), ("FROM-LOCAL", 1, 0), ("SELECT-ME-PLEASE", 0, 1), ("LOCAL-AGAIN", 1, 0)]
    {
        assert_eq!(count(&direct, &osc(text)), direct_count, "local client: {text}");
        assert_eq!(count(&host, &osc(text)), host_count, "host client: {text}");
    }
    assert_eq!(count(&direct, "\x1b]52;"), 2);
    assert_eq!(count(&host, "\x1b]52;"), 2);
    direct.type_keys(b"\x02d");
    host.type_keys(b"\x02d");
    assert_eq!(direct.left("local detach").0, 0);
    assert_eq!(host.left("host detach").0, 0);
}

#[cfg(unix)]
#[test]
fn host_clipboard_policy_is_local_and_native_copy_never_also_sends_osc52() {
    use base64::Engine;

    for (case, allowed, mode, limit, fails, intermediate) in [
        ("native", false, "auto", 1048576, false, false),
        ("allowed", true, "auto", 1048576, false, false),
        ("off", true, "off", 1048576, false, false),
        ("limit", true, "auto", 4, false, false),
        ("failure", true, "auto", 1048576, true, false),
        ("intermediate", true, "auto", 1048576, false, true),
        ("vscode", true, "auto", 1048576, false, true),
        ("broken", true, "off", 1048576, false, false),
        #[cfg(target_os = "linux")]
        ("x11", true, "auto", 1048576, false, false),
        #[cfg(target_os = "linux")]
        ("xsel", true, "auto", 1048576, false, false),
    ] {
        let local = Env::new(&format!("clip-{case}"));
        let remote = Env::new(&format!("clip-server-{case}"));
        remote.workspace("w", "layout {\n    pane name=\"main\" command=\"sh\" { args \"-c\" \"stty -echo; echo READY; cat | sh\"; }\n}\n");
        remote.run(&["open", "w"]);
        remote.wait_for_text("main", "READY");
        let other = RawClient::attach(&remote, "w");
        let (_, path) = fake_ssh(&local, &remote);
        let bin = local.root.join("bin");
        let native = local.root.join("clipboard");
        let calls = local.root.join("calls");
        for tool in ["pbcopy", "wl-copy", "xclip", "xsel"] {
            write_script(
                &bin.join(tool),
                &format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {calls}\ncat > {native}\nexit {code}\n",
                    calls = calls.display(),
                    native = native.display(),
                    code = if fails || (case == "xsel" && tool == "xclip") { 1 } else { 0 }
                ),
            );
        }
        let config = local.root.join("config/kraal/config.toml");
        let text = std::fs::read_to_string(&config).unwrap();
        let invalid = if case == "broken" { "mouse = [\n" } else { "" };
        std::fs::write(config, format!("{text}\n[clipboard]\nmode = '{mode}'\nmax_bytes = {limit}\n{invalid}"))
            .unwrap();
        std::fs::write(
            local.root.join("config/kraal/servers.toml"),
            format!("[[server]]\nname = 'box'\nhost = 'copy.invalid'\nclipboard_native = {allowed}\n"),
        )
        .unwrap();
        let mut cmd = local.isolated(Path::new(env!("CARGO_BIN_EXE_kraal")));
        cmd.args(["-s", &remote.session, "attach", "--host", "box", "w"]);
        cmd.env("PATH", path).env("WAYLAND_DISPLAY", "fake-wayland").env("DISPLAY", ":42");
        if matches!(case, "x11" | "xsel") {
            cmd.env_remove("WAYLAND_DISPLAY");
        }
        for name in [
            "SSH_CONNECTION",
            "SSH_CLIENT",
            "SSH_TTY",
            "VSCODE_IPC_HOOK_CLI",
            "VSCODE_AGENT_FOLDER",
            "WSL_DISTRO_NAME",
            "WSL_INTEROP",
        ] {
            cmd.env_remove(name);
        }
        if intermediate {
            cmd.env(if case == "vscode" { "VSCODE_IPC_HOOK_CLI" } else { "SSH_CONNECTION" }, "intermediate");
        }
        let mut host = TerminalClient::start(cmd);
        host.wait_for("READY");
        let encoded = base64::engine::general_purpose::STANDARD.encode("PROGRAM-COPY");
        host.type_keys(format!("printf '\\033]52;c;{encoded}\\007PROGRAM-DONE\\n'\r").as_bytes());
        host.wait_for("PROGRAM-DONE");
        other.wait_for_screen("PROGRAM-DONE");
        let program_native = allowed && mode == "auto" && limit > 4 && !intermediate && !fails;
        let program_terminal = mode != "off" && limit > 4 && !program_native;
        if program_native {
            common::wait_until(Duration::from_secs(10), "native program copy", || {
                std::fs::read_to_string(&native).ok().as_deref() == Some("PROGRAM-COPY")
            });
        } else if program_terminal {
            host.wait_for(&format!("\x1b]52;c;{encoded}\x07"));
        } else if limit == 4 {
            host.wait_for("copy too large");
        }
        // Recopying a buffer is the user's own gesture even when a program filled it.
        std::thread::sleep(Duration::from_millis(200));
        let before = host.written.lock().unwrap().len();
        host.type_keys(b"\x02=\x19");
        let user_native = mode == "auto" && limit > 4 && !intermediate && !fails;
        let expected = if mode == "off" {
            "external copy disabled"
        } else if limit == 4 {
            "copy too large"
        } else if user_native {
            "copied to laptop clipboard"
        } else {
            "copy sent to terminal via OSC 52"
        };
        if limit == 4 {
            // The same refusal is already drawn for the program copy and need not redraw.
            host.wait_for(expected);
        } else {
            common::wait_until(Duration::from_secs(10), expected, || {
                String::from_utf8_lossy(&host.written.lock().unwrap()[before..]).contains(expected)
            });
        }
        assert_eq!(remote.run(&["buffer", "show"]), "PROGRAM-COPY", "{case}");
        let written = host.written.clone();
        host.type_keys(b"\x02d");
        assert_eq!(host.left("clipboard detach").0, 0);
        let terminal = String::from_utf8_lossy(&written.lock().unwrap()).into_owned();
        assert_eq!(
            terminal.matches("\x1b]52;").count(),
            usize::from(program_terminal) + usize::from(!user_native && mode != "off" && limit > 4),
            "{case}"
        );
        assert!(!String::from_utf8_lossy(&other.screen.lock().unwrap()).contains("\x1b]52;"), "{case}: broadcast");
        if mode == "off" || limit == 4 || intermediate {
            assert!(!calls.exists(), "{case}: native tool must not run");
        } else if !fails {
            assert_eq!(std::fs::read_to_string(native).unwrap(), "PROGRAM-COPY");
            let calls = std::fs::read_to_string(calls).unwrap();
            let attempts = if case == "xsel" { 2 } else { 1 };
            assert_eq!(calls.lines().count(), attempts * (1 + usize::from(program_native)));
            if case == "x11" {
                assert!(calls.lines().all(|args| args == "-selection clipboard -in"));
            } else if case == "xsel" {
                assert_eq!(
                    calls.lines().collect::<Vec<_>>(),
                    [
                        "-selection clipboard -in",
                        "--clipboard --input",
                        "-selection clipboard -in",
                        "--clipboard --input"
                    ]
                );
            } else {
                #[cfg(target_os = "linux")]
                assert!(calls.lines().all(|args| args == "--type text/plain;charset=utf-8"));
            }
        }
    }
}

/// A helper stopped before reading cannot write after its client has detached or been
/// signalled, even though its process group is independent of the client's.
#[cfg(target_os = "linux")]
#[test]
fn client_exit_cancels_a_native_copy_before_its_pending_write() {
    for signal in [None, Some(libc::SIGTERM), Some(libc::SIGHUP), Some(libc::SIGINT), Some(libc::SIGQUIT)] {
        let env = Env::new(&format!("copy-exit-{}", signal.unwrap_or(0)));
        env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
        env.run(&["open", "w"]);
        // More than a pipe can hold: the stopped helper cannot complete the handoff.
        assert!(set_buffer(&env, &[], &"x".repeat(1 << 20)).status.success());
        let bin = env.root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let pidfile = env.root.join("helper-pid");
        let copied = env.root.join("copied");
        write_script(
            &bin.join("wl-copy"),
            &format!("#!/bin/sh\necho $$ > {}\nkill -STOP $$\ncat > {}\n", pidfile.display(), copied.display()),
        );
        let mut cmd = env.cmd(&["attach", "w"]);
        cmd.env("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap()))
            .env("WAYLAND_DISPLAY", "fake-wayland");
        for name in [
            "SSH_CONNECTION",
            "SSH_CLIENT",
            "SSH_TTY",
            "VSCODE_AGENT_FOLDER",
            "VSCODE_IPC_HOOK_CLI",
            "WSL_DISTRO_NAME",
            "WSL_INTEROP",
            "DISPLAY",
        ] {
            cmd.env_remove(name);
        }
        let mut client = TerminalClient::start(cmd);
        client.type_keys(b"\x02=\x19");
        wait_until(PATIENCE, "clipboard helper paused", || {
            std::fs::read_to_string(&pidfile).is_ok_and(|s| !s.trim().is_empty())
        });
        let pid: libc::pid_t = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        // Waiting for the stopped state makes the pause deterministic.
        wait_until(PATIENCE, "clipboard helper stopped", || {
            std::fs::read_to_string(format!("/proc/{pid}/status"))
                .is_ok_and(|s| s.lines().any(|line| line.starts_with("State:") && line.contains("T (stopped)")))
        });
        if let Some(signal) = signal {
            client.signal(signal);
        } else {
            client.type_keys(b"\x02d");
        }
        assert_eq!(client.left("pending clipboard copy").0, signal.map_or(0, |s| (128 + s) as u32));
        // SIGKILL may still be pending until the kernel schedules the helper. It may
        // briefly remain stopped, but it must disappear or become a zombie without writing.
        let running = || {
            std::fs::read_to_string(format!("/proc/{pid}/status"))
                .is_ok_and(|s| !s.lines().any(|line| line.starts_with("State:") && line.contains("Z (zombie)")))
        };
        let start = Instant::now();
        while running() && start.elapsed() < Duration::from_secs(1) {
            std::thread::sleep(Duration::from_millis(5));
        }
        let alive = running();
        if alive {
            // SAFETY: the paused helper this test started still owns its pid and group.
            unsafe {
                libc::kill(-pid, libc::SIGCONT);
            }
            let start = Instant::now();
            while !copied.exists() && running() && start.elapsed() < Duration::from_secs(1) {
                std::thread::sleep(Duration::from_millis(5));
            }
            if running() {
                // SAFETY: this is still the helper whose pending write we resumed.
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
            }
        }
        assert!(!alive, "unfinished clipboard helper survived client exit");
        assert!(!copied.exists(), "clipboard was overwritten after client exit");
    }
}

#[test]
fn panes_keep_the_servers_display_environment_and_native_copy_stays_there() {
    let env = Env::new("copydisplay");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    let out = env.cmd(&["open", "w"]).env("DISPLAY", ":42").env("WAYLAND_DISPLAY", "wayland-test").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let client = RawClient::attach(&env, "w");
    let clipboard = env.root.join("server-clipboard");
    let native = env.root.join("native-copy");
    // Stand in for a native clipboard tool without touching any graphical session.
    write_script(&native, &format!("#!/bin/sh\ncat > {}\n", clipboard.display()));
    // Keep the screen marker first so a narrow split cannot wrap it between terminal sequences.
    let script = format!(
        "printf 'server-only' | {}; printf 'NATIVE-DONE DISPLAY=%s WAYLAND_DISPLAY=%s\\n' \"$DISPLAY\" \"$WAYLAND_DISPLAY\"; exec sleep 60",
        native.display()
    );
    let out = env
        .cmd(&["run", "--name", "copy", "--", "sh", "-c", &script])
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    client.wait_for_screen("NATIVE-DONE");
    env.wait_for_text("copy", "NATIVE-DONE DISPLAY=:42 WAYLAND_DISPLAY=wayland-test");
    assert_eq!(std::fs::read_to_string(clipboard).unwrap(), "server-only");
    assert!(buffer_list(&env).is_empty());
    assert!(!String::from_utf8_lossy(&client.screen.lock().unwrap()).contains("\x1b]52;"));
}

#[cfg(unix)]
#[test]
fn agents_of_a_remote_session_are_listed_and_reached() {
    let local = Env::new("rlocal");
    let remote = Env::new("rremote");
    // The server's kraal: an agent in a tab that is not the one shown.
    remote.workspace("w", "layout {\n    pane name=\"api\"\n}\n");
    remote.run(&["open", "w"]);
    remote.run(&["run", "--tab", "-n", "other", "--", "sh"]);
    claude_hook(&remote, "api", r#"{"hook_event_name":"SessionStart","session_id":"s-1"}"#);
    claude_hook(&remote, "api", r#"{"hook_event_name":"UserPromptSubmit","session_id":"s-1"}"#);
    let remote_api = || -> serde_json::Value {
        let panes: serde_json::Value = serde_json::from_str(&remote.run(&["panes", "--json"])).unwrap();
        panes.as_array().unwrap().iter().find(|p| p["name"] == "api").unwrap().clone()
    };
    let api = remote_api()["id"].to_string();
    assert_eq!(remote_api()["focused"], false);

    let (log, path) = fake_ssh(&local, &remote);
    let inventory = local.root.join("config/kraal/servers.toml");
    std::fs::write(
        &inventory,
        "[[server]]\nname = \"gpu\"\nhost = \"gpu.lan\"\nagents = \"rremote\"\n\n\
         [[server]]\nname = \"down\"\nhost = \"down.lan\"\nagents = \"main\"\n\n\
         [[server]]\nname = \"gpu2\"\nhost = \"gpu2.lan\"\n",
    )
    .unwrap();
    local.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    let out = local.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let local_panes = || -> Vec<serde_json::Value> {
        let panes: serde_json::Value = serde_json::from_str(&local.run(&["panes", "--json"])).unwrap();
        panes.as_array().unwrap().clone()
    };
    let ssh_log = || std::fs::read_to_string(&log).unwrap_or_default();

    // The sidebar lists the agent after its server's name. The server that does not answer is
    // said once, and tried again later, not at once.
    let mut client = RawClient::attach(&local, "w");
    client.wait_for_screen("gpu › ");
    client.wait_for_screen("working · claude · w");
    client.wait_for_screen("the agents of down cannot be listed");
    std::thread::sleep(Duration::from_secs(3));
    let tries = ssh_log().matches("down.lan").count();
    assert!(tries <= 2, "a server that is down is not hammered: {tries} tries in about 3 s");

    // `kraal agents` lists them too, after the server's name, with their id there.
    let listed = local.run(&["agents"]);
    assert!(listed.lines().any(|l| l.contains("gpu › w") && l.contains("api") && l.contains("working")), "{listed}");
    let agents: serde_json::Value = serde_json::from_str(&local.run(&["agents", "--json"])).unwrap();
    let there = agents.as_array().unwrap().iter().find(|a| a["server"] == "gpu").cloned().unwrap();
    assert_eq!((there["session"].as_str(), there["kind"].as_str()), (Some("rremote"), Some("remote")), "{there}");
    assert_eq!((there["id"].to_string(), there["agent"].as_str()), (api.clone(), Some("claude")), "{there}");
    // What another kraal watching this one reads lists this session's own agents only.
    let (mut follow, lines) = local.follow(&["agents", "--follow"]);
    assert_eq!(lines.recv_timeout(Duration::from_secs(10)).expect("a list").trim(), "[]");
    let _ = follow.kill();
    let _ = follow.wait();

    // Blocked there: told and announced here, as a local agent is.
    let (mut events, notified) = local.follow(&["events", "notification", "agent.*"]);
    claude_hook(&remote, "api", r#"{"hook_event_name":"Notification","message":"Claude needs your permission"}"#);
    let mut seen = std::collections::HashMap::new();
    while seen.len() < 2 {
        let line = notified.recv_timeout(Duration::from_secs(10)).expect("an event");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        seen.insert(v["type"].as_str().unwrap().to_string(), v);
    }
    let v = &seen["notification"];
    let said = (v["title"].as_str(), v["body"].as_str());
    assert_eq!(said, (Some("claude · api on gpu"), Some("needs your input")), "{v}");
    let v = &seen["agent.blocked"];
    assert_eq!(
        (v["server"].as_str(), v["session"].as_str(), v["pane"].to_string()),
        (Some("gpu"), Some("rremote"), api.clone())
    );
    assert_eq!(
        (v["name"].as_str(), v["agent"].as_str(), v["previous"].as_str()),
        (Some("api"), Some("claude"), Some("working"))
    );
    let _ = events.kill();
    let _ = events.wait();
    client.wait_for_screen("blocked · claude · w");

    // alt+a: the next agent needing you is there; a tab attaches to that session, its pane shown.
    client.send(b"\x1ba");
    let attach = |p: &serde_json::Value| {
        let cmd: Vec<&str> = p["command"].as_array().unwrap().iter().filter_map(|a| a.as_str()).collect();
        cmd.get(1..).is_some_and(|a| a == ["-s", "rremote", "attach", "--host", "gpu", "--pane", api.as_str()])
    };
    let start = Instant::now();
    while !local_panes().iter().any(attach) {
        assert!(start.elapsed() < Duration::from_secs(10), "no tab attached: {:?}", local_panes());
        std::thread::sleep(Duration::from_millis(50));
    }
    let start = Instant::now();
    while remote_api()["focused"] != true {
        assert!(start.elapsed() < Duration::from_secs(10), "the agent's pane is not shown there: {}", remote_api());
        std::thread::sleep(Duration::from_millis(50));
    }
    // The tab shows another pane there; again: the tab already attached is used, and the
    // client in it asked to show the agent's pane.
    remote.run(&["focus", "other"]);
    let start = Instant::now();
    while remote_api()["focused"] != false {
        assert!(start.elapsed() < Duration::from_secs(10), "{}", remote_api());
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(500));
    client.send(b"\x1ba");
    let start = Instant::now();
    while remote_api()["focused"] != true {
        assert!(start.elapsed() < Duration::from_secs(10), "the agent's pane is not shown again: {}", remote_api());
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!ssh_log().contains(" focus "), "asked through the tab, not over another connection: {}", ssh_log());
    assert_eq!(local_panes().len(), 2, "{:?}", local_panes());

    // Attaching to a server makes kraal list that session's agents from then on.
    let kraal = env!("CARGO_BIN_EXE_kraal");
    local.run(&[
        "run",
        "-n",
        "gpu2",
        "--",
        "env",
        "-u",
        "KRAAL_PANE",
        kraal,
        "-s",
        "rremote",
        "attach",
        "--host",
        "gpu2",
    ]);
    let start = Instant::now();
    while std::fs::read_to_string(&inventory).unwrap().matches("agents = \"rremote\"").count() < 2 {
        assert!(start.elapsed() < Duration::from_secs(10), "{}", std::fs::read_to_string(&inventory).unwrap());
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(std::fs::read_to_string(&inventory).unwrap().contains("agents = \"main\""), "the others stay as written");

    // A new remote connection is offered for explicit addition, never silently saved.
    let before_unknown = std::fs::read_to_string(&inventory).unwrap();
    local.run(&[
        "run",
        "-n",
        "unsaved",
        "--",
        "env",
        "-u",
        "KRAAL_PANE",
        kraal,
        "-s",
        "rremote",
        "attach",
        "--host",
        "unsaved.lan",
    ]);
    local.wait_for_text("unsaved", "api");
    assert_eq!(std::fs::read_to_string(&inventory).unwrap(), before_unknown);

    // A configuration that cannot be read does not say that servers may be added: none is,
    // though the attach works.
    let broken = local.root.join("broken");
    std::fs::create_dir_all(broken.join("kraal")).unwrap();
    std::fs::copy(&inventory, broken.join("kraal/servers.toml")).unwrap();
    std::fs::write(broken.join("kraal/config.toml"), "[servers]\nauto_add = false\nmouse = [\n").unwrap();
    let before = std::fs::read_to_string(broken.join("kraal/servers.toml")).unwrap();
    let config = format!("XDG_CONFIG_HOME={}", broken.display());
    let args = ["run", "-n", "new", "--", "env", "-u", "KRAAL_PANE", &config, kraal, "-s", "rremote", "attach"];
    local.run(&[&args[..], &["--host", "new.lan"]].concat());
    local.wait_for_text("new", "api");
    std::thread::sleep(Duration::from_millis(500));
    assert!(ssh_log().contains("new.lan"));
    assert_eq!(std::fs::read_to_string(broken.join("kraal/servers.toml")).unwrap(), before);
    drop(client);
}

/// A tab attached to a server's session by hand is the one used, and each agent waiting there is
/// shown in it in turn: in the tab, whatever another terminal attached to that session shows.
#[cfg(unix)]
#[test]
fn a_tab_attached_to_a_server_shows_each_agent_there_in_turn() {
    let local = Env::new("tlocal");
    let remote = Env::new("tremote");
    // Two workspaces there, an agent shown in each, both waiting for an answer.
    remote.workspace("one", "layout {\n    pane name=\"api\"\n}\n");
    remote.workspace("two", "layout {\n    pane name=\"web\"\n}\n");
    remote.run(&["open", "one"]);
    remote.run(&["open", "two"]);
    for (pane, n, word) in [("api", 1, "ONE"), ("web", 2, "TWO")] {
        remote.run(&["send", "-p", pane, "--enter", &format!("echo IN-$(({n}))-{word}")]);
        remote.wait_for_text(pane, &format!("IN-{n}-{word}"));
        for event in ["SessionStart", "UserPromptSubmit", "Notification"] {
            let payload = format!(
                r#"{{"hook_event_name":"{event}","session_id":"s-{n}","message":"Claude needs your permission"}}"#
            );
            claude_hook(&remote, pane, &payload);
        }
    }
    let (_log, path) = fake_ssh(&local, &remote);
    std::fs::write(
        local.root.join("config/kraal/servers.toml"),
        "[[server]]\nname = \"gpu\"\nhost = \"gpu.lan\"\nagents = \"tremote\"\n",
    )
    .unwrap();
    local.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    let out = local.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    // Attached by hand, with a command line of its own.
    let kraal = env!("CARGO_BIN_EXE_kraal");
    local.run(&[
        "run",
        "--tab",
        "-n",
        "by-hand",
        "--",
        "env",
        "-u",
        "KRAAL_PANE",
        kraal,
        "-s",
        "tremote",
        "attach",
        "--host",
        "gpu",
    ]);
    local.wait_for_text("by-hand", "IN-1-ONE");
    let mut client = RawClient::attach(&local, "w");
    client.wait_for_screen("gpu › ");
    client.wait_for_screen("blocked · claude · two");
    // Another terminal attached there, used last: what that session is asked without saying
    // which client goes to that one.
    let mut other = RawClient::attach(&remote, "one");
    std::thread::sleep(Duration::from_millis(500));
    let shown = |word: &str, not: &str| {
        let start = Instant::now();
        loop {
            let screen = local.run(&["read", "-p", "by-hand"]);
            if screen.contains(word) && !screen.contains(not) {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "{word} is not what the tab shows:\n{screen}");
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    // The client here looks at that tab (opened last, it is the one its workspace shows), which
    // shows the agent of `one`: that one is seen already, so alt+a goes to the other, then back.
    // Each time, the tab must come to show what it did not show before alt+a.
    let panes: serde_json::Value = serde_json::from_str(&local.run(&["panes", "--json"])).unwrap();
    let tab = panes.as_array().unwrap().iter().find(|p| p["name"] == "by-hand").cloned().unwrap();
    assert_eq!(tab["focused"], true, "{tab}");
    for (word, not) in [("IN-2-TWO", "IN-1-ONE"), ("IN-1-ONE", "IN-2-TWO"), ("IN-2-TWO", "IN-1-ONE")] {
        shown(not, word);
        // The terminal there gained the focus: it is the one used last there.
        other.send(b"\x1b[I");
        std::thread::sleep(Duration::from_millis(200));
        client.send(b"\x1ba");
        shown(word, not);
    }
    let panes: serde_json::Value = serde_json::from_str(&local.run(&["panes", "--json"])).unwrap();
    assert_eq!(panes.as_array().unwrap().len(), 2, "no other tab attached: {panes}");
}

/// Copy the program `from` to `to`. A child process copies it, as one writes a script
/// ([`write_script`]): a program this process wrote cannot be run ("text file busy") while a
/// child that another test started at that moment still holds its file, on its way to running
/// what it was started for.
fn copy_program(from: &str, to: &Path) {
    let copied = Command::new("cp").arg(from).arg(to).status().unwrap();
    assert!(copied.success(), "copying {from} to {}", to.display());
}

/// A release as GitHub serves it, made from this build: `kraal-<target>.tar.gz` and its
/// checksum, in `dir`.
fn fake_release(dir: &Path) -> String {
    fake_release_of(dir, None)
}

/// A release, made from this build; with `version`, its kraal says it is that version and is
/// this build otherwise.
fn fake_release_of(dir: &Path, version: Option<&str>) -> String {
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        _ => "unknown-linux-musl",
    };
    let target = format!("{}-{os}", std::env::consts::ARCH);
    let inner = dir.join(format!("kraal-{target}"));
    std::fs::create_dir_all(&inner).unwrap();
    match version {
        Some(v) => write_script(
            &inner.join("kraal"),
            &format!(
                "#!/bin/sh\n[ \"$1\" = --version ] && {{ echo 'kraal {v}'; exit 0; }}\nexec '{}' \"$@\"\n",
                env!("CARGO_BIN_EXE_kraal")
            ),
        ),
        None => copy_program(env!("CARGO_BIN_EXE_kraal"), &inner.join("kraal")),
    }
    let asset = format!("kraal-{target}.tar.gz");
    let tar = Command::new("tar")
        .arg("-czf")
        .arg(dir.join(&asset))
        .arg("-C")
        .arg(dir)
        .arg(format!("kraal-{target}"))
        .status()
        .unwrap();
    assert!(tar.success());
    let sum = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "cd '{}' && (sha256sum {asset} 2>/dev/null || shasum -a 256 {asset}) > {asset}.sha256",
            dir.display()
        ))
        .status()
        .unwrap();
    assert!(sum.success());
    asset
}

/// `kraal update` puts a release that matches its checksum in place of itself, and leaves
/// itself alone when the checksum is wrong.
#[test]
fn update_installs_a_checked_release_in_place() {
    let env = Env::new("update");
    let release = env.root.join("release");
    let asset = fake_release(&release);
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let kraal = bin.join("kraal");
    copy_program(env!("CARGO_BIN_EXE_kraal"), &kraal);
    let url = format!("file://{}", release.display());
    let update =
        |args: &[&str]| env.isolated(&kraal).arg("update").args(args).env("KRAAL_DOWNLOAD_URL", &url).output().unwrap();

    let same = update(&[]);
    assert!(same.status.success(), "{}", String::from_utf8_lossy(&same.stderr));
    assert!(String::from_utf8_lossy(&same.stdout).contains("already installed"));

    let before = std::fs::metadata(&kraal).unwrap().modified().unwrap();
    std::thread::sleep(Duration::from_millis(20));
    let forced = update(&["--force"]);
    assert!(forced.status.success(), "{}", String::from_utf8_lossy(&forced.stderr));
    assert_ne!(std::fs::metadata(&kraal).unwrap().modified().unwrap(), before, "replaced");
    assert!(Command::new(&kraal).arg("--version").status().unwrap().success());
    let left: Vec<_> = std::fs::read_dir(&bin).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(left, ["kraal"], "nothing left beside it");

    std::fs::write(release.join(format!("{asset}.sha256")), format!("{}  {asset}\n", "0".repeat(64))).unwrap();
    let before = std::fs::metadata(&kraal).unwrap().modified().unwrap();
    let wrong = update(&["--force"]);
    assert!(!wrong.status.success());
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("does not match its checksum"));
    assert_eq!(std::fs::metadata(&kraal).unwrap().modified().unwrap(), before, "left alone");
}

/// After an update, `kraal update` names the sessions still on an older kraal with what a
/// restart stops in them, asks on a terminal (no by default when programs would stop), and has
/// the new kraal restart them when told to.
#[test]
fn update_offers_to_restart_the_sessions_left_on_an_older_kraal() {
    let env = Env::new("updrestart");
    env.workspace(
        "w",
        "layout {\n    pane name=\"job\" command=\"sh\" { args \"-c\" \"echo started; exec sleep 4243\"; }\n}\n",
    );
    env.run(&["open", "w"]);
    env.wait_for_text("job", "started");
    let release = env.root.join("release");
    fake_release_of(&release, Some("9.9.9"));
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let kraal = bin.join("kraal");
    // Each update replaces this copy with the release's kraal: put this build back first.
    let install = || {
        let _ = std::fs::remove_file(&kraal);
        copy_program(env!("CARGO_BIN_EXE_kraal"), &kraal);
    };
    let url = format!("file://{}", release.display());
    let update = |args: &[&str]| {
        let mut c = env.isolated(&kraal);
        c.arg("update").args(args).env("KRAAL_DOWNLOAD_URL", &url);
        c
    };
    let version = env!("CARGO_PKG_VERSION");
    let pid = env.server_pid().expect("the server runs");

    // Without a terminal to ask on, and without --restart: said, not done.
    install();
    let out = update(&[]).output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{said}{}", String::from_utf8_lossy(&out.stderr));
    assert!(said.contains(&format!("Session updrestart still runs kraal {version}.")), "{said}");
    assert!(said.contains("These programs stop:") && said.contains("\"job\" in w)"), "{said}");
    assert!(said.contains(&format!("Session updrestart keeps kraal {version} until: kraal -s updrestart restart")));
    assert_eq!(env.server_pid(), Some(pid));

    // On a terminal: asked, and Enter says no, since a program would stop.
    #[cfg(target_os = "linux")]
    {
        install();
        let screen = in_terminal(update(&[]), b"\n");
        assert!(screen.contains("Restart it on kraal 9.9.9 now? [y/N]"), "{screen}");
        assert!(screen.contains("keeps kraal"), "{screen}");
        assert_eq!(env.server_pid(), Some(pid));
        // --no-restart answers beforehand.
        install();
        let screen = in_terminal(update(&["--no-restart"]), b"");
        assert!(screen.contains("keeps kraal") && !screen.contains("[y/N]"), "{screen}");
        assert_eq!(env.server_pid(), Some(pid));
    }

    // --restart: the kraal just installed restarts it.
    install();
    let out = update(&["--restart"]).output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{said}{}", String::from_utf8_lossy(&out.stderr));
    assert!(said.contains("restarted session updrestart"), "{said}");
    let new = env.server_pid().expect("a server runs again");
    assert_ne!(new, pid);
    env.wait_for_text_since_restore("job", "restored session");
}

/// The server says what it runs to a client that attaches, and says once, to a client that
/// has a newer kraal installed, that a restart brings it. `kraal ls` shows each session's.
#[test]
fn a_client_with_a_newer_kraal_installed_is_told_the_session_runs_an_older_one() {
    let env = Env::new("notice");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    let version = env!("CARGO_PKG_VERSION");
    let newer = RawClient::attach_installed(&env, "w", Some("99.0.0"));
    newer.wait_for_message(&format!(r#"{{"type":"attached","version":"{version}","features":["#));
    newer.wait_for_screen(&format!("kraal 99.0.0 installed; this session runs {version}: ctrl+b W r restarts it"));
    let same = RawClient::attach_installed(&env, "w", Some(version));
    same.wait_for_message("\"attached\"");
    let older = RawClient::attach_installed(&env, "w", Some("0.0.1"));
    older.wait_for_message("\"attached\"");
    std::thread::sleep(Duration::from_millis(500));
    for client in [&same, &older] {
        assert!(!String::from_utf8_lossy(&client.screen.lock().unwrap()).contains(" installed; "));
    }
    let ls = env.run(&["ls"]);
    assert!(ls.lines().any(|l| l.starts_with("notice ") && l.ends_with(&format!("running  {version}"))), "{ls}");
    let keys = env.run(&["config", "keys"]);
    assert!(keys.lines().any(|l| l.trim_start().starts_with("restart ") && l.ends_with("ctrl+b W r")), "{keys}");
}

/// The restart in the menu says what stops and asks first; then the session comes back, and
/// attached clients are told to come back to the workspace they showed.
#[test]
fn the_menu_restarts_the_session_after_asking() {
    let env = Env::new("menurestart");
    env.workspace(
        "w",
        "layout {\n    pane name=\"job\" command=\"sh\" { args \"-c\" \"echo started; exec sleep 4244\"; }\n}\n",
    );
    env.run(&["open", "w"]);
    env.wait_for_text("job", "started");
    let pid = env.server_pid().expect("the server runs");
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_message("\"attached\"");
    client.send(b"\x02Wr");
    client.wait_for_screen("Restart session menurestart on the kraal installed now?");
    client.wait_for_screen("These programs stop:");
    client.wait_for_screen("\"job\" in w)");
    client.send(b"n");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(env.server_pid(), Some(pid), "no is no");
    client.screen.lock().unwrap().clear();
    client.send(b"\x02Wr");
    client.wait_for_screen("Restart session menurestart");
    client.send(b"y");
    client.wait_for_message(r#""reattach":"w""#);
    let start = Instant::now();
    while env.server_pid().is_none_or(|p| p == pid) {
        assert!(start.elapsed() < Duration::from_secs(20), "the session did not come back");
        std::thread::sleep(Duration::from_millis(50));
    }
    env.wait_for_text_since_restore("job", "restored session");
}

/// However a client leaves, it gives its terminal back once, and in order: detach, a signal
/// (also while frames are on their way), the server dying, quit.
#[cfg(target_os = "linux")]
#[test]
fn a_client_gives_its_terminal_back_once_however_it_leaves() {
    let env = Env::new("giveback");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    let mut client = TerminalClient::attach(&env);
    assert!(!client.line_mode(), "raw mode while attached");
    client.type_keys(b"\x02d");
    assert_eq!(client.left("detach"), (0, "[kraal: detached]\r\n".to_string()));

    for (signal, name) in [(libc::SIGTERM, "SIGTERM"), (libc::SIGHUP, "SIGHUP"), (libc::SIGINT, "SIGINT")] {
        let client = TerminalClient::attach(&env);
        client.signal(signal);
        assert_eq!(client.left(name), (128 + signal as u32, String::new()));
    }

    // A frame on its way when the signal comes is written whole, and none follows.
    env.run(&["send", "-p", "shell", "--enter", "seq 1 100000000"]);
    let client = TerminalClient::attach(&env);
    client.wait_for_frames(20);
    client.signal(libc::SIGTERM);
    assert_eq!(client.left("SIGTERM while a pane prints"), (128 + libc::SIGTERM as u32, String::new()));
    env.run(&["keys", "-p", "shell", "ctrl+c"]);

    let client = TerminalClient::attach(&env);
    let server = env.server_pid().expect("the server runs");
    // SAFETY: plain syscall to the server this test started.
    unsafe { libc::kill(server, libc::SIGKILL) };
    let (status, said) = client.left("the server dies");
    assert!(status == 0 && said.starts_with("[kraal: the server went away"), "{status} {said:?}");
    wait_gone(&PathBuf::from(format!("/proc/{server}")));

    env.run(&["open", "w"]);
    let mut client = TerminalClient::attach(&env);
    client.type_keys(b"\x02q");
    client.wait_for("Quit kraal?");
    client.type_keys(b"y");
    let (status, said) = client.left("quit");
    assert!(status == 0 && said.starts_with("[kraal: stopped; kraal -s giveback starts it again"), "{status} {said:?}");
}

/// The server stops for a restart and none comes back: the client turned its input modes off
/// for the wait, and giving the wait up (ctrl+c, or a signal) does not turn them off again.
#[cfg(target_os = "linux")]
#[test]
fn a_restart_wait_given_up_gives_the_terminal_back_once() {
    for by_signal in [false, true] {
        let env = Env::new(if by_signal { "waitsignal" } else { "waitkey" });
        env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
        env.run(&["open", "w"]);
        let mut client = TerminalClient::attach(&env);
        let socket = env.root.join("run/kraal").join(format!("{}.sock", env.session));
        let mut asking = std::os::unix::net::UnixStream::connect(socket).unwrap();
        raw_frame(&mut asking, 0, br#"{"type":"request","version":2,"request":{"cmd":"kill_server","restart":true}}"#);
        client.wait_for("kraal: the session restarts");
        if by_signal {
            client.signal(libc::SIGINT);
            assert_eq!(client.left("SIGINT during the wait"), (128 + libc::SIGINT as u32, String::new()));
        } else {
            client.type_keys(b"\x03");
            let said = "[kraal: gave up waiting for the session to restart]\r\n";
            assert_eq!(client.left("ctrl+c during the wait"), (0, said.to_string()));
        }
    }
}

/// A signal that comes while the client is leaving anyway finds the terminal given back, or
/// gives it back itself: never both.
#[cfg(target_os = "linux")]
#[test]
fn a_signal_that_meets_a_normal_exit_gives_the_terminal_back_once() {
    let env = Env::new("exitsignal");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    // The client leaves some 0.2 ms after the keys: the signal is sent all around that.
    for delay in (0..500).step_by(20).map(Duration::from_micros) {
        let mut client = TerminalClient::attach(&env);
        client.type_keys(b"\x02d");
        let typed = Instant::now();
        while typed.elapsed() < delay {
            std::hint::spin_loop();
        }
        client.signal(libc::SIGTERM);
        let (status, said) = client.left(&format!("SIGTERM {delay:?} after a detach"));
        // Keys the signal overtook are still in the terminal, which shows them as a shell's
        // would; a goodbye is whole, or unsaid.
        let said_by_signal = ["", "^Bd", "[kraal: detached]\r\n"];
        let as_expected = match status {
            0 => said == "[kraal: detached]\r\n",
            status => status == 128 + libc::SIGTERM as u32 && said_by_signal.contains(&said.as_str()),
        };
        assert!(as_expected, "{delay:?}: {status} {said:?}");
    }
}

#[test]
fn closing_a_workspace_lists_its_programs_and_background_jobs_from_both_entries() {
    let env = Env::new("wsclosejobs");
    env.workspace("api", "layout {\n    pane name=\"shell\"\n}\n");
    env.workspace("other", "layout {\n    pane name=\"unrelated\" command=\"sleep\" { args \"4259\"; }\n}\n");
    env.run(&["open", "api"]);
    env.run(&["open", "other"]);
    let job_pid = env.root.join("job.pid");
    env.run(&["send", "-p", "shell", "--enter", &format!("sleep 4257 & echo $! > '{}'", job_pid.display())]);
    wait_until(Duration::from_secs(10), "the background job never started", || {
        let pid = std::fs::read_to_string(&job_pid).unwrap_or_default();
        !pid.trim().is_empty()
            && Command::new("ps")
                .args(["-p", pid.trim(), "-o", "comm="])
                .output()
                .is_ok_and(|out| out.status.success() && String::from_utf8_lossy(&out.stdout).trim().ends_with("sleep"))
    });
    env.run(&["run", "--name", "foreground", "--target", "shell", "--", "sh", "-c", "echo ready; exec sleep 4258"]);
    env.wait_for_text("foreground", "ready");
    let before = env.run(&["panes"]);
    for size in [(80, 24), (124, 36)] {
        for picker in [false, true] {
            let mut client = RawClient::attach_sized(&env, Some(if picker { "other" } else { "api" }), size).traced();
            client.wait_for_message("\"attached\"");
            if picker {
                client.send(b"\x02w");
                client.wait_for_row("Workspaces");
                client.send(b"api");
                client.wait_for_row("Enter Open · Tab Actions · Esc Close");
                client.send(b"\x18");
            } else {
                client.send(b"\x02Wx");
            }
            client.wait_for_row("Close api?");
            let question = client.read_question(b"\x1b[6~").join("\n");
            for text in [
                "The layout will be kept.",
                "These programs will stop:",
                "sleep 4257 (in the background)",
                "sleep 4258",
                "Unsaved work in these programs may be lost.",
            ] {
                assert!(question.contains(text), "{size:?}, picker={picker}: {question}");
            }
            assert!(!question.contains("4259"), "{question}");
            assert!(!question.contains("linked file") && !question.contains("workspace file"), "{question}");
            assert!(!question.contains("(pane "), "{question}");
            assert!(!question.contains("The session will stop."), "{question}");
            client.wait_for_row("Stop and close");
            client.send(b"\r");
            client
                .wait_for_rows("Cancel closed the question", |rows| !rows.iter().any(|r| r.contains("Stop and close")));
            assert_eq!(env.run(&["panes"]), before);
        }
    }
    let mut client = RawClient::attach(&env, "api").traced();
    let other_client = RawClient::attach(&env, "api");
    other_client.wait_for_message("\"attached\"");
    client.wait_for_message("\"attached\"");
    client.send(b"\x02Wx");
    client.wait_for_row("Stop and close");
    client.send(b"y");
    client.wait_for_rows("the workspace closed", |rows| !rows.iter().any(|r| r.contains("Stop and close")));
    other_client.wait_for_row("unrelated");
    let panes = env.run(&["panes"]);
    assert!(!panes.contains("shell") && !panes.contains("foreground") && panes.contains("unrelated"), "{panes}");
    assert!(listed_workspaces(&env).contains(&"api: api.kdl".to_string()));
    assert_eq!(
        std::fs::read_to_string(env.root.join("config/kraal/workspaces/api.kdl")).unwrap(),
        "layout {\n    pane name=\"shell\"\n}\n"
    );
    env.run(&["open", "api"]);
    let panes = env.run(&["panes"]);
    assert!(panes.contains("shell") && panes.contains("foreground"), "{panes}");
}

#[test]
fn closing_a_workspace_without_a_file_keeps_it_and_the_last_session() {
    let env = Env::new("wscloselast");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    let pid = env.server_pid().unwrap();
    let mut client = RawClient::attach(&env, "w").traced();
    client.wait_for_message("\"attached\"");
    client.send(b"\x02Wn");
    client.wait_for_row("Name    ");
    client.send(b"\x01\x0bscratch\r");
    client.wait_for_rows("the workspace was created", |rows| !rows.iter().any(|r| r.contains("Name    ")));
    client.wait_for_row("$ ");
    for size in [(80, 24), (124, 36)] {
        client.resize(size);
        client.send(b"\x02Wx");
        client.wait_for_row("Closed · 1 tab · 1 pane");
        assert!(listed_workspaces(&env).contains(&"scratch: ".to_string()));
        assert_eq!(env.server_pid(), Some(pid));
        assert!(!client.messages.lock().unwrap().iter().any(|m| m.contains(r#""type":"exit""#)));
        client.send(b"\r");
        client.wait_for_rows("scratch reopened", |rows| !rows.iter().any(|r| r.contains("Workspaces")));
        client.wait_for_row("$ ");
        assert!(listed_workspaces(&env).contains(&"scratch:  (open)".to_string()));
    }
    client.send(b"\x02Wx");
    client.wait_for_row("Closed · 1 tab · 1 pane");
    client.send(b"\x1b");
    client.wait_for_rows("the workspace picker closed", |rows| !rows.iter().any(|r| r.contains("Workspaces")));
    client.wait_for_row("$ ");
    client.send(b"\x02Wx");
    client.wait_for_row("Workspaces");
    wait_until(PATIENCE, "both workspaces did not close", || {
        let workspaces: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
        workspaces.as_array().is_some_and(|ws| ws.len() == 2 && ws.iter().all(|w| w["open"] == false))
    });
    assert_eq!(env.server_pid(), Some(pid));
    assert!(env.state_file().exists());
    assert!(!client.messages.lock().unwrap().iter().any(|m| m.contains(r#""type":"exit""#)));
    env.run(&["kill"]);
    env.run(&["open", "scratch"]);
    assert!(listed_workspaces(&env).contains(&"scratch:  (open)".to_string()));
    assert!(listed_workspaces(&env).contains(&"w: w.kdl".to_string()));
}

/// `ctrl+b q` asks first, Cancel selected, saying what stops (a short question when nothing
/// would); a yes saves the layout and stops the session, every attached client is told how it
/// comes back, and the next start brings the layout back.
#[test]
fn the_menu_quits_after_asking_and_keeps_the_layout() {
    let env = Env::new("menuquit");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    let pid = env.server_pid().expect("the server runs");
    let mut client = RawClient::attach(&env, "w");
    let other = RawClient::attach(&env, "w");
    client.wait_for_message("\"attached\"");
    other.wait_for_message("\"attached\"");
    client.send(b"\x02");
    client.wait_for_row("d detach · q quit");
    client.send(b"q");
    let rows = client.wait_for_row("Quit kraal? The layout is saved for next time.");
    assert!(!rows.iter().any(|r| r.contains("These programs stop")), "a shell at its prompt:\n{}", rows.join("\n"));
    client.send(b"\r");
    client.wait_for_rows("the question gone", |rows| !rows.iter().any(|r| r.contains("Quit kraal?")));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(env.server_pid(), Some(pid), "Enter takes Cancel, selected first");

    env.run(&["run", "--name", "job", "--", "sh", "-c", "echo started; exec sleep 4249"]);
    env.wait_for_text("job", "started");
    client.send(b"\x02q");
    client.wait_for_row("Quit kraal? These programs stop:");
    let rows = client.wait_for_row("The layout is saved for next time.");
    // The line as `kraal restart` prints it, spacing included.
    let listed = rows.iter().any(|r| r.contains("  sleep 4249  (pane ") && r.contains("\"job\" in w)"));
    assert!(listed, "{}", rows.join("\n"));
    client.send(b"y");
    let said = "stopped; kraal -s menuquit starts it again with its layout";
    client.wait_for_message(said);
    other.wait_for_message(said);
    wait_until(Duration::from_secs(10), "the server still runs", || env.server_pid().is_none());
    env.run(&["open", "w"]);
    let panes = env.run(&["panes"]);
    assert!(panes.contains("shell") && panes.contains("job"), "{panes}");
}

/// A quit stops the session's programs only once its layout is saved: when it cannot be, the
/// session keeps running, and the quit says why.
#[test]
fn a_quit_that_cannot_save_the_session_leaves_it_running() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new("unsavedquit");
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    env.run(&["open", "w"]);
    wait_until(Duration::from_secs(10), "the layout was never saved", || env.state_file().exists());
    let pid = env.server_pid().expect("the server runs");
    let sessions = env.root.join("state/kraal/sessions-v2");
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o500)).unwrap();
    // A pane opened since the last save: the layout on disk is not the session's any more.
    env.run(&["run", "--name", "two", "--target", "one", "--", "sh"]);
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_message("\"attached\"");
    client.send(b"\x02q");
    client.wait_for_row("Quit kraal?");
    client.send(b"y");
    client.wait_for_rows("the question gone", |rows| !rows.iter().any(|r| r.contains("Quit kraal?")));
    client.wait_for_rows("the Quit failure on the notice line", |rows| {
        rows[0].contains("not quitting: the layout could not be saved")
    });
    client.send(b"\x02Vj");
    client.wait_for_row("not quitting: the layout could not be saved");
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(env.server_pid(), Some(pid), "the session stopped");
    assert!(!client.messages.lock().unwrap().iter().any(|m| m.contains(r#""type":"exit""#)));
}

/// An empty workspace remains usable, saved and attached after its last shell exits.
#[test]
fn the_last_pane_leaves_an_empty_saved_workspace() {
    let env = Env::new("lastexit");
    env.workspace("w", "layout {\n    pane name=\"only\"\n}\n");
    env.run(&["open", "w"]);
    let pid = env.server_pid().unwrap();
    let client = RawClient::attach(&env, "w");
    client.wait_for_message("\"attached\"");
    let before: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    env.run(&["send", "-p", "only", "--enter", "exit"]);
    client.wait_for_row("+ Add pane");
    assert_eq!(env.server_pid(), Some(pid));
    let after: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    assert_eq!(after[0]["id"], before[0]["id"]);
    assert_eq!(after[0]["tabs"], 0);
    assert_eq!(after[0]["panes"], 0);
    env.run(&["kill"]);
    env.run(&["open", "w"]);
    let restored: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    assert_eq!(restored, after);
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_row("+ Add pane");
    client.send(b"\r");
    wait_until(PATIENCE, "Enter did not add a pane", || {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        panes.as_array().is_some_and(|p| p.len() == 1)
    });
}

/// Closing keeps the personal layout even when its imported source changes or disappears.
#[test]
fn close_and_reopen_keep_unsaved_tabs_panes_and_identity() {
    let env = Env::new("closekept");
    let source = "layout {\n    pane name=\"shell\"\n}\n";
    env.workspace("w", source);
    env.run(&["open", "w"]);
    env.run(&["run", "--tab", "--name", "job", "--", "sh", "-c", "echo ready; exec sleep 4209"]);
    env.wait_for_text("job", "ready");
    let before: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    let mut client = RawClient::attach(&env, "w");
    let other = RawClient::attach(&env, "w");
    client.wait_for_message("\"attached\"");
    client.send(b"\x02Wx");
    client.wait_for_row("The layout will be kept.");
    client.send(b"y");
    client.wait_for_row("Closed · 2 tabs · 2 panes");
    other.wait_for_row("Closed · 2 tabs · 2 panes");
    let closed: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    assert_eq!(closed[0]["id"], before[0]["id"]);
    assert_eq!(closed[0]["open"], false);
    assert!(env.run(&["workspaces"]).contains("closed"));
    assert_eq!(std::fs::read_to_string(env.root.join("config/kraal/workspaces/w.kdl")).unwrap(), source);
    std::fs::remove_file(env.root.join("config/kraal/workspaces/w.kdl")).unwrap();
    env.run(&["kill"]);
    let saved = std::fs::read(env.state_file()).unwrap();
    let stopped: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    assert_eq!(stopped, closed);
    assert_eq!(std::fs::read(env.state_file()).unwrap(), saved, "listing never rewrites saved state");
    env.run(&["open", "w"]);
    let reopened: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    assert_eq!(reopened[0]["id"], before[0]["id"]);
    assert_eq!(reopened[0]["tabs"], 2);
    assert_eq!(reopened[0]["panes"], 2);
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    assert!(panes.as_array().unwrap().iter().any(|p| p["name"] == "job" && p["suspended"] == true));
    assert!(env.run(&["read", "-p", "job"]).contains("ready"));
}

#[test]
fn close_preparation_failure_keeps_the_programs_and_offers_retry() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new("closefull");
    env.workspace("w", "layout { pane name=\"only\"; }\n");
    env.run(&["open", "w"]);
    env.run(&["send", "-p", "only", "--enter", "echo waiting"]);
    env.wait_for_text("only", "waiting");
    wait_until(PATIENCE, "no saved state", || env.state_file().exists());
    let sessions = env.root.join("state/kraal/sessions-v2");
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o500)).unwrap();
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_message("\"attached\"");
    client.send(b"\x02Wx");
    client.wait_for_row("Nothing was stopped.");
    env.run(&["send", "-p", "only", "--enter", "echo still-running"]);
    env.wait_for_text("only", "still-running");
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o700)).unwrap();
    client.send(b"y");
    client.wait_for_row("Closed · 1 tab · 1 pane");
}

#[test]
fn commands_outside_kraal_skip_a_closed_first_workspace() {
    let env = Env::new("closedfirst");
    let bin = env.root.join("bin");
    let home = env.root.join("account");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    write_script(&bin.join("ssh"), "#!/bin/sh\necho connected\nexec sleep 60\n");
    write_script(&bin.join("claude"), "#!/bin/sh\n[ \"$2\" = status ] && exit 0\necho agent-ready\nexec sleep 60\n");
    std::fs::write(
        env.root.join("config/kraal/accounts.toml"),
        format!("[[account]]\nname = \"work\"\nagent = \"claude\"\nhome = {:?}\n", home.display().to_string()),
    )
    .unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    env.workspace("a", "layout { pane name=\"first\"; }\n");
    env.workspace("b", "layout { pane name=\"second\"; }\n");
    assert!(env.cmd(&["open", "a"]).env("PATH", &path).status().unwrap().success());
    env.run(&["open", "b"]);
    let mut client = RawClient::attach(&env, "a");
    client.wait_for_row("$ ");
    client.send(b"\x02Wx");
    client.wait_for_row("Closed · 1 tab · 1 pane");
    drop(client);
    // No attached client, and the first retained workspace is Closed.
    let restarted = env.cmd(&["restart", "--yes"]).env("PATH", &path).output().unwrap();
    assert!(restarted.status.success(), "{}", String::from_utf8_lossy(&restarted.stderr));
    let run = env.run(&["run", "--name", "new", "--", "sh"]);
    // Address the server without a pane or attached terminal as the agent's destination.
    let agent = env
        .cmd(&["agent", "claude", "--account", "work", "--tab"])
        .env("KRAAL_SOCKET", env.root.join("run/kraal/closedfirst.sock"))
        .output()
        .unwrap();
    assert!(agent.status.success(), "{}", String::from_utf8_lossy(&agent.stderr));
    let agent = String::from_utf8(agent.stdout).unwrap();
    env.run(&["ssh", "example.invalid", "--tab"]);
    env.run(&["files", env.root.to_str().unwrap()]);
    env.wait_for_text(agent.trim(), "agent-ready");
    let panes: Vec<serde_json::Value> = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let ssh = panes.iter().find(|p| p["command"][0] == "ssh").unwrap();
    env.wait_for_text(&ssh["id"].to_string(), "connected");
    assert_eq!(panes.len(), 5);
    assert!(panes.iter().all(|p| p["workspace"] == "b"), "{panes:?}");
    assert!(panes.iter().any(|p| p["id"].as_u64() == Some(run.trim().parse::<u64>().unwrap())));
    let workspaces: Vec<serde_json::Value> = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    assert_eq!(workspaces[0]["name"], "a");
    assert_eq!(workspaces[0]["open"], false);
}

#[test]
fn a_server_runbook_after_the_last_close_gets_an_open_saved_workspace() {
    let env = Env::new("runafterclose");
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(&bin.join("ssh"), "#!/bin/sh\nfor a; do cmd=$a; done\nexec sh -c \"$cmd\"\n");
    std::fs::write(env.root.join("config/kraal/servers.toml"), "[[server]]\nname = \"box\"\nhost = \"box.invalid\"\n")
        .unwrap();
    std::fs::create_dir_all(env.root.join("config/kraal/runbooks")).unwrap();
    std::fs::write(env.root.join("config/kraal/runbooks/check.toml"), "on = \"server\"\nsteps = [\"echo checked\"]\n")
        .unwrap();
    env.workspace("w", "layout { pane name=\"original\"; }\n");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    assert!(env.cmd(&["open", "w"]).env("PATH", &path).status().unwrap().success());
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_row("$ ");
    client.send(b"\x02Wx");
    client.wait_for_row("Closed · 1 tab · 1 pane");
    let result = env.run(&["runbook", "run", "check", "--on", "box", "--yes", "--wait"]);
    assert!(result.contains("finished: 1 ok, 0 failed, 0 skipped"), "{result}");
    env.wait_for_text("box", "checked");
    let workspaces: Vec<serde_json::Value> = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    assert_eq!(workspaces.len(), 2);
    assert_eq!(workspaces[0]["name"], "w");
    assert_eq!(workspaces[0]["open"], false);
    assert_eq!(workspaces[1]["open"], true);
    assert_eq!(workspaces[1]["panes"], 1);
    env.run(&["kill"]);
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(env.state_file()).unwrap()).unwrap();
    assert_eq!(saved["workspaces"][1]["tabs"][0]["root"]["Pane"]["name"], "box");
}

#[test]
fn the_ssh_group_command_reopens_its_closed_workspace() {
    let env = Env::new("groupreopen");
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(&bin.join("ssh"), "#!/bin/sh\necho connected\nexec sleep 60\n");
    std::fs::write(
        env.root.join("config/kraal/servers.toml"),
        "[[server]]\nname = \"box\"\nhost = \"box.invalid\"\ngroups = [\"prod\"]\n",
    )
    .unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    assert!(env.cmd(&["ssh", "prod", "--group"]).env("PATH", &path).status().unwrap().success());
    env.wait_for_text("box", "connected");
    let before: Vec<serde_json::Value> = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    let name = before[0]["name"].as_str().unwrap();
    let mut client = RawClient::attach(&env, name);
    client.wait_for_message("\"attached\"");
    client.send(b"\x02Wx");
    client.wait_for_row("The layout will be kept.");
    client.send(b"y");
    client.wait_for_row("Closed · 1 tab · 1 pane");
    env.run(&["ssh", "prod", "--group"]);
    env.wait_for_text_since_restore("box", "connected");
    let after: Vec<serde_json::Value> = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0]["id"], before[0]["id"]);
    assert_eq!(after[0]["open"], true);
    assert_eq!(after[0]["panes"], 1);
}

#[test]
fn a_crash_during_close_finishes_stopping_without_reopening_the_workspace() {
    let env = Env::new("closecrash");
    env.workspace("w", "layout { pane name=\"job\" command=\"sh\" { args \"-c\" \"trap '' HUP TERM; echo ready; while :; do sleep 1; done\"; }; }\n");
    env.workspace("other", "layout { pane; }\n");
    env.run(&["open", "w"]);
    env.wait_for_text("job", "ready");
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_message("\"attached\"");
    client.send(b"\x02Wx");
    client.wait_for_row("The layout will be kept.");
    client.send(b"y");
    wait_until(PATIENCE, "close was not prepared", || {
        std::fs::read(env.state_file())
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .is_some_and(|s| s["workspaces"][0]["close_pending"].is_object())
    });
    let pid = env.server_pid().unwrap();
    // SAFETY: only the isolated server this test started, after its close intent reached disk.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    wait_until(PATIENCE, "the crashed server still runs", || env.server_pid().is_none());
    // Its process information can disappear before its listening socket closes.
    let socket = env.root.join("run/kraal/closecrash.sock");
    wait_until(PATIENCE, "the crashed server still listens", || {
        std::os::unix::net::UnixStream::connect(&socket).is_err()
    });
    env.run(&["open", "other"]);
    let list: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    let ws = list.as_array().unwrap().iter().find(|w| w["name"] == "w").unwrap();
    assert_eq!(ws["open"], false);
    assert_eq!(ws["panes"], 1);
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    assert!(panes.as_array().unwrap().iter().all(|p| p["workspace"] != "w"));
}

#[test]
fn a_crash_during_delete_finishes_the_deletion_and_keeps_it_restorable() {
    let env = Env::new("deletecrash");
    env.workspace("w", "layout { pane name=\"job\" command=\"sh\" { args \"-c\" \"trap '' HUP TERM; echo ready; while :; do sleep 1; done\"; }; }\n");
    env.run(&["open", "w"]);
    env.wait_for_text("job", "ready");
    let mut client = RawClient::attach_sized(&env, Some("w"), (80, 24));
    client.wait_for_message("\"attached\"");
    client.send(b"\x02w\t");
    click_account_text(&mut client, "Delete");
    client.wait_for_row("Stop and delete");
    client.send(b"y");
    wait_until(PATIENCE, "deletion was not prepared", || {
        std::fs::read(env.state_file())
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .is_some_and(|s| {
                s["workspaces"][0]["delete_pending"] == true && s["workspaces"][0]["close_pending"].is_object()
            })
    });
    let pid = env.server_pid().unwrap();
    // SAFETY: only this test's server, after its deletion intent reached disk.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    wait_until(PATIENCE, "the crashed server still runs", || env.server_pid().is_none());
    let socket = env.root.join("run/kraal/deletecrash.sock");
    wait_until(PATIENCE, "the crashed server still listens", || {
        std::os::unix::net::UnixStream::connect(&socket).is_err()
    });
    env.run(&["tunnel", "list"]);
    assert!(listed_workspaces(&env).is_empty());
    assert_eq!(env.run(&["panes", "--json"]).trim(), "[]");
    let mut client = RawClient::attach_sized(&env, None, (80, 24));
    client.wait_for_row("Deleted workspaces (1)");
    click_account_text(&mut client, "Deleted workspaces (1)");
    client.wait_for_row("Deleted today");
    client.send(b"\r");
    client.wait_for_row("Workspace restored");
    let restored: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    assert_eq!(restored[0]["name"], "w");
    assert_eq!(restored[0]["open"], false);
    assert_eq!(restored[0]["panes"], 1);
    assert_eq!(env.run(&["panes", "--json"]).trim(), "[]");
}

/// A command pane stays open when its program ends, to show how it ended: it has not ended, and
/// closing it leaves the workspace empty and the session running.
#[test]
fn a_command_pane_left_open_keeps_its_session() {
    let env = Env::new("keptopen");
    env.workspace("w", "layout {\n    pane name=\"job\" command=\"sh\" { args \"-c\" \"sleep 0.3; exit 3\"; }\n}\n");
    env.run(&["open", "w"]);
    let pid = env.server_pid().expect("the server runs");
    let client = RawClient::attach(&env, "w");
    client.wait_for_message("\"attached\"");
    env.run(&["wait", "-p", "job", "--until", "exit", "--timeout", "10"]);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(env.server_pid(), Some(pid), "the session ended");
    assert!(!client.messages.lock().unwrap().iter().any(|m| m.contains(r#""type":"exit""#)));
    env.run(&["close", "job"]);
    client.wait_for_row("+ Add pane");
    assert_eq!(env.server_pid(), Some(pid));
}

/// `kraal restart` says what would stop before asking; a client attached in a terminal waits for
/// the next server and comes back to its workspace; an idle shell comes back; and a restart run
/// from a pane of the session itself outlives that pane.
#[cfg(target_os = "linux")]
#[test]
fn restart_keeps_the_session_and_brings_its_clients_back() {
    let env = Env::new("restart");
    env.workspace(
        "w",
        "layout {\n    pane split_direction=\"vertical\" {\n        pane name=\"idle\"\n        pane name=\"busy\"\n    }\n}\n",
    );
    env.run(&["open", "w"]);
    env.run(&["send", "-p", "idle", "--enter", "echo before-restart"]);
    env.wait_for_text("idle", "before-restart");
    env.run(&["send", "-p", "busy", "--enter", "sleep 4245"]);
    let sleeping = || {
        std::fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .any(|e| std::fs::read(e.path().join("cmdline")).is_ok_and(|c| c == b"sleep\x004245\x00"))
    };
    let start = Instant::now();
    while !sleeping() {
        assert!(start.elapsed() < Duration::from_secs(10), "sleep never started");
        std::thread::sleep(Duration::from_millis(50));
    }
    // A client in a 30-row terminal: once it is attached, the panes are shorter than that.
    let mut client = env.client();
    let attached = || {
        let start = Instant::now();
        loop {
            let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
            if panes.as_array().unwrap().iter().all(|p| p["rows"].as_u64().unwrap() < 30) {
                return;
            }
            assert!(start.elapsed() < Duration::from_secs(20), "no client attached: {panes}");
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    attached();
    let pid = env.server_pid().expect("the server runs");

    // Without a terminal to ask on: what stops is said, and nothing happens.
    let out = env.try_run(&["restart"]);
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "{said}");
    let listed = said.split_once("These programs stop:\n").map(|(_, rest)| rest).unwrap_or_default();
    assert!(listed.starts_with("  sleep 4245  (pane ") && listed.contains(" \"busy\" in w)"), "{said}");
    assert!(!said.contains("idle"), "a shell at its prompt runs nothing: {said}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("add --yes"));
    assert_eq!(env.server_pid(), Some(pid));
    // On a terminal: the list, then the question, whose Enter is no.
    let screen = in_terminal(env.cmd(&["restart"]), b"\n");
    let (listed, asked) = (screen.find("sleep 4245"), screen.find("Restart session restart? [y/N]"));
    assert!(listed.is_some() && asked.is_some() && listed < asked, "{screen}");
    assert!(screen.contains("cancelled"), "{screen}");
    assert_eq!(env.server_pid(), Some(pid));

    let out = env.run(&["restart", "--yes"]);
    assert!(out.contains("restarted session restart"), "{out}");
    let second = env.server_pid().expect("a server runs again");
    assert_ne!(second, pid);
    // The idle shell is back, with what it showed, and takes commands.
    env.wait_for_text("idle", "before-restart");
    env.run(&["send", "-p", "idle", "--enter", "echo after-restart"]);
    env.wait_for_text_since_restore("idle", "after-restart");
    // The client did not leave: it attached to the next server.
    assert!(client.try_wait().unwrap().is_none(), "the client left");
    attached();

    // From a pane of the session: the restart closes that pane, and happens all the same.
    let line = format!("'{}' restart --yes", env!("CARGO_BIN_EXE_kraal"));
    env.run(&["send", "-p", "idle", "--enter", &line]);
    let start = Instant::now();
    while env.server_pid().is_none_or(|p| p == second) {
        assert!(start.elapsed() < Duration::from_secs(20), "the session did not restart");
        std::thread::sleep(Duration::from_millis(50));
    }
    let start = Instant::now();
    while !env.try_run(&["read", "-p", "idle", "--lines", "200"]).stdout.windows(13).any(|w| w == b"after-restart") {
        assert!(start.elapsed() < Duration::from_secs(10), "the idle pane did not come back");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(client.try_wait().unwrap().is_none(), "the client left");
    attached();
    let _ = client.kill();
    let _ = client.wait();
}

/// A restart run in place of the shell of the session's last pane (`exec`): the command stays
/// until the restart has the session, which would otherwise end with that pane, its layout
/// deleted, and the session comes back.
#[cfg(target_os = "linux")]
#[test]
fn a_restart_from_the_last_pane_outlives_it() {
    let env = Env::new("lastpane");
    env.workspace("w", "layout {\n    pane name=\"only\"\n}\n");
    env.run(&["open", "w"]);
    let pidfile = env.root.join("pane.pid");
    env.run(&["send", "-p", "only", "--enter", &format!("echo $$ > '{}'", pidfile.display())]);
    let mut pane = PathBuf::new();
    wait_until(Duration::from_secs(10), "the shell never said its pid", || {
        let pid = std::fs::read_to_string(&pidfile).unwrap_or_default();
        pane = PathBuf::from(format!("/proc/{}", pid.trim()));
        pid.ends_with('\n')
    });
    let cmdline = |process: &Path| std::fs::read(process.join("cmdline")).unwrap_or_default();
    let kraal = env!("CARGO_BIN_EXE_kraal");
    let go = env.root.join("go");
    let line = format!("while [ ! -e '{}' ]; do sleep 0.1; done; exec '{kraal}' restart --yes", go.display());
    env.run(&["send", "-p", "only", "--enter", &line]);
    // With the server stopped, the restart waits for it, and nothing can end the command in the
    // pane meanwhile but the command itself.
    let server = env.server_pid().expect("the server runs");
    let stopped = Stopped::stop(server);
    std::fs::write(&go, "").unwrap();
    let helper = [kraal.as_bytes(), b"\0-s\0lastpane\0restart\0--yes\0"].concat();
    let started = || std::fs::read_dir("/proc").unwrap().flatten().any(|e| cmdline(&e.path()) == helper);
    wait_until(Duration::from_secs(20), "the restart never started", started);
    let command = [kraal.as_bytes(), b"\0restart\0--yes\0"].concat();
    for _ in 0..30 {
        assert_eq!(cmdline(&pane), command, "the command left the pane before the restart had the session");
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(stopped);
    wait_until(Duration::from_secs(30), "the session did not come back", || {
        env.server_pid().is_some_and(|p| p != server)
    });
    assert!(env.run(&["panes"]).contains("only"));
}

/// `kraal restart` run in a pane of the session says what stops as it does from anywhere else:
/// the jobs in the background of its own pane too. Only the command itself and what runs it (a
/// script) are not programs that stop, in the foreground or in the background.
#[cfg(target_os = "linux")]
#[test]
fn a_restart_from_a_pane_says_what_stops_in_that_pane_too() {
    let env = Env::new("ownjobs");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    let pid = env.server_pid().expect("the server runs");
    let running = |argv: &[u8]| {
        std::fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .any(|e| std::fs::read(e.path().join("cmdline")).is_ok_and(|c| c == argv))
    };
    env.run(&["send", "-p", "shell", "--enter", "sleep 4251 &"]);
    wait_until(Duration::from_secs(10), "the job never started", || running(b"sleep\x004251\x00"));
    // Without a terminal to ask on, the command says what stops and goes no further.
    let said = env.root.join("said");
    let restart = format!("'{}' restart </dev/null >'{}' 2>&1", env!("CARGO_BIN_EXE_kraal"), said.display());
    let script = env.root.join("restart.sh");
    write_script(&script, &format!("#!/bin/sh\n{restart}\n"));
    let stops = |typed: &str| -> Vec<String> {
        let _ = std::fs::remove_file(&said);
        env.run(&["send", "-p", "shell", "--enter", typed]);
        let mut text = String::new();
        wait_until(Duration::from_secs(10), "the restart did not stop at what it would stop", || {
            text = std::fs::read_to_string(&said).unwrap_or_default();
            text.contains("add --yes")
        });
        text.lines().filter_map(|line| line.strip_prefix("  ")).map(String::from).collect()
    };
    // The pane's shell may have its own standard input elsewhere while the command runs: dash
    // does that for a command given `< file`, and this line does it in any shell. The jobs are
    // those of the shell's terminal all the same.
    let elsewhere = |command: &str| format!("exec 3<&0 </dev/null; {command}; exec <&3 3<&-");
    let typed = [
        restart.clone(),
        format!("sh '{}'", script.display()),
        format!("{restart} &"),
        elsewhere(&format!("'{}' restart >'{}' 2>&1", env!("CARGO_BIN_EXE_kraal"), said.display())),
    ];
    for typed in typed {
        let stops = stops(&typed);
        assert_eq!(stops.len(), 1, "{typed}: {stops:?}");
        assert!(stops[0].starts_with("sleep 4251 (in the background)  (pane "), "{typed}: {stops:?}");
    }
    // The same list as from outside the session.
    let outside = || {
        let out = env.try_run(&["restart"]);
        assert!(!out.status.success());
        let said = String::from_utf8_lossy(&out.stdout).into_owned();
        said.lines().filter_map(|line| line.strip_prefix("  ")).map(String::from).collect::<Vec<_>>()
    };
    let stops = outside();
    assert_eq!(stops.len(), 1, "{stops:?}");
    assert!(stops[0].starts_with("sleep 4251 (in the background)  (pane "), "{stops:?}");
    // And from outside while that shell runs a program with its standard input elsewhere.
    env.run(&["send", "-p", "shell", "--enter", &elsewhere("sleep 4253")]);
    wait_until(Duration::from_secs(10), "the program never started", || running(b"sleep\x004253\x00"));
    let stops = outside();
    assert_eq!(stops.len(), 2, "{stops:?}");
    assert!(stops[0].starts_with("sleep 4253  (pane "), "{stops:?}");
    assert!(stops[1].starts_with("sleep 4251 (in the background)  (pane "), "{stops:?}");
    assert_eq!(env.server_pid(), Some(pid));
    assert!(running(b"sleep\x004251\x00"));
}

/// A job left in the background of a shell stops with a restart (the terminal hangs up): it is
/// said with what stops, and the restart needs a yes for it. What the shell runs off its
/// terminal (a prompt's daemon) is not the user's job.
#[cfg(target_os = "linux")]
#[test]
fn a_restart_says_it_stops_the_jobs_in_the_background() {
    let env = Env::new("bgjobs");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    let pid = env.server_pid().expect("the server runs");
    let jobs = "sleep 4247 & (exec sleep 4248 </dev/null >/dev/null 2>&1) &";
    env.run(&["send", "-p", "shell", "--enter", jobs]);
    let running = |argv: &[u8]| {
        std::fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .any(|e| std::fs::read(e.path().join("cmdline")).is_ok_and(|c| c == argv))
    };
    wait_until(Duration::from_secs(10), "the jobs never started", || {
        running(b"sleep\x004247\x00") && running(b"sleep\x004248\x00")
    });
    let out = env.try_run(&["restart"]);
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "{said}");
    assert!(said.contains("These programs stop:\n  sleep 4247 (in the background)  (pane "), "{said}");
    assert!(!said.contains("4248"), "{said}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("add --yes"));
    assert_eq!(env.server_pid(), Some(pid));
}

/// Run `cmd` in a terminal (`script`, from util-linux), typing `input` into it; what the
/// terminal showed.
#[cfg(target_os = "linux")]
fn in_terminal(cmd: Command, input: &[u8]) -> String {
    use std::io::{Read, Write};
    let line = std::iter::once(cmd.get_program())
        .chain(cmd.get_args())
        .map(|a| format!("'{}'", a.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ");
    let mut c = Command::new("script");
    c.args(["-q", "-f", "-c", &line, "/dev/null"]);
    if let Some(dir) = cmd.get_current_dir() {
        c.current_dir(dir);
    }
    for (k, v) in cmd.get_envs() {
        match v {
            Some(v) => c.env(k, v),
            None => c.env_remove(k),
        };
    }
    c.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
    let mut child = c.spawn().expect("script (util-linux)");
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(input).unwrap();
    let mut out = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut shown = Vec::new();
        let _ = out.read_to_end(&mut shown);
        shown
    });
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        assert!(start.elapsed() < Duration::from_secs(30), "{line} is still running");
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(stdin);
    String::from_utf8_lossy(&reader.join().unwrap()).into_owned()
}

/// install.sh installs a release that matches its checksum, and nothing when it does not.
#[test]
fn install_script_installs_a_checked_release() {
    let env = Env::new("installsh");
    let release = env.root.join("release");
    let asset = fake_release(&release);
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh");
    let dir = env.root.join("bin");
    let install = || {
        Command::new("sh")
            .arg(script)
            .env("KRAAL_DOWNLOAD_URL", format!("file://{}", release.display()))
            .env("KRAAL_INSTALL_DIR", &dir)
            .output()
            .unwrap()
    };
    let done = install();
    assert!(done.status.success(), "{}", String::from_utf8_lossy(&done.stderr));
    assert!(String::from_utf8_lossy(&done.stderr).contains("Installed kraal"));
    assert!(Command::new(dir.join("kraal")).arg("--version").status().unwrap().success());

    std::fs::remove_file(dir.join("kraal")).unwrap();
    std::fs::write(release.join(format!("{asset}.sha256")), format!("{}  {asset}\n", "0".repeat(64))).unwrap();
    let wrong = install();
    assert!(!wrong.status.success());
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("does not match its checksum"));
    assert!(!dir.join("kraal").exists(), "nothing installed");
}

/// A throwaway sshd on 127.0.0.1 with its own host and user keys, and an `ssh` that uses only
/// them (never ~/.ssh): a server reached for real, where the user who logs in has a home of the
/// test's own. `None` where no sshd can run here.
#[cfg(target_os = "linux")]
struct Sshd {
    child: std::process::Child,
    /// Where it listens: a port of its own, so that no test waits for the server of another
    /// one, in this run of the tests or in another on this machine.
    port: u16,
    /// Where that `ssh` is, to put first in PATH.
    bin: PathBuf,
}

#[cfg(target_os = "linux")]
impl Sshd {
    fn start(dir: &Path) -> Option<Sshd> {
        let sshd = ["/usr/sbin/sshd", "/usr/bin/sshd"].into_iter().map(PathBuf::from).find(|p| p.exists())?;
        let ssh =
            ["/usr/bin/ssh", "/bin/ssh", "/usr/local/bin/ssh"].into_iter().map(PathBuf::from).find(|p| p.exists())?;
        std::fs::create_dir_all(dir.join("home")).unwrap();
        for key in ["host_key", "user_key"] {
            let made =
                Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(dir.join(key)).status();
            if !made.is_ok_and(|s| s.success()) {
                return None;
            }
        }
        std::fs::copy(dir.join("user_key.pub"), dir.join("authorized_keys")).unwrap();
        let user = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout).unwrap();
        let d = dir.display();
        let host_key = std::fs::read_to_string(dir.join("host_key.pub")).unwrap();
        std::fs::write(
            dir.join("ssh_config"),
            format!(
                "Host *\n  UserKnownHostsFile {d}/known_hosts\n  GlobalKnownHostsFile /dev/null\n  \
                 StrictHostKeyChecking yes\n  IdentityFile {d}/user_key\n  IdentitiesOnly yes\n  \
                 IdentityAgent none\n  LogLevel ERROR\n"
            ),
        )
        .unwrap();
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        // kraal gives a server a few seconds to answer. This one is on this machine, as slow as
        // the tests beside it make it, and that limit is not what its tests are about: its `ssh`
        // waits as long as they do (of an option given twice, ssh takes the first).
        write_script(
            &bin.join("ssh"),
            &format!(
                "#!/bin/sh\nexec '{}' -F '{d}/ssh_config' -o ConnectTimeout={} \"$@\"\n",
                ssh.display(),
                PATIENCE.as_secs()
            ),
        );
        let log = dir.join("sshd.log");
        loop {
            // A port the system says is free. Another program may take it before sshd does:
            // sshd says so, and another one is asked for.
            let port = std::net::TcpListener::bind(("127.0.0.1", 0)).ok()?.local_addr().ok()?.port();
            std::fs::write(
                dir.join("sshd_config"),
                format!(
                    "Port {port}\nListenAddress 127.0.0.1\nHostKey {d}/host_key\nPidFile {d}/sshd.pid\n\
                     AuthorizedKeysFile {d}/authorized_keys\nStrictModes no\nUsePAM no\nPasswordAuthentication no\n\
                     KbdInteractiveAuthentication no\nPubkeyAuthentication yes\nAllowUsers {}\n\
                     SetEnv HOME={d}/home\nPermitUserRC no\n",
                    user.trim()
                ),
            )
            .unwrap();
            std::fs::write(dir.join("known_hosts"), format!("[127.0.0.1]:{port} {host_key}")).unwrap();
            let _ = std::fs::remove_file(&log);
            let config = dir.join("sshd_config");
            let child = Command::new(&sshd).arg("-D").arg("-f").arg(config).arg("-E").arg(&log).spawn().ok()?;
            let mut sshd = Sshd { child, port, bin: bin.clone() };
            let start = Instant::now();
            let said = || std::fs::read_to_string(&log).unwrap_or_default();
            loop {
                if said().contains("Server listening") {
                    return Some(sshd);
                }
                // What it said last is read once it has ended.
                let ended = sshd.child.try_wait().ok().flatten().is_some();
                if ended && said().contains("Address already in use") {
                    break;
                }
                if ended || start.elapsed() > PATIENCE {
                    eprintln!("sshd did not start: {}", said());
                    return None;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for Sshd {
    fn drop(&mut self) {
        // One that could not start has ended, and its pid may be another program's by now.
        if let Ok(None) = self.child.try_wait() {
            // SAFETY: the pid of the child this test started, not reaped yet.
            unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
            let _ = self.child.wait();
        }
    }
}

/// A file of a server opens in the local editor, each save goes back to the server (its mode
/// kept), a change made on the server meanwhile is never overwritten unasked, and each answer
/// does what it says. The server is a real sshd; the editor is a script told what to do.
#[cfg(target_os = "linux")]
#[test]
fn a_server_file_is_edited_in_the_local_editor() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new("redit");
    let Some(sshd) = Sshd::start(&env.root.join("sshd")) else {
        eprintln!("skipped: no sshd can run here");
        return;
    };
    let user = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout).unwrap();
    let entry = |name: &str| {
        format!(
            "[[server]]\nname = \"{name}\"\nhost = \"127.0.0.1\"\nport = {}\nuser = \"{}\"\n",
            sshd.port,
            user.trim()
        )
    };
    std::fs::write(env.root.join("config/kraal/servers.toml"), format!("{}\n{}", entry("box"), entry("box-prod")))
        .unwrap();
    let downloads = env.root.join("downloads");
    let cfg = env.root.join("config/kraal/config.toml");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str(&format!("\n[servers]\nmultiplex = false\ndownload_dir = \"{}\"\n", downloads.display()));
    std::fs::write(&cfg, text).unwrap();
    // The editor says which file it edits, runs each step it is given (`$f` is the file), and
    // ends when told.
    let root = env.root.display().to_string();
    let editor = env.root.join("editor");
    write_script(
        &editor,
        &format!(
            "#!/bin/sh\nfor f; do :; done\nprintf '%s' \"$f\" > '{root}/editing.tmp' && mv '{root}/editing.tmp' '{root}/editing'\n\
             n=0\nwhile :; do\n  if [ -e '{root}/step'$n ]; then . '{root}/step'$n; n=$((n + 1)); fi\n  \
             if [ -e '{root}/quit' ]; then rm -f '{root}/quit' '{root}/editing' '{root}/step'*; exit 0; fi\n  sleep 0.05\ndone\n"
        ),
    );
    let path = format!("{}:{}", sshd.bin.display(), std::env::var("PATH").unwrap_or_default());
    let kraal =
        |args: &[&str]| env.cmd(args).env("PATH", &path).env("EDITOR", &editor).env_remove("VISUAL").output().unwrap();
    let wait_until = |what: &str, ok: &dyn Fn() -> bool| {
        let start = Instant::now();
        while !ok() {
            assert!(start.elapsed() < PATIENCE, "timed out waiting: {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;
    let step = |n: u32, script: &str| {
        let tmp = env.root.join("step.tmp");
        std::fs::write(&tmp, script).unwrap();
        std::fs::rename(&tmp, env.root.join(format!("step{n}"))).unwrap();
    };
    // Open `target`: the editor's copy.
    let open = |target: &str, extra: &[&str]| -> PathBuf {
        let mut args = vec!["edit"];
        args.extend_from_slice(extra);
        args.push(target);
        let out = kraal(&args);
        assert!(out.status.success(), "kraal edit {target}: {}", String::from_utf8_lossy(&out.stderr));
        let editing = env.root.join("editing");
        wait_until("the editor starts", &|| editing.exists());
        PathBuf::from(read(&editing))
    };
    // End the editor: its private copy goes.
    let quit = |local: &Path| {
        std::fs::write(env.root.join("quit"), "").unwrap();
        let dir = local.parent().unwrap().to_path_buf();
        wait_until("the private copy is removed", &|| !dir.exists());
    };

    env.workspace("w", "layout {\n    pane name=\"sh\"\n}\n");
    let out = kraal(&["open", "w"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mut client = RawClient::attach(&env, "w");
    std::thread::sleep(Duration::from_millis(500));
    let srv = env.root.join("srv").join("it's a \"dir\" $(touch pwned)");
    std::fs::create_dir_all(&srv).unwrap();

    // Each save goes to the server, through a new file renamed in place with the old one's mode.
    let notes = srv.join("notes 'q' $HOME;.conf");
    std::fs::write(&notes, "hello\n").unwrap();
    std::fs::set_permissions(&notes, std::fs::Permissions::from_mode(0o640)).unwrap();
    let local = open(&format!("box:{}", notes.display()), &[]);
    assert!(local.starts_with(env.root.join("run")), "the copy is in the private runtime directory: {local:?}");
    assert_eq!(read(&local), "hello\n");
    assert_eq!(mode(&local), 0o600);
    assert_eq!(mode(local.parent().unwrap()), 0o700);
    step(0, "printf 'added\\n' >> \"$f\"");
    wait_until("the first save reaches the server", &|| read(&notes) == "hello\nadded\n");
    assert_eq!(mode(&notes), 0o640, "the mode is kept");
    client.wait_for_screen("saved to box");
    step(1, "printf 'more\\n' >> \"$f\"");
    wait_until("the second save reaches the server", &|| read(&notes) == "hello\nadded\nmore\n");
    quit(&local);
    let names: Vec<String> =
        std::fs::read_dir(&srv).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(names, vec!["notes 'q' $HOME;.conf".to_string()], "nothing else is left there");
    assert!(!env.root.join("srv/pwned").exists());

    // Changed on the server meanwhile: nothing is sent; kept as a local file, later saves go there.
    let file = srv.join("conflict.txt");
    std::fs::write(&file, "v1\n").unwrap();
    let local = open(&format!("box:{}", file.display()), &[]);
    std::fs::write(&file, "theirs\n").unwrap();
    client.screen.lock().unwrap().clear();
    step(0, "printf 'mine\\n' > \"$f\"");
    client.wait_for_screen("Keep your version as a local file");
    assert_eq!(read(&file), "theirs\n");
    client.send(b"\r");
    let kept = downloads.join("conflict.txt");
    wait_until("the copy is kept", &|| read(&kept) == "mine\n");
    assert_eq!(mode(&kept), 0o600);
    step(1, "printf 'mine2\\n' > \"$f\"");
    wait_until("the next save goes to the kept file", &|| read(&kept) == "mine2\n");
    quit(&local);
    assert_eq!(read(&file), "theirs\n", "the server's change stays");

    // Replaced, once asked.
    let file = srv.join("over.txt");
    std::fs::write(&file, "v1\n").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o604)).unwrap();
    let local = open(&format!("box:{}", file.display()), &[]);
    std::fs::write(&file, "theirs\n").unwrap();
    client.screen.lock().unwrap().clear();
    step(0, "printf 'mine\\n' > \"$f\"");
    client.wait_for_screen("Replace it with your version");
    // Typing does not answer it.
    client.send(b"replace");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(read(&file), "theirs\n");
    for key in [&b"\x1b[B"[..], b"\x1b[B", b"\r"] {
        client.send(key);
        std::thread::sleep(Duration::from_millis(100));
    }
    wait_until("the server's version is replaced", &|| read(&file) == "mine\n");
    assert_eq!(mode(&file), 0o604);
    quit(&local);

    // The server's version kept: the editor's copy becomes it, and nothing is sent.
    let file = srv.join("theirs.txt");
    std::fs::write(&file, "v1\n").unwrap();
    let local = open(&format!("box:{}", file.display()), &[]);
    std::fs::write(&file, "theirs\n").unwrap();
    client.screen.lock().unwrap().clear();
    step(0, "printf 'mine\\n' > \"$f\"");
    client.wait_for_screen("Keep the version of box");
    client.send(b"\x1b[B");
    std::thread::sleep(Duration::from_millis(100));
    client.send(b"\r");
    wait_until("the editor's copy is the server's version", &|| read(&local) == "theirs\n");
    quit(&local);
    assert_eq!(read(&file), "theirs\n");
    assert!(!downloads.join("theirs.txt").exists());

    // Production asks first: from the command line, --production says yes.
    let file = srv.join("prod.txt");
    std::fs::write(&file, "v1\n").unwrap();
    let target = format!("box-prod:{}", file.display());
    let refused = kraal(&["edit", &target]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("production"));
    let local = open(&target, &["--production", "--line", "1"]);
    step(0, "printf 'v2\\n' > \"$f\"");
    wait_until("a production file is saved once allowed", &|| read(&file) == "v2\n");
    quit(&local);

    // From the file manager: `e` on a server's file, asked first on a production server.
    let dir = env.root.join("fm");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("fm.txt");
    std::fs::write(&file, "v1\n").unwrap();
    client.screen.lock().unwrap().clear();
    let out = kraal(&["files", &env.root.display().to_string(), &format!("box-prod:{}", dir.display())]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    client.wait_for_screen("fm.txt");
    for key in [&b"\t"[..], b"\x1b[B", b"e"] {
        client.send(key);
        std::thread::sleep(Duration::from_millis(150));
    }
    client.wait_for_row("It is production");
    client.wait_for_row("Each save in the editor goes to the server.");
    client.wait_for_row("Enter Cancel");
    assert!(!env.root.join("editing").exists(), "nothing opens before the answer");
    client.send(b"y");
    let editing = env.root.join("editing");
    wait_until("the editor starts", &|| editing.exists());
    let local = PathBuf::from(read(&editing));
    step(0, "printf 'v2\\n' > \"$f\"");
    wait_until("a save from the file manager's editor", &|| read(&file) == "v2\n");
    quit(&local);

    // What cannot be edited is said.
    for (target, why) in [
        (format!("box:{}", srv.display()), "directory"),
        (format!("box:{}", env.root.join("nope").display()), "no such file"),
        (env.root.join("editor").display().to_string(), "not on a server"),
    ] {
        let out = kraal(&["edit", &target]);
        assert!(!out.status.success());
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(why), "{target}: {err}");
    }
    drop(sshd);
}

/// `kraal buffer set` with `text` on its standard input.
fn set_buffer(env: &Env, args: &[&str], text: &str) -> Output {
    use std::io::Write;
    let mut child = env
        .cmd(&[&["buffer", "set"], args].concat())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

fn buffer_list(env: &Env) -> Vec<serde_json::Value> {
    let list: serde_json::Value = serde_json::from_str(&env.run(&["buffer", "list", "--json"])).unwrap();
    list.as_array().unwrap().clone()
}

#[test]
fn paste_buffers_from_the_command_line() {
    let env = Env::new("buffers");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    let keys = env.run(&["config", "keys"]);
    assert!(keys.contains("ctrl+b V p, ctrl+b =") && keys.contains("Paste buffers"), "{keys}");
    env.run(&["open", "w"]);
    let out = env.try_run(&["buffer", "show"]);
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("no paste buffers"));
    // Text from the standard input, and from a file under a name.
    assert!(set_buffer(&env, &[], "echo first\n").status.success());
    let file = env.root.join("notes.txt");
    std::fs::write(&file, "line one\nline two\n").unwrap();
    env.run(&["buffer", "set", "notes", "--from-file", &file.display().to_string()]);
    // What a program copies (OSC 52) is kept too, as the newest.
    let copy = "printf '\\033]52;c;%s\\007' \"$(printf copied-by-a-program | base64)\"; exec sleep 60";
    env.run(&["run", "--name", "copier", "--", "sh", "-c", copy]);
    let start = Instant::now();
    while buffer_list(&env).len() < 3 {
        assert!(start.elapsed() < Duration::from_secs(10), "the program's copy was not kept");
        std::thread::sleep(Duration::from_millis(50));
    }
    let list = buffer_list(&env);
    let keys: Vec<&str> = list.iter().map(|b| b["buffer"].as_str().unwrap()).collect();
    assert_eq!(keys, ["0", "notes", "1"], "newest first; numbers count the numbered buffers");
    let notes = (list[1]["named"].as_bool(), list[1]["lines"].as_u64(), list[1]["bytes"].as_u64());
    assert_eq!(notes, (Some(true), Some(2), Some(18)));
    assert_eq!(env.run(&["buffer", "show"]), "copied-by-a-program");
    assert_eq!(env.run(&["buffer", "show", "notes"]), "line one\nline two\n");
    assert_eq!(env.run(&["buffer", "show", "1"]), "echo first\n");
    let shown = env.run(&["buffer", "list"]);
    assert!(shown.contains("notes") && shown.contains("line one | line two"), "{shown}");
    // The same text again is not kept twice, and a number is not a name.
    assert!(set_buffer(&env, &[], "copied-by-a-program").status.success());
    assert_eq!(buffer_list(&env).len(), 3);
    let out = set_buffer(&env, &["7"], "x");
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("is a number"));
    assert!(!env.try_run(&["buffer", "show", "5"]).status.success());

    // A program that asked for bracketed paste gets the markers around it; one that did not
    // gets its line ends as Enter.
    assert!(set_buffer(&env, &["word"], "pasted-word").status.success());
    let show = |mode: &str| format!("stty raw -echo; printf '{mode}READY'; exec cat -v");
    env.run(&["run", "--name", "bracketed", "--", "sh", "-c", &show("\\033[?2004h")]);
    env.run(&["run", "--name", "plain", "--", "sh", "-c", &show("")]);
    env.wait_for_text("bracketed", "READY");
    env.wait_for_text("plain", "READY");
    env.run(&["buffer", "paste", "word", "-p", "bracketed"]);
    env.wait_for_text("bracketed", "^[[200~pasted-word^[[201~");
    env.run(&["buffer", "paste", "1", "-p", "plain"]);
    let screen = env.wait_for_text("plain", "echo first^M");
    assert!(!screen.contains("200~"), "{screen}");

    env.run(&["buffer", "delete", "notes"]);
    assert!(!buffer_list(&env).iter().any(|b| b["buffer"] == "notes"));
    env.run(&["buffer", "delete", "--all"]);
    assert!(buffer_list(&env).is_empty());
}

#[test]
fn paste_buffers_stay_in_memory_unless_kept_on_purpose() {
    let env = Env::new("bufkeep");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let file = env.root.join("state/kraal/buffers").join(format!("{}.json", env.session));
    assert!(set_buffer(&env, &["secret"], "hunter2").status.success());
    env.run(&["kill"]);
    assert!(!file.exists(), "written to disk unasked");
    env.run(&["open", "w"]);
    assert!(buffer_list(&env).is_empty(), "came back after a restart");

    // Kept on purpose: in a file only its owner reads, and back after a restart.
    let path = env.root.join("config/kraal/config.toml");
    let base = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("{base}[buffers]\npersist = true\n")).unwrap();
    env.run(&["config", "reload"]);
    assert!(set_buffer(&env, &["secret"], "hunter2").status.success());
    let start = Instant::now();
    while !file.exists() {
        assert!(start.elapsed() < Duration::from_secs(5), "never written");
        std::thread::sleep(Duration::from_millis(50));
    }
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    env.run(&["kill"]);
    env.run(&["open", "w"]);
    assert_eq!(env.run(&["buffer", "show", "secret"]), "hunter2");
    // Turned off again: the file goes.
    std::fs::write(&path, &base).unwrap();
    env.run(&["config", "reload"]);
    wait_gone(&file);
    assert!(!file.exists());
    assert_eq!(env.run(&["buffer", "show", "secret"]), "hunter2", "still there until the server stops");
}

#[test]
fn paste_buffers_that_could_not_be_kept_are_written_later() {
    let env = Env::new("buffail");
    let config = env.root.join("config/kraal/config.toml");
    let base = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{base}[buffers]\npersist = true\n")).unwrap();
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    // A file where the buffers' directory goes: nothing can be written there.
    let dir = env.root.join("state/kraal/buffers");
    std::fs::write(&dir, "in the way").unwrap();
    assert!(set_buffer(&env, &["notes"], "kept-later").status.success());
    // Long enough for the write to be tried, and to fail.
    std::thread::sleep(Duration::from_millis(1500));
    std::fs::remove_file(&dir).unwrap();
    env.run(&["kill"]);
    env.run(&["open", "w"]);
    assert_eq!(env.run(&["buffer", "show", "notes"]), "kept-later");
    // Still impossible when the server stops: said.
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::write(&dir, "in the way").unwrap();
    assert!(set_buffer(&env, &["notes"], "changed").status.success());
    let out = env.try_run(&["kill"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("the paste buffers could not be saved"), "{err}");
}

#[test]
fn copies_of_the_paste_buffers_a_killed_server_was_writing_go() {
    let env = Env::new("buftemp");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    // What a server killed while it wrote left: temporary copies, named after its pid.
    let mut gone = Command::new("true").spawn().unwrap();
    let dead = gone.id();
    gone.wait().unwrap();
    let (buffers, sessions) = (env.root.join("state/kraal/buffers"), env.root.join("state/kraal/sessions-v2"));
    let temp = |dir: &Path, session: &str| dir.join(format!(".{session}.json.{dead}.0.tmp"));
    for dir in [&buffers, &sessions] {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(temp(dir, "buftemp"), "hunter2").unwrap();
    }
    std::fs::write(temp(&buffers, "other"), "another session's").unwrap();
    // `kraal forget` deletes them with the rest of the session, and nothing of another.
    env.run(&["forget"]);
    assert!(!temp(&buffers, "buftemp").exists() && !temp(&sessions, "buftemp").exists());
    assert!(temp(&buffers, "other").exists());
    // A server that starts deletes all of them, buffers kept or not.
    env.run(&["open", "w"]);
    wait_gone(&temp(&buffers, "other"));
    assert!(!temp(&buffers, "other").exists());
}

#[test]
fn a_paste_buffer_goes_whole_or_not_at_all() {
    let env = Env::new("bufbig");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let got = env.root.join("pasted");
    let script = format!("stty raw -echo; printf READY; exec cat > {}", got.display());
    env.run(&["run", "--name", "sink", "--", "sh", "-c", &script]);
    env.wait_for_text("sink", "READY");
    // Within the buffers' 32 MB, more than a program can be sent at once (16 MiB).
    assert!(set_buffer(&env, &["big"], &"y".repeat(17 << 20)).status.success());
    let out = env.try_run(&["buffer", "paste", "big", "-p", "sink"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("at most"), "{err}");
    // What fits goes.
    assert!(set_buffer(&env, &["small"], "fits").status.success());
    env.run(&["buffer", "paste", "small", "-p", "sink"]);
    let size = || std::fs::metadata(&got).map(|m| m.len()).unwrap_or(0);
    let start = Instant::now();
    while size() < 4 {
        assert!(start.elapsed() < Duration::from_secs(10), "the small paste never arrived");
        std::thread::sleep(Duration::from_millis(50));
    }
    // Chosen in the list: said too.
    env.run(&["focus", "sink"]);
    let mut client = RawClient::attach(&env, "w");
    client.send(b"\x02=");
    client.wait_for_screen("Paste buffers");
    client.send(b"\x1b[B");
    std::thread::sleep(Duration::from_millis(150));
    client.send(b"\r");
    client.wait_for_screen("at most now");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(size(), 4, "nothing of the large paste went");
}

#[test]
fn paste_buffers_reach_production_only_on_purpose() {
    let env = Env::new("bufprod");
    let tf = env.root.join("infra");
    std::fs::create_dir_all(tf.join(".terraform")).unwrap();
    std::fs::write(tf.join(".terraform/environment"), "prod\n").unwrap();
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let script = "stty raw -echo; printf READY; exec cat -v";
    env.run(&["run", "--cwd", &tf.display().to_string(), "--name", "ops", "--", "sh", "-c", script]);
    wait_production(&env, "ops");
    env.wait_for_text("ops", "READY");
    assert!(set_buffer(&env, &[], "rm -rf /tmp/x\nreboot\n").status.success());
    // From the command line: refused unless said.
    let out = env.try_run(&["buffer", "paste", "-p", "ops"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("is production") && err.contains("--production"), "{err}");
    std::thread::sleep(Duration::from_millis(200));
    assert!(!env.run(&["read", "-p", "ops"]).contains("reboot"));
    // From the list: the question a paste of several lines into production gets.
    env.run(&["focus", "ops"]);
    let mut client = RawClient::attach(&env, "w");
    client.send(b"\x02=");
    client.wait_for_screen("Paste buffers");
    client.send(b"\r");
    client.wait_for_screen("Paste 2 lines into production");
    std::thread::sleep(Duration::from_millis(200));
    assert!(!env.run(&["read", "-p", "ops"]).contains("reboot"), "pasted before the answer");
    client.send(b"y");
    env.wait_for_text("ops", "rm -rf /tmp/x^Mreboot^M");
    // On purpose from the command line.
    assert!(set_buffer(&env, &[], "said-so").status.success());
    env.run(&["buffer", "paste", "-p", "ops", "--production"]);
    env.wait_for_text("ops", "said-so");
}

#[test]
fn the_paste_buffers_list_pastes_shows_and_deletes() {
    let env = Env::new("bufpick");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    env.run(&["run", "--name", "src", "--", "sh", "-c", "echo COPY-ME-LINE; exec sleep 60"]);
    env.wait_for_text("src", "COPY-ME-LINE");
    env.run(&["run", "--name", "sink", "--", "sh", "-c", "stty raw -echo; printf READY; exec cat -v"]);
    env.wait_for_text("sink", "READY");
    env.run(&["focus", "src"]);
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_screen("COPY-ME-LINE");
    // Copy mode: up to the line, select it, copy. It goes to the terminal and to the buffers.
    for key in [&b"\x02v"[..], b"k", b"V", b"y"] {
        client.send(key);
        std::thread::sleep(Duration::from_millis(150));
    }
    client.wait_for_screen("\x1b]52;c;");
    assert!(env.run(&["buffer", "show"]).contains("COPY-ME-LINE"));
    assert!(set_buffer(&env, &[], "older-text").status.success());
    assert!(set_buffer(&env, &[], "newest-text").status.success());

    // Enter pastes the newest into the focused pane; typing filters.
    env.run(&["focus", "sink"]);
    client.send(b"\x02=");
    client.wait_for_screen("Paste buffers");
    client.send(b"\r");
    env.wait_for_text("sink", "READYnewest-text");
    client.send(b"\x02V");
    std::thread::sleep(Duration::from_millis(150));
    client.send(b"p");
    client.wait_for_screen("older-text");
    client.send(b"older");
    std::thread::sleep(Duration::from_millis(150));
    client.send(b"\r");
    env.wait_for_text("sink", "newest-textolder-text");

    // Tab lists what else can be done; ctrl+o shows the buffer in a window.
    client.send(b"\x02=");
    std::thread::sleep(Duration::from_millis(150));
    client.send(b"\t");
    client.wait_for_screen("Show in a preview pane");
    // Escape is the key once the server has waited for what may follow it: a key that reached
    // it sooner, on a machine that took its time to read the Escape, would be Alt with that key.
    client.send(b"\x1b");
    client.wait_for_rows("back at the list", |rows| {
        rows.iter().any(|r| r.contains("Paste buffers")) && !rows.iter().any(|r| r.contains("Show in a preview pane"))
    });
    client.send(b"\x0f");
    env.wait_for_text("buffer 0", "newest-text");
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let shown = panes.as_array().unwrap().iter().find(|p| p["name"] == "buffer 0").cloned().unwrap();
    assert_eq!((shown["kind"].as_str(), shown["floating"].as_bool()), (Some("preview:text"), Some(true)));
    // Nothing of it is saved with the session.
    env.run(&["kill"]);
    let state = std::fs::read_to_string(env.state_file()).unwrap();
    assert!(state.contains("\"sink\"") && !state.contains("buffer 0") && !state.contains("newest-text"), "{state}");
}

#[test]
fn the_paste_buffers_list_deletes_and_stays_open() {
    let env = Env::new("bufdel");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    for text in ["first-text", "second-text", "third-text"] {
        assert!(set_buffer(&env, &[], text).status.success());
    }
    let mut client = RawClient::attach(&env, "w");
    client.send(b"\x02=");
    client.wait_for_screen("Paste buffers");
    // ctrl+x deletes the selected buffer (the newest), and the list stays to go on.
    client.send(b"\x18");
    let start = Instant::now();
    while buffer_list(&env).len() != 2 {
        assert!(start.elapsed() < Duration::from_secs(5), "not deleted");
        std::thread::sleep(Duration::from_millis(50));
    }
    client.send(b"\x1b[B");
    std::thread::sleep(Duration::from_millis(150));
    client.send(b"\x18");
    let start = Instant::now();
    while buffer_list(&env).len() != 1 {
        assert!(start.elapsed() < Duration::from_secs(5), "the list did not stay");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(env.run(&["buffer", "show"]), "second-text");
    client.send(b"\x1b");
}

/// A session, a real sshd as the server `box`, and a local editor told what
/// to do, for the tests of editing a server's file. The commands kraal runs there find a `sudo`
/// that takes the password `pass word` (and runs them as this user), and wait 2 seconds once
/// the file `slow` exists.
#[cfg(target_os = "linux")]
struct EditBench {
    env: Env,
    _sshd: Sshd,
    path: String,
    editor: PathBuf,
    srv: PathBuf,
}

#[cfg(target_os = "linux")]
impl EditBench {
    fn new(name: &str) -> Option<EditBench> {
        let env = Env::new(name);
        let sshd = Sshd::start(&env.root.join("sshd"))?;
        let user = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout).unwrap();
        std::fs::write(
            env.root.join("config/kraal/servers.toml"),
            format!(
                "[[server]]\nname = \"box\"\nhost = \"127.0.0.1\"\nport = {}\nuser = \"{}\"\n",
                sshd.port,
                user.trim()
            ),
        )
        .unwrap();
        let cfg = env.root.join("config/kraal/config.toml");
        let mut text = std::fs::read_to_string(&cfg).unwrap();
        let downloads = env.root.join("downloads");
        text.push_str(&format!("\n[servers]\nmultiplex = false\ndownload_dir = \"{}\"\n", downloads.display()));
        std::fs::write(&cfg, text).unwrap();
        let root = env.root.display().to_string();
        let fake = env.root.join("server-bin");
        std::fs::create_dir_all(&fake).unwrap();
        write_script(
            &fake.join("sudo"),
            "#!/bin/sh\nn=\nwhile [ $# -gt 0 ]; do case $1 in -n) n=1 ;; -p) shift ;; --) shift; break ;; -*) ;; *) break ;; esac; shift; done\n\
             if [ -n \"$n\" ]; then echo 'sudo: a password is required' >&2; exit 1; fi\n\
             IFS= read -r pw\nif [ \"$pw\" != 'pass word' ]; then echo 'Sorry, try again.' >&2; exit 1; fi\nexec \"$@\"\n",
        );
        // In front of the test's ssh: what runs there finds that sudo first.
        let bin = env.root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_script(
            &bin.join("ssh"),
            &format!(
                "#!/bin/sh\nif [ -e '{root}/slow' ]; then rm -f '{root}/slow'; sleep 2; fi\nn=$#; i=0\nfor a; do\n  i=$((i + 1))\n  \
                 if [ \"$i\" -eq \"$n\" ]; then set -- \"$@\" \"env PATH={}:/usr/local/bin:/usr/bin:/bin $a\"; else set -- \"$@\" \"$a\"; fi\n\
                 done\nshift \"$n\"\nexec '{}/ssh' \"$@\"\n",
                fake.display(),
                sshd.bin.display()
            ),
        );
        let editor = env.root.join("editor");
        write_script(
            &editor,
            &format!(
                "#!/bin/sh\nfor f; do :; done\nprintf '%s' \"$f\" > '{root}/editing.tmp' && mv '{root}/editing.tmp' '{root}/editing'\n\
                 n=0\nwhile :; do\n  if [ -e '{root}/step'$n ]; then . '{root}/step'$n; n=$((n + 1)); fi\n  \
                 if [ -e '{root}/quit' ]; then rm -f '{root}/quit' '{root}/editing' '{root}/step'*; exit 0; fi\n  sleep 0.05\ndone\n"
            ),
        );
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        let srv = env.root.join("srv");
        std::fs::create_dir_all(&srv).unwrap();
        env.workspace("w", "layout {\n    pane name=\"sh\"\n}\n");
        let bench = EditBench { env, _sshd: sshd, path, editor, srv };
        let out = bench.kraal(&["open", "w"]).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        Some(bench)
    }

    fn kraal(&self, args: &[&str]) -> Command {
        let mut c = self.env.cmd(args);
        c.env("PATH", &self.path).env("EDITOR", &self.editor).env_remove("VISUAL");
        c
    }

    fn wait_until(&self, what: &str, ok: &dyn Fn() -> bool) {
        let start = Instant::now();
        while !ok() {
            assert!(start.elapsed() < PATIENCE, "timed out waiting: {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The editor's copy, once the editor runs.
    fn editing(&self) -> PathBuf {
        let editing = self.env.root.join("editing");
        self.wait_until("the editor starts", &|| editing.exists());
        PathBuf::from(read(&editing))
    }

    /// Have the editor run `script` (`$f` is the file) as its step `n`.
    fn step(&self, n: u32, script: &str) {
        let tmp = self.env.root.join("step.tmp");
        std::fs::write(&tmp, script).unwrap();
        std::fs::rename(&tmp, self.env.root.join(format!("step{n}"))).unwrap();
    }

    /// End the editor: its private copy goes.
    fn quit(&self, local: &Path) {
        std::fs::write(self.env.root.join("quit"), "").unwrap();
        let dir = local.parent().unwrap().to_path_buf();
        self.wait_until("the private copy is removed", &|| !dir.exists());
    }
}

#[cfg(target_os = "linux")]
fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

/// `kraal edit --sudo`: a pane asks for the password, sudo gets it, and each save is written as
/// root, through the same checks; a wrong password opens nothing, and a file one may not write
/// is refused with the way to edit it.
#[cfg(target_os = "linux")]
#[test]
fn a_server_file_is_edited_as_root_through_sudo() {
    use std::os::unix::fs::PermissionsExt;
    let Some(b) = EditBench::new("rsudo") else {
        eprintln!("skipped: no sshd can run here");
        return;
    };
    let mut client = RawClient::attach(&b.env, "w");
    std::thread::sleep(Duration::from_millis(500));
    // Named like the FIFO the password goes through, in the same private directory.
    let file = b.srv.join("sudo");
    std::fs::write(&file, "v1\n").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
    let target = format!("box:{}", file.display());
    let piped = std::process::Stdio::piped;
    // Its pane says so (once it is there).
    let asked = || {
        let out = b.env.try_run(&["read", "-p", "sudo · box"]);
        out.status.success() && String::from_utf8_lossy(&out.stdout).contains("password on box")
    };
    let cli = b.kraal(&["edit", "--sudo", &target]).stdout(piped()).stderr(piped()).spawn().unwrap();
    b.wait_until("the password is asked in its pane", &asked);
    assert!(!b.env.root.join("editing").exists(), "nothing opens before the password");
    client.send(b"pass word\r");
    let local = b.editing();
    let out = cli.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(read(&local), "v1\n");
    b.step(0, "printf 'v2\\n' > \"$f\"");
    b.wait_until("the save reaches the server", &|| read(&file) == "v2\n");
    assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o7777, 0o640, "the mode is kept");
    b.step(1, "printf 'v3\\n' > \"$f\"");
    b.wait_until("the next save, through the same sudo", &|| read(&file) == "v3\n");
    b.quit(&local);
    let names: Vec<String> =
        std::fs::read_dir(&b.srv).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(names, vec!["sudo".to_string()], "no temporary file or lock is left");

    // A wrong password: refused, nothing opens.
    let cli = b.kraal(&["edit", "--sudo", &target]).stdout(piped()).stderr(piped()).spawn().unwrap();
    b.wait_until("the password is asked in its pane", &asked);
    client.send(b"wrong\r");
    let out = cli.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("sudo refused"), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!b.env.root.join("editing").exists());

    // Without --sudo, a file one may not write is refused, saying how to edit it.
    let ro = b.srv.join("ro.conf");
    std::fs::write(&ro, "x\n").unwrap();
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o444)).unwrap();
    let out = b.kraal(&["edit", &format!("box:{}", ro.display())]).output().unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("kraal edit --sudo"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// "Keep the version of the server" while the editor saves again before that version arrived:
/// the new save is not overwritten, and the question comes back.
#[cfg(target_os = "linux")]
#[test]
fn keeping_the_servers_version_never_drops_a_later_save() {
    let Some(b) = EditBench::new("rkeep") else {
        eprintln!("skipped: no sshd can run here");
        return;
    };
    let mut client = RawClient::attach(&b.env, "w");
    std::thread::sleep(Duration::from_millis(500));
    let file = b.srv.join("theirs.txt");
    std::fs::write(&file, "v1\n").unwrap();
    let out = b.kraal(&["edit", &format!("box:{}", file.display())]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let local = b.editing();
    std::fs::write(&file, "theirs\n").unwrap();
    client.screen.lock().unwrap().clear();
    b.step(0, "printf 'mine\\n' > \"$f\"");
    client.wait_for_screen("Keep the version of box");
    // Reading the server's version takes a while; the user saves again meanwhile.
    std::fs::write(b.env.root.join("slow"), "").unwrap();
    client.send(b"\x1b[B");
    std::thread::sleep(Duration::from_millis(100));
    client.screen.lock().unwrap().clear();
    client.send(b"\r");
    b.step(1, "printf 'mine2\\n' > \"$f\"");
    b.wait_until("the second save", &|| read(&local) == "mine2\n");
    // Once the server's version arrived: asked again, the later save still there.
    client.wait_for_screen("Keep your version as a local file");
    assert_eq!(read(&local), "mine2\n", "the later save is still there");
    client.send(b"\r");
    let kept = b.env.root.join("downloads/theirs.txt");
    b.wait_until("the later save is kept", &|| read(&kept) == "mine2\n");
    assert_eq!(read(&file), "theirs\n");
    b.quit(&local);
}

/// Opening the worktree actions of a shell that went into a worktree never waits for git on the
/// server's loop: what git said about the pane's directory is enough.
#[cfg(target_os = "linux")]
#[test]
fn worktree_actions_never_wait_for_git() {
    let env = Env::new("wtslow");
    let repo = env.root.join("app");
    let _git = git_repo(&repo, &[("a.txt", "1\n")]);
    let out = env.cmd(&["wt", "new", "x", "--no-setup"]).current_dir(&repo).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let wt = env.root.join("app.worktrees/x");
    // A git slow to tell a worktree's base, once `slow` exists.
    let real = Command::new("sh").arg("-c").arg("command -v git").output().unwrap();
    let real = String::from_utf8(real.stdout).unwrap();
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let slow = env.root.join("slow");
    write_script(
        &bin.join("git"),
        &format!(
            "#!/bin/sh\ncase \"$*\" in *kraalbase*) if [ -e '{}' ]; then sleep 5; fi ;; esac\nexec '{}' \"$@\"\n",
            slow.display(),
            real.trim()
        ),
    );
    env.workspace("w", &format!("layout {{\n    pane name=\"sh\" cwd={:?}\n}}\n", wt.display().to_string()));
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_screen("kraal/x");
    std::fs::write(&slow, "").unwrap();
    client.send(b"\x02Rg");
    std::thread::sleep(Duration::from_millis(150));
    let start = Instant::now();
    env.run(&["panes"]);
    let waited = start.elapsed();
    std::fs::remove_file(&slow).unwrap();
    assert!(waited < Duration::from_millis(1000), "the server waited {waited:?} for git");
    client.wait_for_screen("Worktree x");
}

/// A pane opened from the command line in a worktree the session has not seen yet gets its
/// variables (number, port offset, port).
#[test]
fn a_pane_opened_in_an_unseen_worktree_gets_its_variables() {
    let env = Env::new("wtvars");
    let repo = env.root.join("app");
    let _git = git_repo(&repo, &[("a.txt", "1\n")]);
    let config = env.root.join("config/kraal/config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("\n[worktrees]\nport_base = 3000\n");
    std::fs::write(&config, text).unwrap();
    for name in ["x", "y"] {
        let out = env.cmd(&["wt", "new", name, "--no-setup"]).current_dir(&repo).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    let wt = env.root.join("app.worktrees/x");
    env.workspace("w", "layout {\n    pane name=\"sh\"\n}\n");
    env.run(&["open", "w"]);
    let probe = "echo \"idx=$KRAAL_WORKTREE_INDEX offset=$KRAAL_PORT_OFFSET port=$PORT\"; exec sleep 60";
    env.run(&["run", "--name", "probe", "--cwd", &wt.display().to_string(), "--", "sh", "-c", probe]);
    env.wait_for_text("probe", "idx=1 offset=10 port=3010");
    // Reached through a symbolic link (as /var is /private/var on macOS), while git gives the
    // worktree's path with its links resolved.
    std::os::unix::fs::symlink(&env.root, env.root.join("link")).unwrap();
    let through = env.root.join("link/app.worktrees/y");
    env.run(&["run", "--name", "linked", "--cwd", &through.display().to_string(), "--", "sh", "-c", probe]);
    env.wait_for_text("linked", "idx=2 offset=20 port=3020");
}

/// The worktrees of a repository are made and removed one at a time, whoever asks at the same
/// moment (several `kraal wt` here, as a script, or the picker beside it, would): git does not
/// keep two `git worktree add` apart, and one that reads the entry another is writing fails.
/// Each worktree still gets a number of its own.
#[test]
fn the_worktrees_of_a_repository_are_changed_one_at_a_time() {
    let env = Env::new("wtserial");
    let repo = env.root.join("app");
    let _git = git_repo(&repo, &[("a.txt", "1\n")]);
    // A git that notes when two change the worktrees at the same moment, and takes long enough
    // over it for the others to come meanwhile.
    let real = Command::new("sh").arg("-c").arg("command -v git").output().unwrap();
    let real = String::from_utf8(real.stdout).unwrap();
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let (busy, overlaps) = (env.root.join("busy"), env.root.join("overlaps"));
    write_script(
        &bin.join("git"),
        &format!(
            "#!/bin/sh\ncase \" $* \" in\n*' worktree add '*|*' worktree remove '*)\n  \
             mkdir '{busy}' 2>/dev/null || echo \"$*\" >> '{overlaps}'\n  sleep 0.1\n  '{real}' \"$@\"\n  \
             status=$?\n  rmdir '{busy}' 2>/dev/null\n  exit $status ;;\nesac\nexec '{real}' \"$@\"\n",
            busy = busy.display(),
            overlaps = overlaps.display(),
            real = real.trim()
        ),
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let names = ["a", "b", "c", "d", "e"];
    let all_at_once = |verb: &str, flag: &str| {
        std::thread::scope(|scope| {
            let (env, path, repo) = (&env, &path, &repo);
            let runs: Vec<_> = names
                .iter()
                .map(|name| {
                    scope.spawn(move || {
                        env.cmd(&["wt", verb, name, flag]).env("PATH", path).current_dir(repo).output().unwrap()
                    })
                })
                .collect();
            for run in runs {
                let out = run.join().unwrap();
                assert!(out.status.success(), "wt {verb}: {}", String::from_utf8_lossy(&out.stderr));
            }
        });
        let overlaps = std::fs::read_to_string(&overlaps).unwrap_or_default();
        assert_eq!(overlaps, "", "wt {verb}: two at the same moment");
    };
    all_at_once("new", "--no-setup");
    let number = |name: &&str| {
        let recorded = std::fs::read_to_string(repo.join(".git/worktrees").join(name).join("kraal-index")).unwrap();
        recorded.trim().parse::<u32>().unwrap()
    };
    let mut numbers: Vec<u32> = names.iter().map(number).collect();
    numbers.sort();
    assert_eq!(numbers, [1, 2, 3, 4, 5]);
    all_at_once("rm", "--force");
    let left = std::fs::read_dir(repo.join(".git/worktrees")).map_or(0, |entries| entries.count());
    assert_eq!(left, 0, "worktrees left");
}

/// `kraal wt prune` removes a branch merged by squash (its commits are not in the base's
/// history, their changes are), keeps one whose ignored files are its own unless told to discard
/// them, and `--no-setup` makes a worktree even when the repository's setup file is broken.
#[test]
fn merged_worktrees_are_pruned_however_they_were_merged() {
    let env = Env::new("wtsquash");
    let repo = env.root.join("app");
    let git = git_repo(&repo, &[(".gitignore", "*.db\n"), ("a.txt", "1\n")]);
    let wt = |args: &[&str]| env.cmd(&[&["wt"], args].concat()).current_dir(&repo).output().unwrap();
    let text =
        |out: &Output| format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let dir = env.root.join("app.worktrees");
    for name in ["sq", "data"] {
        assert!(wt(&["new", name, "--no-setup"]).status.success());
    }
    let in_sq = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir.join("sq"))
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    };
    std::fs::write(dir.join("sq/a.txt"), "2\n").unwrap();
    in_sq(&["commit", "-q", "-am", "two"]);
    std::fs::write(dir.join("sq/b.txt"), "b\n").unwrap();
    in_sq(&["add", "b.txt"]);
    in_sq(&["commit", "-q", "-m", "b"]);
    git(&["merge", "-q", "--squash", "kraal/sq"]);
    git(&["commit", "-q", "-m", "sq (#1)"]);
    std::fs::write(dir.join("data/dev.db"), "rows").unwrap();
    // A broken setup file: the worktree is still made without it.
    std::fs::create_dir_all(repo.join(".kraal")).unwrap();
    std::fs::write(repo.join(".kraal/worktree.toml"), "setup = 1\n").unwrap();
    git(&["add", ".kraal"]);
    git(&["commit", "-q", "-m", "broken"]);
    let out = wt(&["new", "broken"]);
    assert!(!out.status.success() && text(&out).contains(".kraal/worktree.toml"), "{}", text(&out));
    let out = wt(&["new", "broken", "--no-setup"]);
    assert!(out.status.success(), "{}", text(&out));

    let out = wt(&["prune", "--dry-run"]);
    let said = text(&out);
    assert!(said.contains("kraal/sq (its changes are in its base)"), "{said}");
    assert!(said.contains("kept data: it has ignored files not in the main checkout (dev.db)"), "{said}");
    assert!(said.contains("--discard-ignored"), "{said}");
    let out = wt(&["prune"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(!dir.join("sq").exists() && !dir.join("broken").exists());
    assert_eq!(std::fs::read_to_string(dir.join("data/dev.db")).unwrap(), "rows", "its own files are kept");
    let out = wt(&["rm", "data"]);
    assert!(!out.status.success() && text(&out).contains("--discard-ignored"), "{}", text(&out));
    let out = wt(&["prune", "--discard-ignored"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(!dir.join("data").exists());
}

fn click_account_text(client: &mut RawClient, text: &str) {
    let clickable = |row: &str| !row.contains("Enter ") && !row.contains('⏎');
    let rows = client.wait_for_rows(&format!("clickable {text}"), |rows| {
        rows.iter().any(|row| clickable(row) && row.contains(text))
    });
    let (x, y) = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| clickable(r))
        .filter_map(|(y, r)| r.rfind(text).map(|at| (r[..at].chars().count(), y)))
        .max()
        .unwrap();
    client.send(format!("\x1b[<0;{};{}M\x1b[<0;{};{}m", x + 1, y + 1, x + 1, y + 1).as_bytes());
}

#[test]
fn accounts_add_form_finishes_in_the_list_and_actions_keep_live_conversations() {
    for size in [(80, 24), (124, 36)] {
        let env = Env::new(&format!("accounts-form-{}", size.0));
        let bin = env.root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for agent in ["claude", "codex"] {
            write_script(
                &bin.join(agent),
                r#"#!/bin/sh
home="${CLAUDE_CONFIG_DIR:-${CODEX_HOME:-}}"
case "$*" in
    'auth status'|'login status') test -f "$home/signed-in"; exit $? ;;
    'auth login'|'login') echo login >> "$home/logins"; touch "$home/signed-in"; exit 0 ;;
    app-server) exit 1 ;;
esac
echo "READY account=$KRAAL_ACCOUNT"
while IFS= read -r line; do printf 'reply:%s\n' "$line"; done
"#,
            );
        }
        let home = env.root.join("work-home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("signed-in"), "credential").unwrap();
        std::fs::write(home.join("conversation"), "history").unwrap();
        let file = env.root.join("config/kraal/accounts.toml");
        std::fs::write(&file, format!("[[account]]\nname='work'\nagent='codex'\nhome={:?}\n", home.to_str().unwrap()))
            .unwrap();
        env.workspace("w", "layout { pane name=\"agent\" agent=\"codex:work\" }\n");
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        env.wait_for_text("agent", "READY account=codex:work");
        let mut client = RawClient::attach_sized(&env, Some("w"), size).traced();
        if size.0 == 124 {
            // Discovery removes the unsigned standard accounts and moves the heading.
            client.wait_for_rows("startup account discovery completed", |rows| {
                rows.iter().any(|r| r.contains("ACCOUNTS")) && !rows.iter().any(|r| r.contains("Existing "))
            });
            click_account_text(&mut client, "ACCOUNTS");
            client.wait_for_row("Enter Details");
            client.send(b"\x1b");
            client.wait_for_rows("Accounts closed", |rows| !rows.iter().any(|r| r.contains("Enter Details")));
            click_account_text(&mut client, "No usage · 1 pane");
            client.wait_for_row("Enter Back · Tab Actions");
            client.send(b"\r\x1b");
            client.wait_for_rows("Details closed", |rows| {
                !rows.iter().any(|r| r.contains("Enter Back") || r.contains("Enter Details"))
            });
        } else {
            assert!(!client.rows().iter().any(|r| r.contains("ACCOUNTS")));
        }
        client.send(b"\x02An");
        client.wait_for_row("Agent   Codex");
        client.wait_for_row("Name    e.g. Work or Personal");
        client.send(b"\x15work\r");
        client.wait_for_row("already has an account named");
        client.wait_for_row("Name    work");
        client.wait_for_row("Agent   Codex");
        assert!(!home.join("logins").exists());
        client.send(b"\x15second");
        client.wait_for_row("Name    second");
        click_account_text(&mut client, "Sign in");
        client.wait_for_row("Account added");
        client.wait_for_row("Codex · second");
        client.send(b"\r");
        client.wait_for_row("Enter Back · Tab Actions");
        let rows = client.wait_for_row("Usage unavailable");
        assert!(!rows.iter().any(|r| r.contains("0 panes")));
        client.close_overlay("Enter Back · Tab Actions");
        let added: toml::Value = toml::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        let second =
            added["account"].as_array().unwrap().iter().find(|a| a["name"].as_str() == Some("second")).unwrap();
        let second_home = PathBuf::from(second["home"].as_str().unwrap());
        assert_eq!(std::fs::read_to_string(second_home.join("logins")).unwrap(), "login\n");
        assert!(second.get("sign_in_required").is_none());
        client.close_overlay("Enter Details");
        client.send(b"\x02a");
        client.wait_for_row("Start an agent");
        client.send(b"\t");
        let rows = client.wait_for_row("Start in new tab");
        assert!(rows.iter().any(|r| r.contains("Account details")));
        for absent in ["Rename", "Remove", "Start in worktree"] {
            assert!(!rows.iter().any(|r| r.contains(absent)), "{rows:?}");
        }
        if size.0 == 80 {
            assert!(!rows.iter().any(|r| r.contains("Add account")), "{rows:?}");
        }
        client.close_overlay("Start in new tab");
        client.send(b"second");
        client.wait_for_row("1/2");
        click_account_text(&mut client, "Codex · second");
        client.wait_for_row("READY account=codex:second");
        let launched: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        assert_eq!(launched.as_array().unwrap().iter().filter(|p| p["account"] == "codex:second").count(), 1);
        client.send(b"\x02Al");
        client.wait_for_row("Enter Details");
        click_account_text(&mut client, "Codex · second");
        client.wait_for_row("Enter Back · Tab Actions");
        client.send(b"\t");
        click_account_text(&mut client, "Rename");
        client.wait_for_row("Rename Codex · second");
        client.send(b"\x15renamed\r");
        client.wait_for_row("Codex · renamed");
        client.send(b"\r");
        client.wait_for_row("Enter Back · Tab Actions");
        client.send(b"\x1b");
        client.wait_for_row("Enter Details");
        click_account_text(&mut client, "Codex · work");
        client.send(b"\t");
        click_account_text(&mut client, "Remove");
        client.wait_for_row("Conversations and the agent's own sign-in are kept.");
        client.send(b"\r");
        client.wait_for_row("Enter Details");
        client.send(b"\x18");
        client.wait_for_row("Conversations and the agent's own sign-in are kept.");
        client.send(b"y");
        client.wait_for_row("Account removed");
        assert_eq!(std::fs::read_to_string(home.join("signed-in")).unwrap(), "credential");
        assert_eq!(std::fs::read_to_string(home.join("conversation")).unwrap(), "history");
        env.run(&["send", "-p", "agent", "still-here\r"]);
        env.wait_for_text("agent", "reply:still-here");
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        assert_eq!(panes.as_array().unwrap().iter().find(|p| p["name"] == "agent").unwrap()["account"], "codex:work");
    }
}

#[test]
fn the_add_form_names_an_existing_sign_in_without_running_login() {
    let env = Env::new("account-form-existing");
    let path = existing_sign_in_agent(&env, "claude");
    let standard = env.root.join("home/.claude");
    std::fs::create_dir_all(&standard).unwrap();
    std::fs::write(standard.join("status"), "0").unwrap();
    let file = env.root.join("config/kraal/accounts.toml");
    std::fs::write(
        &file,
        format!("[[account]]\nagent='claude'\nname='work'\nhome={:?}\n", env.root.join("work").to_str().unwrap()),
    )
    .unwrap();
    env.workspace("w", "layout { pane name=\"shell\" }\n");
    let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mut client = RawClient::attach_sized(&env, Some("w"), (80, 24)).traced();
    client.send(b"\x02Al");
    wait_for_account_review(&client, "Claude");
    click_account_text(&mut client, "+ Add account");
    let rows = client.wait_for_row("Enter Review");
    assert!(rows.iter().any(|r| r.contains("Existing Claude sign-in needs review.")));
    assert!(!rows.iter().any(|r| r.contains("Name    ")));
    let pending_file = std::fs::read_to_string(&file).unwrap();
    click_account_text(&mut client, "Sign in to another account");
    client.wait_for_row("Name    e.g. Work or Personal");
    client.send(b"\r\r");
    client.wait_for_row("Enter a name for this account.");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), pending_file);
    assert!(!env.root.join("home/logins").exists());
    click_account_text(&mut client, "Cancel");
    client.wait_for_row("+ Add account");
    click_account_text(&mut client, "+ Add account");
    client.wait_for_row("Enter Review");
    client.send(b"\r");
    client.wait_for_row("Which account is this?");
    click_account_text(&mut client, "A different account");
    client.wait_for_row("Name    e.g. Work or Personal");
    let before = std::fs::read_to_string(&file).unwrap();
    let checks = || std::fs::read_to_string(env.root.join("home/status-checks")).unwrap_or_default().lines().count();
    let finished = || std::fs::read_to_string(standard.join("check-finished")).unwrap_or_default().lines().count();
    let (before_checks, before_finished) = (checks(), finished());
    std::fs::write(standard.join("hold-check"), "").unwrap();
    client.send(b"\x15cancelled\r\r");
    client.wait_for_row("Checking sign-in...");
    common::wait_until(PATIENCE, "one check started", || checks() == before_checks + 1);
    click_account_text(&mut client, "Cancel");
    client.wait_for_row("+ Add account");
    std::fs::remove_file(standard.join("hold-check")).unwrap();
    common::wait_until(PATIENCE, "cancelled check finished", || finished() == before_finished + 1);
    assert_eq!(checks(), before_checks + 1, "Enter while checking did not start another check");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
    click_account_text(&mut client, "+ Add account");
    client.wait_for_row("Enter Review");
    click_account_text(&mut client, "Use existing account");
    client.wait_for_row("Which account is this?");
    click_account_text(&mut client, "A different account");
    client.wait_for_row("Name    e.g. Work or Personal");
    client.send(b"\x15personal\r");
    client.wait_for_row("Account added");
    client.wait_for_row("Claude · personal");
    assert!(!env.root.join("home/logins").exists());
    let accounts: toml::Value = toml::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
    let a = accounts["account"].as_array().unwrap().iter().find(|a| a["name"].as_str() == Some("personal")).unwrap();
    assert_eq!(a["home"].as_str().unwrap(), "~/.claude");
    assert!(a.get("review").is_none());
}

#[test]
fn a_different_account_review_after_sign_out_exposes_sign_in() {
    let env = Env::new("account-review-signed-out");
    let path = existing_sign_in_agent(&env, "claude");
    let standard = env.root.join("home/.claude");
    std::fs::create_dir_all(&standard).unwrap();
    std::fs::write(standard.join("status"), "0").unwrap();
    let file = env.root.join("config/kraal/accounts.toml");
    std::fs::write(
        &file,
        format!("[[account]]\nagent='claude'\nname='work'\nhome={:?}\n", env.root.join("work").to_str().unwrap()),
    )
    .unwrap();
    env.workspace("w", "layout { pane name=\"shell\" }\n");
    let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mut client = RawClient::attach_sized(&env, Some("w"), (80, 24));
    client.send(b"\x02Al");
    client.wait_for_row("Existing Claude account");
    client.wait_for_row("Review");
    assert!(std::fs::read_to_string(&file).unwrap().contains("review = true"));
    // Discovery succeeded, but the agent signs out before the user resolves its review.
    std::fs::write(standard.join("status"), "1").unwrap();
    click_account_text(&mut client, "Existing Claude account");
    client.wait_for_row("Which account is this?");
    click_account_text(&mut client, "A different account");
    client.wait_for_row("Name    e.g. Work or Personal");
    client.send(b"personal\r");
    client.wait_for_row("Account added");
    client.wait_for_row("Claude · personal");
    client.wait_for_row("Sign-in required");
    assert!(!env.root.join("home/logins").exists());
    let accounts: toml::Value = toml::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let accounts = accounts["account"].as_array().unwrap();
    assert_eq!(accounts.len(), 2);
    let a = accounts.iter().find(|a| a["name"].as_str() == Some("personal")).unwrap();
    assert_eq!(a["home"].as_str().unwrap(), "~/.claude");
    assert_eq!(a["sign_in_required"].as_bool(), Some(true));
    assert!(a.get("review").is_none());
    client.send(b"\r");
    client.wait_for_row("Enter Back · Tab Actions");
    client.send(b"\t");
    client.wait_for_row("Sign in");
    click_account_text(&mut client, "Sign in");
    common::wait_until(PATIENCE, "the account's Sign in action starts login", || env.root.join("home/logins").exists());
    assert_eq!(std::fs::read_to_string(env.root.join("home/logins")).unwrap(), "unexpected\n");
}

#[test]
fn sidebar_accounts_discover_a_sign_in_completed_after_startup() {
    for entry in ["ACCOUNTS", "+ Add account"] {
        let env = Env::new(if entry == "ACCOUNTS" { "sidebar-accounts-discovery" } else { "sidebar-add-discovery" });
        let path = existing_sign_in_agent(&env, "claude");
        let standard = env.root.join("home/.claude");
        std::fs::create_dir_all(&standard).unwrap();
        std::fs::write(standard.join("status"), "1").unwrap();
        let file = env.root.join("config/kraal/accounts.toml");
        std::fs::write(
            &file,
            format!("[[account]]\nagent='claude'\nname='work'\nhome={:?}\n", env.root.join("work").to_str().unwrap()),
        )
        .unwrap();
        env.workspace("w", "layout { pane name=\"shell\" }\n");
        let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let mut client = RawClient::attach_sized(&env, Some("w"), (124, 36)).traced();
        common::wait_until(PATIENCE, "startup checked the unsigned home", || standard.join("check-finished").exists());
        client.wait_for_rows("startup discovery completed", |rows| {
            rows.iter().any(|r| r.contains("ACCOUNTS")) && !rows.iter().any(|r| r.contains("Checking sign-in"))
        });
        assert!(!std::fs::read_to_string(&file).unwrap().contains("review = true"));

        // The agent signs in outside kraal after the server cached its unsigned status.
        std::fs::write(standard.join("status"), "0").unwrap();
        click_account_text(&mut client, entry);
        if entry == "ACCOUNTS" {
            wait_for_account_review(&client, "Claude");
            click_account_text(&mut client, "+ Add account");
        }
        client.wait_for_row("Use existing account");
        assert!(std::fs::read_to_string(&file).unwrap().contains("review = true"));
        assert!(!env.root.join("home/logins").exists());
    }
}

#[test]
fn importing_a_file_previews_then_keeps_commands_waiting_and_export_preserves_the_source() {
    let env = Env::new("layout-transfer");
    let path = env.root.join("recipe.kdl");
    let marker = env.root.join("must-not-run");
    let source = format!(
        "// handwritten\nlayout {{ pane command=\"touch\" {{ args {:?}; }}; unknown; }}\n",
        marker.display().to_string()
    );
    std::fs::write(&path, &source).unwrap();
    let file = path.to_str().unwrap();
    let refused = env.try_run(&["import", file]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stdout).contains("Commands wait for you to start them"));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("No workspace was added"));
    assert!(!marker.exists());
    let added = env.run(&["import", file, "--name", "personal", "--yes"]);
    assert!(added.contains("unknown") && added.contains("Workspace \"personal\" added"), "{added}");
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    assert!(panes.as_array().unwrap().iter().filter(|p| p["workspace"] == "personal").all(|p| p["suspended"] == true));
    assert!(!marker.exists());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
    let again = env.try_run(&["import", file, "--name", "duplicate", "--yes"]);
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("already used by personal"));
    let exported = env.root.join("config/kraal/workspaces/shared.kdl");
    assert!(env.run(&["export", exported.to_str().unwrap(), "--workspace", "personal"]).contains("Layout exported"));
    assert!(!listed_workspaces(&env).iter().any(|w| w.starts_with("shared:")));
    let refused = env.try_run(&["export", file, "--workspace", "personal"]);
    assert!(!refused.status.success());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
    env.run(&["export", file, "--workspace", "personal", "--replace"]);
    assert_eq!(std::fs::read_to_string(path.with_extension("kdl.bak")).unwrap(), source);
    env.run(&["kill"]);
    env.run(&["open", "personal"]);
    assert!(!marker.exists());
    assert!(!listed_workspaces(&env).iter().any(|w| w.starts_with("shared:")));
    let saved = env.try_run(&["save", "legacy-copy", "--workspace", "personal"]);
    assert!(saved.status.success());
    assert!(String::from_utf8_lossy(&saved.stderr).contains("Workspaces are saved automatically"));
    assert!(String::from_utf8_lossy(&saved.stdout).trim_end().ends_with("legacy-copy.kdl"));
}

#[test]
fn workspace_cli_quotes_names_and_shortens_home_paths_in_rendered_output() {
    let env = Env::new("workspace-messages");
    let path = env.root.join("home/recipe.kdl");
    std::fs::write(&path, "layout { pane command=\"true\"; }").unwrap();
    let imported = env.run(&["import", path.to_str().unwrap(), "--name", "imported", "--yes"]);
    assert!(imported.lines().any(|line| line == "~/recipe.kdl"), "{imported}");
    assert!(imported.contains("Workspace \"imported\" added."), "{imported}");
    let exported = env.run(&["export", "~/shared.kdl", "--workspace", "imported"]);
    assert_eq!(exported.trim(), "Layout exported\n~/shared.kdl");
    let opened = env.run(&["open", "imported"]);
    assert!(opened.contains("workspace \"imported\" is open"), "{opened}");
}

#[test]
fn importing_a_bare_filename_uses_the_callers_directory() {
    let env = Env::new("import-relative");
    let project = env.root.join("project");
    std::fs::create_dir(&project).unwrap();
    let source = "layout { pane command=\"true\"; }";
    let other = "layout { pane command=\"false\"; }";
    std::fs::write(project.join("layout.kdl"), source).unwrap();
    std::fs::write(env.root.join("home/layout.kdl"), other).unwrap();
    let out = env.cmd(&["import", "layout.kdl", "--yes"]).current_dir(&project).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("Workspace \"layout\" added"));
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let pane = panes.as_array().unwrap().iter().find(|p| p["workspace"] == "layout").unwrap();
    assert_eq!(pane["command"], serde_json::json!(["true"]));
    assert_eq!(pane["suspended"], true);
    let workspaces: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    let workspace = workspaces.as_array().unwrap().iter().find(|w| w["name"] == "layout").unwrap();
    assert_eq!(workspace["definition"], std::fs::canonicalize(project.join("layout.kdl")).unwrap().to_str().unwrap());
    assert_eq!(std::fs::read_to_string(project.join("layout.kdl")).unwrap(), source);
    assert_eq!(std::fs::read_to_string(env.root.join("home/layout.kdl")).unwrap(), other);
}

#[test]
fn from_file_and_export_use_the_shared_forms_at_both_sizes() {
    for size in [(80, 24), (124, 36)] {
        let env = Env::new(&format!("file-forms-{}", size.0));
        env.workspace("base", "layout { pane; }");
        env.run(&["open", "base"]);
        let path = env.root.join("imported.kdl");
        let original = "// retain this\nlayout { pane command=\"false\"; }\n";
        std::fs::write(&path, original).unwrap();
        let mut client = RawClient::attach_sized(&env, Some("base"), size);
        client.wait_for_message("\"attached\"");
        client.send(b"\x02Wn");
        client.wait_for_row("From file...");
        click_account_text(&mut client, "From file...");
        client.send(format!("{}\t", path.display()).as_bytes());
        client.wait_for_row("Commands wait for you to start them.");
        client.send(b"\r");
        client.wait_for_row("Workspace added");
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let imported = panes.as_array().unwrap().iter().find(|p| p["workspace"] == "imported").unwrap();
        assert_eq!(imported["suspended"], true);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        client.send(b"\x02Ws");
        client.wait_for_row("Workspaces are saved automatically.");
        let copy = env.root.join("copy.kdl");
        client.send(format!("\x01\x0b{}\r", copy.display()).as_bytes());
        client.wait_for_row("Layout exported");
        assert!(std::fs::read_to_string(copy).unwrap().contains("command=\"false\""));
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }
}

#[test]
fn legacy_zellij_import_keeps_manual_bytes_and_backs_up_replacements() {
    let env = Env::new("import-zellij-backup");
    let from = env.root.join("layouts");
    std::fs::create_dir(&from).unwrap();
    let source = "// keep all comments\nlayout { pane; unknown; }\n";
    std::fs::write(from.join("manual.kdl"), source).unwrap();
    env.workspace("manual", "layout { pane name=\"before\"; }");
    let dest = env.root.join("config/kraal/workspaces/manual.kdl");
    let old = std::fs::read_to_string(&dest).unwrap();
    env.run(&["import-zellij", "--from", from.to_str().unwrap()]);
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), old);
    env.run(&["import-zellij", "--from", from.to_str().unwrap(), "--force"]);
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), source);
    assert_eq!(std::fs::read_to_string(dest.with_extension("kdl.bak")).unwrap(), old);
    env.run(&["open", "manual"]);
    std::fs::write(from.join("manual.kdl"), "layout { pane name=\"replacement\"; }").unwrap();
    let skipped = env.try_run(&["import-zellij", "--from", from.to_str().unwrap(), "--force"]);
    assert!(skipped.status.success());
    assert!(String::from_utf8_lossy(&skipped.stderr).contains("belongs to workspace"));
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), source);
    env.run(&["kill"]);
    let skipped = env.try_run(&["import-zellij", "--from", from.to_str().unwrap(), "--force"]);
    assert!(skipped.status.success());
    assert!(String::from_utf8_lossy(&skipped.stderr).contains("belongs to workspace"));
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), source);
    assert_eq!(std::fs::read_to_string(dest.with_extension("kdl.bak")).unwrap(), old);
    assert!(env.server_pid().is_none());
}

#[test]
fn servers_add_saves_before_connecting_and_a_failed_connection_keeps_the_entry() {
    for size in [(80, 24), (124, 36)] {
        let env = Env::new(&format!("server-form-{}", size.0));
        let bin = env.root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let log = env.root.join("ssh.log");
        write_script(
            &bin.join("ssh"),
            &format!("#!/bin/sh\necho attempt >> '{}'\necho 'Connection refused' >&2\nexit 255\n", log.display()),
        );
        env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        let out = env.cmd(&["open", "w"]).env("PATH", path).output().unwrap();
        assert!(out.status.success());
        let mut client = RawClient::attach_sized(&env, Some("w"), size);
        client.send(b"\x02s");
        click_account_text(&mut client, "+ Add server");
        client.wait_for_row("Address");
        client.send(b"admin@down.lan\r");
        client.wait_for_row("Server added");
        assert!(!log.exists(), "saving never invokes SSH");
        let inventory = env.root.join("config/kraal/servers.toml");
        let saved = std::fs::read_to_string(&inventory).unwrap();
        assert!(saved.contains("name = \"down\""));
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        assert_eq!(panes.as_array().unwrap().len(), 1);
        client.send(b"\r");
        client.wait_for_row("The server is saved.");
        assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);
        click_account_text(&mut client, "Try again");
        common::wait_until(PATIENCE, "second SSH attempt", || {
            std::fs::read_to_string(&log).unwrap().lines().count() == 2
        });
        client.wait_for_row("The server is saved.");
        client.send(b"\r");
        common::wait_until(PATIENCE, "failed SSH pane closed", || {
            let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
            panes.as_array().unwrap().len() == 1
        });
        assert_eq!(std::fs::read_to_string(&inventory).unwrap(), saved);
        assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 2, "Enter defaults to Close");
    }
}

#[test]
fn saving_beside_a_watched_server_does_not_start_another_ssh() {
    let env = Env::new("server-form-watched");
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = env.root.join("ssh.log");
    write_script(
        &bin.join("ssh"),
        &format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexec sleep 60\n", log.display()),
    );
    let inventory = env.root.join("config/kraal/servers.toml");
    let original = "[[server]]\nname = \"box\"\nhost = \"box.lan\"\nagents = \"main\"\n";
    std::fs::write(&inventory, original).unwrap();
    env.workspace("w", "layout { pane; }\n");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let out = env.cmd(&["open", "w"]).env("PATH", path).output().unwrap();
    assert!(out.status.success());
    common::wait_until(PATIENCE, "the saved server's agent watch started", || {
        std::fs::read_to_string(&log).is_ok_and(|text| text.lines().count() == 1)
    });
    let before = std::fs::read_to_string(&log).unwrap();
    assert_eq!(before.lines().count(), 1);
    let mut client = RawClient::attach_sized(&env, Some("w"), (80, 24));
    client.send(b"\x02Sn");
    client.wait_for_row("Address");
    client.send(b"box.lan:22\t\x15box22\r");
    client.wait_for_row("Server added");
    client.settle();
    assert_eq!(std::fs::read_to_string(&log).unwrap(), before, "saving must not start another SSH");
    let saved = std::fs::read_to_string(&inventory).unwrap();
    assert!(saved.starts_with(original));
    let parsed: toml::Table = toml::from_str(&saved).unwrap();
    let entries = parsed["server"].as_array().unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[1]["name"].as_str(), Some("box22"));
    assert_eq!(entries[1]["host"].as_str(), Some("box.lan"));
    assert_eq!(entries[1]["port"].as_integer(), Some(22));
    assert!(entries[1].get("agents").is_none());
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    assert_eq!(panes.as_array().unwrap().len(), 1);
}

#[test]
fn server_form_save_error_retries_without_losing_fields_and_concurrent_adds_do_not_duplicate() {
    let env = Env::new("server-form-retry");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let inventory = env.root.join("config/kraal/servers.toml");
    let broken = "# Keep this file\n[[server]\n";
    std::fs::write(&inventory, broken).unwrap();
    let mut client = RawClient::attach_sized(&env, Some("w"), (80, 24));
    client.send(b"\x02Sn");
    client.wait_for_row("Address");
    client.send(b"admin@db.lan:2222\t\x15database\r");
    client.wait_for_row("Couldn't save server.");
    client.wait_for_row("admin@db.lan:2222");
    client.wait_for_row("database");
    assert_eq!(std::fs::read_to_string(&inventory).unwrap(), broken);
    let original = "# Keep this file\n[[server]]\nname = \"web\"\nhost = \"web.lan\"\ncustom = \"kept\"\n";
    std::fs::write(&inventory, original).unwrap();
    let mut other = RawClient::attach_sized(&env, Some("w"), (124, 36));
    other.send(b"\x02Sn");
    other.wait_for_row("Address");
    other.send(b"admin@db.lan:2222\t\x15database");
    client.send(b"\r");
    other.send(b"\r");
    for client in [&mut client, &mut other] {
        client.wait_for_rows("save result", |rows| {
            rows.iter().any(|r| r.contains("Server added") || r.contains("Server already saved"))
        });
    }
    let saved = std::fs::read_to_string(&inventory).unwrap();
    assert!(saved.starts_with(original));
    assert_eq!(saved.matches("name = \"database\"").count(), 1);
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    assert_eq!(panes.as_array().unwrap().len(), 1);
    client.send(b"\x15nothing-matches");
    client.wait_for_row("+ Add server");
    client.send(b"\r");
    client.wait_for_row("Address   nothing-matches");
    client.send(b"\x1b");
    client.wait_for_row("nothing-matches");
    assert_eq!(std::fs::read_to_string(&inventory).unwrap(), saved, "cancel does not save");
}

#[test]
fn file_review_rechecks_changes_stops_only_on_confirmation_and_undo_keeps_the_local_layout() {
    for size in [(80, 24), (124, 36)] {
        let env = Env::new(&format!("source-review-{}", size.0));
        env.workspace(
            "source",
            "layout { pane name=\"original\" command=\"sh\" { args \"-c\" \"echo running; exec sleep 60\"; }; }",
        );
        env.run(&["open", "source"]);
        env.wait_for_text("original", "running");
        let path = env.root.join("config/kraal/workspaces/source.kdl");
        let marker = env.root.join("must-not-run");
        let mut client = RawClient::attach_sized(&env, Some("source"), size);
        client.wait_for_message("\"attached\"");
        std::fs::write(&path, "layout { pane command=\"false\"; }").unwrap();
        client.send(b"\x02w");
        client.wait_for_row("File changed");
        client.send(b"\t");
        click_account_text(&mut client, "Review");
        client.wait_for_row("Saved workspace:");
        let changed = format!(
            "layout {{ pane command=\"touch\" {{ args \"{}\"; }}; pane command=\"false\"; }}",
            marker.display()
        );
        std::fs::write(&path, &changed).unwrap();
        click_account_text(&mut client, "Load from file");
        client.wait_for_row("File changed again");
        let before: Vec<serde_json::Value> = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        assert_eq!(before.len(), 1);
        assert_eq!(before[0]["name"], "original");
        click_account_text(&mut client, "Load from file");
        client.wait_for_row("Stop and load");
        client.send(b"n");
        client.wait_for_row("Saved workspace:");
        let cancelled: Vec<serde_json::Value> = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        assert_eq!(cancelled.len(), 1);
        assert_eq!(cancelled[0]["id"], before[0]["id"]);
        click_account_text(&mut client, "Load from file");
        client.wait_for_row("Stop and load");
        client.send(b"y");
        client.wait_for_row("Layout loaded");
        let panes: Vec<serde_json::Value> = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        assert_eq!(panes.len(), 2);
        assert!(panes.iter().all(|p| p["suspended"] == true));
        assert!(!marker.exists());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), changed);
        client.send(b"\t");
        click_account_text(&mut client, "Undo load");
        client.wait_for_row("Layout restored");
        let workspaces: Vec<serde_json::Value> = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0]["open"], false);
        assert_eq!(workspaces[0]["panes"], 1);
        env.run(&["kill"]);
        drop(client);
        std::fs::remove_file(&path).unwrap();
        env.run(&["open", "source"]);
        let panes: Vec<serde_json::Value> = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0]["name"], "original");
        assert_eq!(panes[0]["suspended"], true);
        assert!(!path.exists());
        assert!(!marker.exists());
    }
}

#[test]
fn tunnel_form_shows_the_port_and_live_results_can_retry_and_stop() {
    for size in [(80, 24), (124, 36)] {
        let env = Env::new(&format!("tunnel-form-{}", size.0));
        let bin = env.root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_script(
            &bin.join("ssh"),
            "#!/bin/sh\n\
             prev=; for a; do [ \"$prev\" = -L ] && fwd=$a; prev=$a; done\n\
             echo started >> \"$HOME/tunnel-starts\"\n\
             while [ ! -f \"$HOME/tunnel-release\" ]; do sleep 0.05; done\n\
             if [ ! -f \"$HOME/tunnel-ready\" ]; then echo 'Cannot reach db' >&2; exit 255; fi\n\
             port=${fwd#127.0.0.1:}; port=${port%%:*}\n\
             echo \"debug1: Local forwarding listening on 127.0.0.1 port $port.\" >&2\n\
             echo 'debug1: channel 0: new port-listener [port listener]' >&2\n\
             exec sleep 600\n",
        );
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        let config = env.root.join("config/kraal/config.toml");
        let text = std::fs::read_to_string(&config).unwrap();
        std::fs::write(&config, format!("{text}[sidebar]\nvisible = false\n")).unwrap();
        std::fs::write(env.root.join("config/kraal/servers.toml"), "[[server]]\nname = \"db\"\nhost = \"db.lan\"\n")
            .unwrap();
        env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
        let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let mut client = RawClient::attach_sized(&env, Some("w"), size);
        client.send(b"\x02So");
        client.wait_for_row("+ Add tunnel");
        click_account_text(&mut client, "+ Add tunnel");
        client.wait_for_row("Server    Name or user@host");
        client.close_overlay("Server    Name or user@host");
        client.close_overlay("+ Add tunnel");
        client.send(b"\x02sdb\x0f");
        client.wait_for_row("Server    db");
        let busy = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = busy.local_addr().unwrap().port();
        client.send(format!("localhost:{port}").as_bytes());
        client.wait_for_row(&format!("Local     {port}"));
        client.send(b"\r");
        client.wait_for_row("used by another program");
        let rows = client.wait_for_row("Use ");
        let suggested: u16 = rows
            .iter()
            .filter_map(|r| r.split("Use ").nth(1))
            .find_map(|s| s.chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok())
            .unwrap();
        assert_ne!(suggested, port);
        assert_eq!(env.run(&["tunnel", "list"]).trim(), "no tunnels");
        click_account_text(&mut client, &format!("Use {suggested}"));
        client.wait_for_row(&format!("Local     {suggested}"));
        assert_eq!(env.run(&["tunnel", "list"]).trim(), "no tunnels");
        click_account_text(&mut client, " Add ");
        client.wait_for_row("Connecting");
        assert!(env.run(&["tunnel", "list"]).contains(&format!("localhost:{suggested}")));
        client.send(b"\r");
        client.wait_for_row("Tunnel · db");
        client.wait_for_row("Enter Close");
        std::fs::write(env.root.join("home/tunnel-release"), "").unwrap();
        client.wait_for_row("Cannot reach db");
        client.wait_for_row("Retrying in ");
        std::fs::write(env.root.join("home/tunnel-ready"), "").unwrap();
        click_account_text(&mut client, " Retry now ");
        client.wait_for_row("Ready");
        assert!(env.run(&["tunnel", "list"]).starts_with("up"));
        client.close_overlay("Tunnel · db");
        client.wait_for_row("Ready");
        client.send(b"\t");
        client.wait_for_row("Stop");
        click_account_text(&mut client, "Stop");
        client.wait_for_rows("the stopped tunnel left the list", |rows| {
            rows.iter().any(|r| r.contains("+ Add tunnel")) && !rows.iter().any(|r| r.contains("Ready"))
        });
        assert_eq!(env.run(&["tunnel", "list"]).trim(), "no tunnels");
        let workspaces: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
        assert!(
            workspaces.as_array().unwrap().iter().any(|w| w["name"] == "w" && w["open"] == true),
            "stopping the tunnel keeps the workspace open"
        );
    }
}

#[test]
fn workspace_commands_keep_identity_sources_and_recover_deletions_after_restart() {
    let env = Env::new("workspace-lifecycle-cli");
    let source = "// Keep this recipe.\nlayout { pane command=\"sleep 60\" start_suspended=true; }\n";
    env.workspace("api", source);
    env.run(&["open", "api"]);
    let list = || serde_json::from_str::<serde_json::Value>(&env.run(&["workspaces", "--json"])).unwrap();
    let id = list()[0]["id"].as_str().unwrap().to_string();
    env.run(&["workspaces", "rename", &id, "backend"]);
    assert_eq!(list()[0]["id"], id);
    assert_eq!(list()[0]["name"], "backend");
    let path = env.root.join("config/kraal/workspaces/api.kdl");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
    assert!(!path.with_extension("kdl.bak").exists());
    env.run(&["workspaces", "close", &id]);
    assert_eq!(list()[0]["open"], false);
    assert!(env.run(&["panes", "--json"]).trim() == "[]");
    let denied = env.try_run(&["workspaces", "delete", &id]);
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("--yes"));
    assert_eq!(list()[0]["id"], id);
    env.run(&["workspaces", "delete", &id, "--yes"]);
    assert_eq!(list(), serde_json::json!([]));
    env.run(&["kill"]);
    let deleted = env.run(&["workspaces", "--deleted", "--json"]);
    let deleted: serde_json::Value = serde_json::from_str(&deleted).unwrap();
    assert_eq!(deleted[0]["id"], id);
    env.run(&["workspaces", "restore", &id]);
    assert_eq!(list()[0]["id"], id);
    assert_eq!(list()[0]["open"], false);
    assert!(env.run(&["panes", "--json"]).trim() == "[]");
    assert_eq!(std::fs::read_to_string(path).unwrap(), source);
}

#[test]
fn workspace_commands_create_refuse_collisions_and_require_approval_to_stop_programs() {
    let env = Env::new("workspace-create-cli");
    let out = env.cmd(&["workspaces", "create", "api"]).current_dir(&env.root).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!env.try_run(&["workspaces", "create", "api"]).status.success());
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    assert_eq!(panes.as_array().unwrap().len(), 1);
    assert_eq!(Path::new(panes[0]["cwd"].as_str().unwrap()).canonicalize().unwrap(), env.root.canonicalize().unwrap());
    env.run(&["run", "--name", "busy", "sh", "-c", "echo workspace-ready; sleep 60"]);
    env.wait_for_text("busy", "workspace-ready");
    let denied = env.try_run(&["workspaces", "close", "api"]);
    assert!(!denied.status.success());
    let preview = String::from_utf8_lossy(&denied.stdout);
    assert!(preview.contains("These programs will stop:"), "{preview}");
    assert!(preview.contains("sleep 60"), "{preview}");
    assert!(String::from_utf8_lossy(&denied.stderr).contains("--yes"));
    assert!(env.run(&["panes", "--json"]).contains("busy"));
    env.run(&["workspaces", "close", "api", "--yes"]);
    env.run(&["workspaces", "delete", "api", "--yes"]);
    env.run(&["workspaces", "create", "api"]);
    assert!(!env.try_run(&["workspaces", "restore", "api"]).status.success());
    env.run(&["workspaces", "restore", "api", "--name", "recovered"]);
    let list: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    let recovered = list.as_array().unwrap().iter().find(|w| w["name"] == "recovered").unwrap();
    assert_eq!(recovered["open"], false);
    assert_eq!(recovered["panes"], 2);
}

#[cfg(unix)]
#[test]
fn palette_attachment_transfers_a_complete_batch_over_host_attach_and_keeps_it_after_detach() {
    use std::os::unix::fs::PermissionsExt;

    let local = Env::new("attachment-local");
    let remote = Env::new("attachment-host");
    remote.workspace(
        "w",
        "layout {\n    pane name=\"main\" command=\"sh\" { args \"-c\" \"stty -echo; echo READY; cat\"; }\n}\n",
    );
    remote.run(&["open", "w"]);
    remote.wait_for_text("main", "READY");
    let other = RawClient::attach(&remote, "w");
    let (_, path) = fake_ssh(&local, &remote);
    std::fs::write(local.root.join("config/kraal/servers.toml"), "[[server]]\nname='box'\nhost='attachment.invalid'\n")
        .unwrap();
    let first = local.root.join("notes client.txt");
    let second = local.root.join("capture.bin");
    std::fs::write(&first, b"notes from the laptop").unwrap();
    let content = vec![0x91; 2 * 64 * 1024 + 7];
    std::fs::write(&second, &content).unwrap();
    let mut command = local.isolated(Path::new(env!("CARGO_BIN_EXE_kraal")));
    command.args(["-s", &remote.session, "attach", "--host", "box", "w"]).env("PATH", path);
    let mut host = TerminalClient::start(command);
    host.wait_for("READY");
    host.type_keys(b"\x02 Attach files\r");
    host.wait_for("Enter sends, Ctrl+C cancels");
    host.type_keys(format!("'{}' {}\r", first.display(), second.display()).as_bytes());
    host.wait_for("Files received.");
    host.wait_for("Ctrl+C dismisses or cancels.");
    let root = remote.root.join("state/kraal/attachments-v1").join(&remote.session);
    let batches: Vec<_> = std::fs::read_dir(&root).unwrap().map(|e| e.unwrap().path()).collect();
    assert_eq!(batches.len(), 1);
    let batch = &batches[0];
    assert_eq!(std::fs::read(batch.join("0/notes client.txt")).unwrap(), b"notes from the laptop");
    assert_eq!(std::fs::read(batch.join("1/capture.bin")).unwrap(), content);
    assert_eq!(std::fs::metadata(batch).unwrap().permissions().mode() & 0o777, 0o700);
    assert_eq!(std::fs::metadata(batch.join("1/capture.bin")).unwrap().permissions().mode() & 0o777, 0o600);
    // cat cannot print the paths until the user sends Enter.
    assert!(!remote.run(&["read", "-p", "main"]).contains("attachments-v1"));
    host.type_keys(b"\x03");
    // Cancellation consumes its whole terminal read. Wait for the result to be dismissed
    // before typing Enter, so the two keys cannot be coalesced into that read.
    wait_until(PATIENCE, "attachment result dismissed", || {
        let written = host.written.lock().unwrap();
        !text_after(&written, 100, 30).iter().any(|row| row.contains("Ctrl+C dismisses or cancels."))
    });
    host.type_keys(b"\r");
    let pasted = remote.wait_for_text("main", "attachments-v1");
    assert!(pasted.contains("notes client.txt"));
    assert!(pasted.contains("capture.bin"));
    other.wait_for_screen("capture.bin");
    host.type_keys(b"\x02d");
    assert_eq!(host.left("attachment detach").0, 0);
    assert_eq!(std::fs::read(batch.join("1/capture.bin")).unwrap(), content);
    remote.run(&["kill"]);
    assert!(batch.exists());
    remote.run(&["forget"]);
    assert!(!root.exists());
}

#[cfg(unix)]
#[test]
fn attachment_wire_cancellation_disconnect_and_repeated_finish_do_not_paste_partial_or_duplicate_paths() {
    let env = Env::new("attachment-wire");
    env.workspace(
        "w",
        "layout {\n    pane name=\"main\" command=\"sh\" { args \"-c\" \"stty -echo; echo READY; cat\"; }\n}\n",
    );
    env.run(&["open", "w"]);
    env.wait_for_text("main", "READY");
    let root = env.root.join("state/kraal/attachments-v1").join(&env.session);
    for outcome in ["cancel", "disconnect", "finish"] {
        let mut client = RawClient::attach(&env, "w");
        raw_frame(&mut client.stream, 0, br#"{"type":"attach","version":2,"cols":120,"rows":40,"workspace":"w","remote":true,"features":["attachments"]}"#);
        client.send(b"\x02 Attach files\r");
        client.wait_for_message("attachment_offer");
        let token = client
            .messages
            .lock()
            .unwrap()
            .iter()
            .find_map(|m| {
                let value: serde_json::Value = serde_json::from_str(m).ok()?;
                (value["type"] == "attachment_offer").then(|| value["token"].as_str().unwrap().to_string())
            })
            .unwrap();
        let send = |client: &mut RawClient, value: serde_json::Value| {
            raw_frame(&mut client.stream, 0, &serde_json::to_vec(&value).unwrap())
        };
        send(
            &mut client,
            serde_json::json!({"type":"attachment_start","token":token,"files":[{"name":"note","size":3}]}),
        );
        client.wait_for_message("attachment_ack");
        send(
            &mut client,
            serde_json::json!({"type":"attachment_chunk","token":token,"file":0,"offset":0,"data":"YWJj"}),
        );
        assert!(!env.run(&["read", "-p", "main"]).contains("attachments-v1"));
        match outcome {
            "cancel" => {
                send(&mut client, serde_json::json!({"type":"attachment_cancel","token":token}));
                client.wait_for_message("No paths or queued typing pasted");
            }
            "disconnect" => {
                drop(client);
            }
            "finish" => {
                send(&mut client, serde_json::json!({"type":"attachment_finish","token":token}));
                client.wait_for_message("Files received");
                send(&mut client, serde_json::json!({"type":"attachment_finish","token":token}));
                client.send(b"\r");
                let text = env.wait_for_text("main", "attachments-v1");
                assert_eq!(text.matches("attachments-v1").count(), 1);
                assert_eq!(std::fs::read(root.join(&token).join("0/note")).unwrap(), b"abc");
            }
            _ => unreachable!(),
        }
        wait_until(PATIENCE, "incomplete attachment removed", || !root.join(format!(".{token}")).exists());
        if outcome != "finish" {
            assert!(!root.join(token).exists());
        }
    }
}
