//! Running helper commands (git, choice lists, ssh fan-out) with a deadline and bounded
//! output, so a stuck or chatty command can neither freeze kraal nor exhaust its memory.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
pub struct Output {
    /// Exit code; `None` when killed (timeout, signal).
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Output beyond the limit was dropped.
    pub truncated: bool,
    pub timed_out: bool,
    /// Stopped because the caller cancelled it.
    pub cancelled: bool,
}

impl Output {
    pub fn success(&self) -> bool {
        self.code == Some(0) && !self.timed_out && !self.cancelled
    }

    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// Put the command in its own process group, so killing it reaches its descendants.
pub fn own_group(cmd: &mut Command) -> &mut Command {
    use std::os::unix::process::CommandExt;
    cmd.process_group(0)
}

/// Whether anything still runs in the process group of a child started with [`own_group`]
/// (the child itself, or what it left in the background).
pub fn group_alive(child: &Child) -> bool {
    // SAFETY: signal 0 only checks that the group exists.
    unsafe { libc::kill(-(child.id() as libc::pid_t), 0) == 0 }
}

/// Whether `child` has ended, without reaping it: until it is reaped, its pid (and the id of
/// the group it leads) cannot go to another process, so [`kill_group`] still reaches only what
/// it started.
pub fn has_exited(child: &Child) -> bool {
    // SAFETY: waitid fills `info`, which we own; WNOWAIT leaves the child to be reaped later.
    unsafe {
        let mut info: libc::siginfo_t = std::mem::zeroed();
        let flags = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
        libc::waitid(libc::P_PID, child.id() as libc::id_t, &mut info, flags) == 0 && info.si_pid() != 0
    }
}

/// Kill a child started with [`own_group`] and everything it started, then reap it.
pub fn kill_group(child: &mut Child) {
    // SAFETY: plain syscall on a process group we created.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// [`kill_group`] without waiting for the child to end: one stuck in the kernel (a file system
/// that does not answer) ends when the kernel lets it, and a thread of its own reaps it then.
/// How it ended, if it had already.
pub fn kill_group_nowait(mut child: Child) -> Option<std::process::ExitStatus> {
    // SAFETY: plain syscall on a process group we created.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    let _ = child.kill();
    match child.try_wait() {
        Ok(Some(status)) => Some(status),
        _ => {
            reap_later(child);
            None
        }
    }
}

/// [`kill_group`], and also what the child started that left its group or session (`setsid`):
/// for a program that may start daemons of its own (a wrapper script around an agent).
pub fn kill_tree(child: &mut Child) {
    // Listed while the child lives: once it is gone, its orphans are no longer its descendants.
    let tree = crate::process::descendants(child.id());
    for pid in tree {
        // SAFETY: plain syscall on our own live descendants.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
    }
    kill_group(child);
}

/// `kraal supervise PROGRAM ARGS…`: run PROGRAM, then end whatever its processes left running,
/// however they left (a daemon that cleared its environment and left its session included).
/// On Linux kraal becomes a subreaper here, so what they orphan is reparented to it and still
/// found by parent; elsewhere only what is still below PROGRAM when it ends is found. Ends with
/// PROGRAM, or on SIGTERM, SIGINT or SIGHUP (PROGRAM first). Its exit status is PROGRAM's.
pub fn supervise(argv: &[std::ffi::OsString]) -> i32 {
    #[cfg(target_os = "linux")]
    // SAFETY: plain syscall on this process.
    unsafe {
        libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
    }
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        let _ = signal_hook::flag::register(signal, stop.clone());
    }
    let Some((program, args)) = argv.split_first() else { return 2 };
    let Ok(mut child) = Command::new(program).args(args).spawn() else { return 127 };
    // PROGRAM has the pipes now: whoever reads them sees their end when PROGRAM's processes
    // are done, not when this one is.
    if let Ok(null) = std::fs::OpenOptions::new().read(true).write(true).open("/dev/null") {
        use std::os::unix::io::AsRawFd;
        // SAFETY: replacing this process's own stdin and stdout with /dev/null.
        unsafe {
            libc::dup2(null.as_raw_fd(), 0);
            libc::dup2(null.as_raw_fd(), 1);
        }
    }
    let code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status_code(status),
            Ok(None) if !stop.load(Ordering::Relaxed) => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                break child.wait().map(status_code).unwrap_or(1);
            }
        }
    };
    end_descendants(Duration::from_secs(2));
    code
}

/// The exit code a shell would show for `status`.
fn status_code(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    status.code().or_else(|| status.signal().map(|s| 128 + s)).unwrap_or(1)
}

/// Kill every process below this one, and reap them, for at most `limit` (more may keep
/// appearing while they end).
fn end_descendants(limit: Duration) {
    let me = std::process::id();
    let start = Instant::now();
    loop {
        let left = crate::process::descendants(me);
        for pid in &left {
            // SAFETY: plain syscall on our own descendants.
            unsafe {
                libc::kill(*pid as libc::pid_t, libc::SIGKILL);
            }
        }
        // SAFETY: reaping our own children, never blocking.
        while unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) } > 0 {}
        if left.is_empty() || start.elapsed() > limit {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The environment variable [`kill_tagged`] finds processes by.
pub const TAG: &str = "KRAAL_RUN_TAG";

/// Kill what still runs with `TAG=tag` in its environment: what a command started with that
/// tag left behind, daemons that left its process tree included (unless they cleared their
/// environment).
pub fn kill_tagged(tag: &str) {
    for pid in crate::process::tagged(TAG, tag) {
        // SAFETY: plain syscall on our own processes, started under this tag.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGKILL);
        }
    }
}

/// Keep reading `pipe` to its end, remembering its last non-empty line (an error message), so
/// the writer never blocks on a full pipe.
pub fn last_line(mut pipe: impl Read + Send + 'static) -> Arc<std::sync::Mutex<String>> {
    const KEEP: usize = 1024;
    let last = Arc::new(std::sync::Mutex::new(String::new()));
    let keep = last.clone();
    std::thread::spawn(move || {
        // Bytes of the line being read, bounded: a stream without newlines cannot grow it.
        let mut line: Vec<u8> = Vec::new();
        let mut buf = [0u8; 4096];
        let publish = |line: &[u8]| {
            let text = String::from_utf8_lossy(line);
            if !text.trim().is_empty()
                && let Ok(mut l) = keep.lock()
            {
                *l = text.trim_end().chars().take(300).collect();
            }
        };
        loop {
            let n = match pipe.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            for &b in &buf[..n] {
                if b == b'\n' {
                    publish(&line);
                    line.clear();
                } else if line.len() < KEEP {
                    line.push(b);
                }
            }
        }
        publish(&line);
    });
    last
}

/// Read `pipe` line by line (each at most 1 KiB), giving each to `on_line`, until it ends or
/// `stop` is set: a process that inherited the pipe and outlived what was stopped (a helper
/// that left its group) never keeps the reader for good.
pub fn read_lines_until(
    pipe: impl Read + std::os::fd::AsRawFd + Send + 'static,
    stop: Arc<AtomicBool>,
    mut on_line: impl FnMut(&str) + Send + 'static,
) {
    read_bounded_lines(pipe, stop, 1024, move |line, _| on_line(String::from_utf8_lossy(line).trim_end()));
}

/// [`read_lines_until`] for lines of up to `max` bytes: a longer one is given as `None`, never
/// cut (a piece of a JSON document is not one).
pub fn read_long_lines_until(
    pipe: impl Read + std::os::fd::AsRawFd + Send + 'static,
    stop: Arc<AtomicBool>,
    max: usize,
    mut on_line: impl FnMut(Option<&str>) + Send + 'static,
) {
    read_bounded_lines(pipe, stop, max, move |line, whole| {
        let text = String::from_utf8_lossy(line);
        on_line(whole.then(|| text.trim_end()))
    });
}

/// Read `pipe` line by line until it ends or `stop` is set, keeping at most `keep` bytes of each
/// line; `on_line` gets what was kept and whether that is the whole line.
fn read_bounded_lines(
    mut pipe: impl Read + std::os::fd::AsRawFd + Send + 'static,
    stop: Arc<AtomicBool>,
    keep: usize,
    mut on_line: impl FnMut(&[u8], bool) + Send + 'static,
) {
    std::thread::spawn(move || {
        let mut line: Vec<u8> = Vec::new();
        let mut whole = true;
        let mut buf = [0u8; 4096];
        while !stop.load(Ordering::Relaxed) {
            let mut fds = libc::pollfd { fd: pipe.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one valid pollfd, owned by this frame.
            match unsafe { libc::poll(&mut fds, 1, 200) } {
                0 => continue,
                n if n < 0 => {
                    if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    break;
                }
                _ => {}
            }
            let n = match pipe.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            for &b in &buf[..n] {
                if b == b'\n' {
                    on_line(&line, whole);
                    line.clear();
                    whole = true;
                } else if line.len() < keep {
                    line.push(b);
                } else {
                    whole = false;
                }
            }
        }
        if !line.is_empty() {
            on_line(&line, whole);
        }
    });
}

/// The last non-empty line of a finished command's stderr, or how it ended.
pub fn last_stderr_line(out: &Output) -> String {
    let err = out.stderr_text();
    match err.lines().rev().map(str::trim).find(|l| !l.is_empty()) {
        Some(l) => l.to_string(),
        None if out.timed_out => "timed out".into(),
        None => format!("exit status {}", out.code.map_or("?".into(), |c| c.to_string())),
    }
}

/// Let a short-lived child run, and reap it when it ends (no zombie left behind).
pub fn reap_later(mut child: Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

/// Wait until these processes have exited, and the process groups they lead are empty (what
/// they started there too), at most `timeout`; whether they did. A directory is removed only
/// once nothing runs in it.
pub fn wait_gone(pids: &[u32], timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        let alive = pids
            .iter()
            .any(|&p| crate::process::exists(p as libc::pid_t) || crate::process::exists(-(p as libc::pid_t)));
        if !alive {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Read up to `limit` bytes, then keep draining (so the writer never blocks) without storing,
/// until the end of the stream or `stop`.
fn drain(mut pipe: impl Read + std::os::fd::AsRawFd, limit: usize, stop: &AtomicBool) -> (Vec<u8>, bool) {
    let mut kept = Vec::new();
    let mut truncated = false;
    let mut buf = [0u8; 8192];
    while !stop.load(Ordering::Relaxed) {
        // Wait for data at most 100 ms at a time, to notice `stop`.
        let mut fds = libc::pollfd { fd: pipe.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd, owned by this frame.
        match unsafe { libc::poll(&mut fds, 1, 100) } {
            0 => continue,
            n if n < 0 => {
                if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break;
            }
            _ => {}
        }
        match pipe.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = limit.saturating_sub(kept.len());
                kept.extend_from_slice(&buf[..n.min(room)]);
                truncated |= n > room;
            }
        }
    }
    (kept, truncated)
}

/// The readers of a child's stdout and stderr. They stop when told, so a descendant that keeps a
/// pipe open never leaves a thread blocked for good.
pub struct Drains {
    stop: Arc<AtomicBool>,
    rx: mpsc::Receiver<(bool, (Vec<u8>, bool))>,
    readers: usize,
}

impl Drains {
    /// Start reading the child's piped stdout and stderr, keeping `limit` bytes of each.
    pub fn start(child: &mut Child, limit: usize) -> Drains {
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let mut readers = 0;
        if let Some(out) = child.stdout.take() {
            readers += 1;
            let (tx, stop) = (tx.clone(), stop.clone());
            std::thread::spawn(move || {
                let _ = tx.send((true, drain(out, limit, &stop)));
            });
        }
        if let Some(err) = child.stderr.take() {
            readers += 1;
            let (tx, stop) = (tx.clone(), stop.clone());
            std::thread::spawn(move || {
                let _ = tx.send((false, drain(err, limit, &stop)));
            });
        }
        Drains { stop, rx, readers }
    }

    /// Once the child has ended: what the readers got, waiting at most `grace` for the end of
    /// the streams (a background descendant may hold them open), then stopping them.
    pub fn finish(self, grace: Duration) -> (Vec<u8>, Vec<u8>, bool) {
        let (mut out, mut err, mut truncated) = (Vec::new(), Vec::new(), false);
        let deadline = Instant::now() + grace;
        let mut got = 0;
        while got < self.readers {
            let left = deadline.saturating_duration_since(Instant::now());
            let result = match self.rx.recv_timeout(left) {
                Ok(r) => r,
                Err(_) => {
                    self.stop.store(true, Ordering::Relaxed);
                    match self.rx.recv_timeout(Duration::from_millis(300)) {
                        Ok(r) => r,
                        Err(_) => break,
                    }
                }
            };
            got += 1;
            let (is_out, (data, cut)) = result;
            truncated |= cut;
            if is_out {
                out = data;
            } else {
                err = data;
            }
        }
        self.stop.store(true, Ordering::Relaxed);
        (out, err, truncated)
    }
}

/// Run `cmd` (stdin closed), keeping at most `limit` bytes of each output stream, killing it
/// and its descendants after `timeout`.
pub fn run(cmd: &mut Command, timeout: Duration, limit: usize) -> std::io::Result<Output> {
    run_cancellable(cmd, timeout, limit, &AtomicBool::new(false))
}

/// [`run`] for a command the user may have to answer through the terminal (ssh asking for a
/// password or about a host key): it stays in the terminal's process group, and `input` is its
/// stdin (a file to upload, say). A timeout kills the command only.
pub fn run_interactive(cmd: &mut Command, input: Stdio, timeout: Duration, limit: usize) -> std::io::Result<Output> {
    run_from(cmd, input, timeout, limit, &AtomicBool::new(false), false)
}

/// [`run`] with `input` as stdin (what to send through ssh, say).
pub fn run_with_input(cmd: &mut Command, input: Stdio, timeout: Duration, limit: usize) -> std::io::Result<Output> {
    run_from(cmd, input, timeout, limit, &AtomicBool::new(false), true)
}

/// [`run`], also stopping (like a timeout) as soon as `cancel` is set.
pub fn run_cancellable(
    cmd: &mut Command,
    timeout: Duration,
    limit: usize,
    cancel: &AtomicBool,
) -> std::io::Result<Output> {
    run_from(cmd, Stdio::null(), timeout, limit, cancel, true)
}

fn run_from(
    cmd: &mut Command,
    input: Stdio,
    timeout: Duration,
    limit: usize,
    cancel: &AtomicBool,
    group: bool,
) -> std::io::Result<Output> {
    if group {
        own_group(cmd);
    }
    cmd.stdin(input).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let drains = Drains::start(&mut child, limit);
    let start = Instant::now();
    let mut out = Output::default();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                out.code = status.code();
                break;
            }
            Ok(None) if cancel.load(Ordering::Relaxed) => {
                kill_group(&mut child);
                out.cancelled = true;
                break;
            }
            Ok(None) if start.elapsed() >= timeout => {
                kill_group(&mut child);
                out.timed_out = true;
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(e) => {
                kill_group(&mut child);
                drains.finish(Duration::ZERO);
                return Err(e);
            }
        }
    }
    let (stdout, stderr, truncated) = drains.finish(Duration::from_millis(200));
    out.stdout = stdout;
    out.stderr = stderr;
    out.truncated = truncated;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut c = Command::new("sh");
        c.arg("-c").arg(script);
        c
    }

    #[test]
    fn a_line_too_long_is_dropped_whole() {
        let (reader, mut writer) = std::io::pipe().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        read_long_lines_until(reader, Arc::new(AtomicBool::new(false)), 8, move |l| {
            let _ = tx.send(l.map(str::to_string));
        });
        std::io::Write::write_all(&mut writer, b"[1,2]\n0123456789abc\nok\n").unwrap();
        drop(writer);
        let got: Vec<Option<String>> = rx.iter().collect();
        assert_eq!(got, [Some("[1,2]".to_string()), None, Some("ok".to_string())]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_tree_is_killed_even_what_left_its_session() {
        let dir = std::env::temp_dir().join(format!("kraal-tree-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("pid");
        let script = format!("setsid sh -c 'echo $$ > {}; exec sleep 30' & sleep 30", pidfile.display());
        let mut child = own_group(&mut sh(&script)).stdin(Stdio::null()).spawn().unwrap();
        let start = Instant::now();
        while std::fs::read_to_string(&pidfile).map_or(true, |s| s.trim().is_empty())
            && start.elapsed() < crate::testing::PATIENCE
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        let escaped: libc::pid_t = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        assert!(crate::process::running(escaped), "the escaped sleep runs");
        kill_tree(&mut child);
        let start = Instant::now();
        while crate::process::running(escaped) && start.elapsed() < crate::testing::PATIENCE {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!crate::process::running(escaped), "what left the session went too");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn what_was_started_under_a_tag_goes_even_once_orphaned() {
        let dir = std::env::temp_dir().join(format!("kraal-tag-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("pid");
        // A wrapper that starts a daemon (out of its group, session and tree) and exits at once.
        let script = format!(
            "daemon() {{ if command -v setsid >/dev/null; then setsid sh -c 'echo $$ > {f}; exec sleep 30'; \
             else perl -MPOSIX -e 'POSIX::setsid(); open(F, \">{f}\"); print F $$; close F; exec \"sleep\", \"30\"'; fi; }}; \
             daemon & exit 0",
            f = pidfile.display()
        );
        let tag = format!("test-{}", std::process::id());
        let mut child = own_group(&mut sh(&script)).env(TAG, &tag).stdin(Stdio::null()).spawn().unwrap();
        child.wait().unwrap();
        let start = Instant::now();
        while std::fs::read_to_string(&pidfile).map_or(true, |s| s.trim().is_empty())
            && start.elapsed() < crate::testing::PATIENCE
        {
            std::thread::sleep(Duration::from_millis(10));
        }
        let daemon: libc::pid_t = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
        // The wrapper is gone: its tree and group no longer hold the daemon.
        kill_tree(&mut child);
        std::thread::sleep(Duration::from_millis(100));
        assert!(crate::process::running(daemon), "out of reach of the tree");
        kill_tagged(&tag);
        let start = Instant::now();
        while crate::process::running(daemon) && start.elapsed() < crate::testing::PATIENCE {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!crate::process::running(daemon), "found by its tag");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_group_lives_while_what_it_left_runs() {
        let mut child = own_group(&mut sh("sleep 5 & exit 0"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child.wait().unwrap();
        assert!(group_alive(&child), "the background sleep is still in the group");
        kill_group(&mut child);
        let start = Instant::now();
        while group_alive(&child) && start.elapsed() < crate::testing::PATIENCE {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!group_alive(&child));
    }

    #[test]
    fn a_reader_stops_when_told_even_if_the_pipe_stays_open() {
        // `sleep` keeps the pipe open (as a helper that left its group would).
        let mut child = sh("echo first; echo debug1: second; exec sleep 30").stdout(Stdio::piped()).spawn().unwrap();
        let lines = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let keep = lines.clone();
        read_lines_until(child.stdout.take().unwrap(), stop.clone(), move |l| keep.lock().unwrap().push(l.into()));
        let start = Instant::now();
        while lines.lock().unwrap().len() < 2 && start.elapsed() < crate::testing::PATIENCE {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(*lines.lock().unwrap(), vec!["first".to_string(), "debug1: second".into()]);
        stop.store(true, Ordering::SeqCst);
        // The reader (holding the other end of `lines`) ends while `sleep` still runs.
        let start = Instant::now();
        while Arc::strong_count(&lines) > 1 && start.elapsed() < crate::testing::PATIENCE {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(Arc::strong_count(&lines), 1);
        assert!(matches!(child.try_wait(), Ok(None)));
        let _ = child.kill();
        let _ = child.wait();
    }

    #[test]
    fn an_ended_child_keeps_its_pid_until_reaped() {
        let mut child = own_group(&mut sh("exit 3")).stdin(Stdio::null()).spawn().unwrap();
        let start = Instant::now();
        while !has_exited(&child) && start.elapsed() < crate::testing::PATIENCE {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(has_exited(&child));
        // Not reaped: its pid, and its group id, are still its own.
        assert_eq!(unsafe { libc::kill(child.id() as libc::pid_t, 0) }, 0);
        kill_group(&mut child);
        assert_eq!(child.try_wait().unwrap().and_then(|s| s.code()), Some(3));
    }

    #[test]
    fn last_lines_are_bounded() {
        let mut child =
            sh("printf 'first\\n' >&2; head -c 3000000 /dev/zero | tr '\\0' x >&2; printf '\\nlast words\\n' >&2")
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
        let last = last_line(child.stderr.take().unwrap());
        child.wait().unwrap();
        let start = Instant::now();
        while last.lock().unwrap().as_str() != "last words" && start.elapsed() < crate::testing::PATIENCE {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(last.lock().unwrap().as_str(), "last words");
    }

    #[test]
    fn captures_and_reports() {
        let out = run(&mut sh("echo out; echo err >&2; exit 3"), crate::testing::PATIENCE, 1024).unwrap();
        assert_eq!((out.code, out.stdout_text().as_str(), out.stderr_text().as_str()), (Some(3), "out\n", "err\n"));
        assert!(!out.success() && !out.timed_out && !out.truncated);
    }

    #[test]
    fn big_output_neither_blocks_nor_grows() {
        // 1 MiB through a pipe whose buffer is 64 KiB: the child must not block on us.
        let start = Instant::now();
        let out = run(&mut sh("head -c 1048576 /dev/zero"), crate::testing::PATIENCE, 1000).unwrap();
        assert!(out.success(), "{out:?}");
        assert_eq!(out.stdout.len(), 1000);
        assert!(out.truncated);
        assert!(start.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn a_descendant_holding_the_pipe_does_not_hold_us() {
        // `sleep` keeps stdout open after `sh` exits, in its own session (out of our group), for
        // 30 s: held by it, this would take that long, where a loaded machine takes a few
        // seconds to start `sh`.
        let start = Instant::now();
        let out = run(&mut sh("echo early; setsid sleep 30 & exit 0"), Duration::from_secs(30), 1024).unwrap();
        assert_eq!(out.stdout_text(), "early\n");
        assert!(out.success());
        assert!(start.elapsed() < Duration::from_secs(15), "{:?}", start.elapsed());
    }

    #[test]
    fn cancelling_stops_it() {
        let cancel = AtomicBool::new(true);
        let out = run_cancellable(&mut sh("sleep 5"), Duration::from_secs(10), 64, &cancel).unwrap();
        assert!(out.cancelled && !out.success());
    }

    #[test]
    fn timeout_kills_the_whole_group() {
        let start = Instant::now();
        // The background sleep keeps stdout open: killing only `sh` would leave us waiting for
        // its 30 s, where a loaded machine takes a few seconds to start and stop them.
        let out = run(&mut sh("sleep 30 & sleep 30"), Duration::from_millis(200), 1024).unwrap();
        assert!(out.timed_out && out.code.is_none());
        assert!(start.elapsed() < Duration::from_secs(15), "{:?}", start.elapsed());
    }

    /// A process that runs as another user (as a program run through sudo does) is not gone
    /// because it may not be signalled. Nothing to check as root, or where no such process shows.
    #[test]
    fn another_users_process_is_not_gone() {
        // SAFETY: getuid cannot fail.
        let me = unsafe { libc::getuid() };
        let ps = Command::new("ps").args(["-A", "-o", "pid=", "-o", "uid="]).output().unwrap();
        // The lowest pid: a process of the system's own, there for good.
        let theirs = String::from_utf8_lossy(&ps.stdout).lines().find_map(|line| {
            let mut fields = line.split_whitespace().map(str::parse::<u32>);
            let (Some(Ok(pid)), Some(Ok(uid))) = (fields.next(), fields.next()) else { return None };
            (pid > 1 && uid != me).then_some(pid)
        });
        let Some(pid) = theirs.filter(|_| me != 0) else { return };
        assert!(!wait_gone(&[pid], Duration::from_millis(50)), "process {pid} still runs");
    }
}
