//! Clipboard policy and native tools run on the attaching machine, never on the server.

use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, bounded};
use serde::Deserialize;

use crate::protocol::{ClipboardOrigin as Origin, ClipboardOutcome as Outcome};

// JSON can expand each byte sixfold; even the largest copy must fit the 64 MiB wire frame.
pub const MAX_BYTES: usize = 8 << 20;
pub const INTERVAL: Duration = Duration::from_millis(100);
const TIMEOUT: Duration = Duration::from_secs(2);
const STARTUP: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Auto,
    Terminal,
    Off,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Settings {
    pub mode: Mode,
    pub max_bytes: usize,
}

#[derive(Clone)]
pub struct Policy {
    pub settings: Settings,
    pub allow_programs: bool,
    pub intermediate: bool,
}

impl Policy {
    fn native(&self, origin: Origin) -> bool {
        self.settings.mode == Mode::Auto
            && !self.intermediate
            && (origin == Origin::User || (origin == Origin::Program && self.allow_programs))
    }
}

pub fn intermediate() -> bool {
    intermediate_with(|name| std::env::var_os(name).is_some_and(|v| !v.is_empty()))
}

fn intermediate_with(present: impl Fn(&str) -> bool) -> bool {
    // The CLI hook also exists locally, but a tunnel need not set any other marker.
    // Prefer the terminal whenever native tools might address an intermediate machine.
    [
        "SSH_CONNECTION",
        "SSH_CLIENT",
        "SSH_TTY",
        "VSCODE_AGENT_FOLDER",
        "VSCODE_IPC_HOOK_CLI",
        "WSL_DISTRO_NAME",
        "WSL_INTEROP",
    ]
    .into_iter()
    .any(present)
}

fn copy(
    policy: &Policy,
    origin: Origin,
    text: &str,
    native: impl FnOnce(&[u8]) -> Option<Outcome>,
    terminal: impl FnOnce(&[u8]) -> bool,
) -> (Outcome, bool) {
    if policy.settings.mode == Mode::Off {
        return (Outcome::Disabled, false);
    }
    if text.len() > policy.settings.max_bytes.min(MAX_BYTES) {
        return (Outcome::TooLarge, false);
    }
    let native_unavailable = policy.native(origin);
    if native_unavailable && let Some(outcome) = native(text.as_bytes()) {
        return (outcome, false);
    }
    let outcome = if terminal(&crate::pane::osc52(text)) { Outcome::Terminal } else { Outcome::Failed };
    (outcome, native_unavailable)
}

struct Job {
    id: u64,
    origin: Origin,
    text: String,
}

/// All workers of an attachment, including one being started as a signal arrives.
#[derive(Default)]
pub struct Shutdown {
    state: Mutex<(bool, Vec<Weak<Cancellation>>)>,
}

impl Shutdown {
    pub fn cancel(&self) {
        let mut state = self.state.lock().unwrap();
        state.0 = true;
        for worker in state.1.iter().filter_map(Weak::upgrade) {
            worker.cancel();
        }
    }

    fn register(&self, worker: &Arc<Cancellation>) {
        let mut state = self.state.lock().unwrap();
        if state.0 {
            worker.cancel();
        } else {
            state.1.retain(|worker| worker.strong_count() != 0);
            state.1.push(Arc::downgrade(worker));
        }
    }
}

#[derive(Default)]
struct Cancellation {
    stopped: AtomicBool,
    // Spawn, cancellation and handoff share this lock. An unfinished helper is never
    // untracked, and a completed selection owner is no longer subject to cancellation.
    active: Mutex<Option<Child>>,
}

impl Cancellation {
    fn cancel(&self) {
        let mut active = self.active.lock().unwrap();
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(child) = active.take() {
            crate::exec::kill_group_nowait(child);
        }
    }

    fn kill_active(&self) {
        if let Some(child) = self.active.lock().unwrap().take() {
            crate::exec::kill_group_nowait(child);
        }
    }
}

struct Pending<'a>(&'a Cancellation);

impl Drop for Pending<'_> {
    fn drop(&mut self) {
        self.0.kill_active();
    }
}

pub struct Worker {
    tx: Sender<Job>,
    cancelled: Arc<Cancellation>,
}

impl Worker {
    pub fn start(
        policy: Policy,
        shutdown: &Shutdown,
        terminal: impl Fn(&[u8]) -> bool + Send + 'static,
        mut report: impl FnMut(u64, Outcome, bool, &AtomicBool) + Send + 'static,
    ) -> io::Result<Self> {
        let (tx, rx) = bounded::<Job>(1);
        let cancelled = Arc::new(Cancellation::default());
        shutdown.register(&cancelled);
        let stop = cancelled.clone();
        crate::threads::named("clipboard").spawn(move || {
            let mut backend = Backend::default();
            let mut last = None;
            let mut diagnosed = false;
            while let Ok(job) = rx.recv() {
                if stop.stopped.load(Ordering::SeqCst) {
                    break;
                }
                let (outcome, unavailable) = if last.is_some_and(|at: Instant| at.elapsed() < INTERVAL) {
                    (Outcome::Busy, false)
                } else {
                    last = Some(Instant::now());
                    copy(
                        &policy,
                        job.origin,
                        &job.text,
                        |bytes| backend.write(bytes, &stop),
                        |bytes| !stop.stopped.load(Ordering::SeqCst) && terminal(bytes),
                    )
                };
                if !stop.stopped.load(Ordering::SeqCst) {
                    report(job.id, outcome, unavailable && !diagnosed, &stop.stopped);
                    diagnosed |= unavailable;
                }
            }
            // A Wayland owner may live until the next copy, even after detach. Waiting here
            // keeps it reaped eventually without holding up the terminal or its input thread.
            if let Some(mut owner) = backend.owner {
                let _ = owner.wait();
            }
        })?;
        Ok(Self { tx, cancelled })
    }

    pub fn cancel(&self) {
        self.cancelled.cancel();
    }

    pub fn submit(&self, id: u64, origin: Origin, text: String) {
        // The server allows only one outstanding copy. An overproducing peer
        // cannot allocate an unbounded queue or create a thread per copy.
        let _ = self.tx.try_send(Job { id, origin, text });
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[derive(Default)]
struct Backend {
    owner: Option<Child>,
}

impl Backend {
    fn write(&mut self, bytes: &[u8], cancelled: &Cancellation) -> Option<Outcome> {
        #[cfg(target_os = "macos")]
        let commands: [(&str, &[&str], bool); 1] = [("pbcopy", &[], false)];
        #[cfg(not(target_os = "macos"))]
        let commands = {
            let mut commands: Vec<(&str, &[&str], bool)> = Vec::new();
            if std::env::var_os("WAYLAND_DISPLAY").is_some_and(|v| !v.is_empty()) {
                commands.push(("wl-copy", &["--type", "text/plain;charset=utf-8"], true));
            }
            if std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty()) {
                commands.push(("xclip", &["-selection", "clipboard", "-in"], true));
                commands.push(("xsel", &["--clipboard", "--input"], true));
            }
            commands
        };
        for (program, args, persistent) in commands {
            if cancelled.stopped.load(Ordering::SeqCst) {
                return None;
            }
            let mut cmd = Command::new(program);
            cmd.args(args);
            if let Some((outcome, owner)) = run(&mut cmd, bytes, persistent, cancelled, TIMEOUT) {
                if let Some(old) = self.owner.take() {
                    crate::exec::kill_group_nowait(old);
                }
                self.owner = owner;
                return Some(outcome);
            }
        }
        None
    }
}

/// The pipe is nonblocking too: a tool that never reads must not hold the worker forever.
fn run(
    cmd: &mut Command,
    bytes: &[u8],
    persistent: bool,
    cancelled: &Cancellation,
    timeout: Duration,
) -> Option<(Outcome, Option<Child>)> {
    let mut active = cancelled.active.lock().unwrap();
    if cancelled.stopped.load(Ordering::SeqCst) {
        return None;
    }
    let mut child =
        crate::exec::own_group(cmd).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().ok()?;
    let input = child.stdin.take();
    *active = Some(child);
    drop(active);
    let _pending = Pending(cancelled);
    let mut input = input?;
    let fd = input.as_raw_fd();
    // SAFETY: fcntl only changes the flags of this owned pipe descriptor.
    let nonblocking = unsafe { libc::fcntl(fd, libc::F_SETFL, libc::fcntl(fd, libc::F_GETFL) | libc::O_NONBLOCK) };
    if nonblocking < 0 {
        return None;
    }
    let start = Instant::now();
    let mut left = bytes;
    while !left.is_empty() {
        if cancelled.stopped.load(Ordering::SeqCst) || start.elapsed() >= timeout {
            return None;
        }
        match input.write(left) {
            Ok(0) => break,
            Ok(n) => left = &left[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => break,
        }
    }
    drop(input);
    if !left.is_empty() {
        return None;
    }
    let written = Instant::now();
    loop {
        if cancelled.stopped.load(Ordering::SeqCst) {
            return None;
        }
        let mut active = cancelled.active.lock().unwrap();
        let child = active.as_mut()?;
        match child.try_wait() {
            Ok(Some(status)) => {
                active.take();
                return status.success().then_some((Outcome::Native, None));
            }
            Ok(None) => {}
            Err(_) => {
                return None;
            }
        }
        // Selection owners need not exit to serve the clipboard. This says only that the
        // handoff completed, not that an unacknowledged persistent process confirmed a write.
        if persistent && written.elapsed() >= STARTUP {
            return Some((Outcome::NativeStarted, active.take()));
        }
        if start.elapsed() >= timeout {
            return None;
        }
        drop(active);
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn policy() -> Policy {
        Policy {
            settings: Settings { mode: Mode::Auto, max_bytes: 1 << 20 },
            allow_programs: false,
            intermediate: false,
        }
    }

    #[test]
    fn origins_permissions_and_intermediate_clients_choose_exactly_one_transport() {
        for mode in [Mode::Auto, Mode::Terminal, Mode::Off] {
            for origin in [Origin::User, Origin::Program, Origin::Unknown] {
                for allow_programs in [false, true] {
                    for intermediate in [false, true] {
                        for available in [false, true] {
                            let policy = Policy {
                                settings: Settings { mode, ..policy().settings },
                                allow_programs,
                                intermediate,
                            };
                            let native = Cell::new(0);
                            let terminal = Cell::new(0);
                            let (outcome, unavailable) = copy(
                                &policy,
                                origin,
                                "hello",
                                |_| {
                                    native.set(native.get() + 1);
                                    available.then_some(Outcome::Native)
                                },
                                |bytes| {
                                    terminal.set(terminal.get() + 1);
                                    assert_eq!(bytes, crate::pane::osc52("hello"));
                                    true
                                },
                            );
                            let attempt = mode == Mode::Auto
                                && !intermediate
                                && (origin == Origin::User || (origin == Origin::Program && allow_programs));
                            assert_eq!(native.get(), usize::from(attempt));
                            assert_eq!(terminal.get(), usize::from(mode != Mode::Off && !(attempt && available)));
                            assert_eq!(unavailable, attempt && !available);
                            assert_eq!(
                                outcome,
                                if mode == Mode::Off {
                                    Outcome::Disabled
                                } else if attempt && available {
                                    Outcome::Native
                                } else {
                                    Outcome::Terminal
                                }
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn byte_limit_rejects_without_truncation_or_a_backend_call() {
        let mut policy = policy();
        policy.settings.max_bytes = 4;
        assert_eq!(copy(&policy, Origin::User, "ééx", |_| panic!(), |_| panic!()), (Outcome::TooLarge, false));
        assert_eq!(
            copy(
                &policy,
                Origin::User,
                "éé",
                |b| {
                    assert_eq!(b, "éé".as_bytes());
                    Some(Outcome::Native)
                },
                |_| panic!()
            ),
            (Outcome::Native, false)
        );
        policy.settings.mode = Mode::Terminal;
        assert_eq!(copy(&policy, Origin::User, "", |_| panic!(), |_| false), (Outcome::Failed, false));
    }

    #[test]
    fn ssh_vscode_and_wsl_are_intermediate_but_local_nesting_is_not() {
        for name in [
            "SSH_CONNECTION",
            "SSH_CLIENT",
            "SSH_TTY",
            "VSCODE_AGENT_FOLDER",
            "VSCODE_IPC_HOOK_CLI",
            "WSL_DISTRO_NAME",
            "WSL_INTEROP",
        ] {
            assert!(intermediate_with(|key| key == name));
        }
        for name in ["TMUX", "KRAAL_SOCKET", "DISPLAY", "WAYLAND_DISPLAY"] {
            assert!(!intermediate_with(|key| key == name));
        }
    }

    fn shell(script: &str) -> Command {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", script]);
        cmd
    }

    #[test]
    fn native_success_failure_missing_tool_and_a_pipe_that_never_reads() {
        let stop = Cancellation::default();
        let bytes = vec![b'x'; 1 << 20];
        let result = run(&mut shell("test \"$(wc -c)\" -eq 1048576"), &bytes, false, &stop, crate::testing::PATIENCE);
        assert!(matches!(result, Some((Outcome::Native, None))));
        assert!(run(&mut shell("exit 1"), b"x", true, &stop, TIMEOUT).is_none());
        assert!(run(&mut Command::new("/no-such-kraal-clipboard-tool"), b"x", false, &stop, TIMEOUT).is_none());
        let start = Instant::now();
        assert!(run(&mut shell("sleep 60"), &bytes, true, &stop, Duration::from_millis(100)).is_none());
        assert!(start.elapsed() < Duration::from_secs(5), "a full stdin pipe must time out");
    }

    #[test]
    fn persistent_owner_lives_after_handoff_but_a_non_owner_times_out() {
        let stop = Cancellation::default();
        let result = run(&mut shell("cat >/dev/null; exec sleep 60"), b"text", true, &stop, TIMEOUT);
        let Some((Outcome::NativeStarted, Some(mut owner))) = result else { panic!("owner did not start") };
        stop.cancel();
        assert!(owner.try_wait().unwrap().is_none(), "clipboard owner was killed after copying");
        crate::exec::kill_group(&mut owner);
        let stop = Cancellation::default();
        assert!(
            run(&mut shell("cat >/dev/null; exec sleep 60"), b"text", false, &stop, Duration::from_millis(100))
                .is_none()
        );
        let stop = Cancellation::default();
        stop.cancel();
        assert!(run(&mut shell("cat >/dev/null; exec sleep 60"), b"text", true, &stop, TIMEOUT).is_none());
    }

    #[test]
    fn cancellation_reaches_a_helper_paused_before_reading() {
        let dir = std::env::temp_dir().join(format!("kraal-copy-cancel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ready = dir.join("ready");
        let copied = dir.join("copied");
        let stop = Arc::new(Cancellation::default());
        let worker_stop = stop.clone();
        let mut cmd = shell("echo $$ > \"$1\"; kill -STOP $$; cat > \"$2\"");
        cmd.arg("sh").arg(&ready).arg(&copied);
        let worker = std::thread::spawn(move || {
            run(&mut cmd, &vec![b'x'; 1 << 20], false, &worker_stop, crate::testing::PATIENCE)
        });
        let start = Instant::now();
        while !std::fs::read_to_string(&ready).is_ok_and(|s| !s.trim().is_empty()) {
            assert!(start.elapsed() < crate::testing::PATIENCE, "helper never started");
            std::thread::sleep(Duration::from_millis(5));
        }
        stop.cancel();
        assert!(stop.active.lock().unwrap().is_none(), "cancellation left its helper to the worker");
        assert!(worker.join().unwrap().is_none());
        assert!(!copied.exists(), "cancelled helper wrote to the clipboard");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn worker_submission_is_bounded_and_disconnect_discards_queued_work() {
        let (entered, entered_rx) = bounded(1);
        let (release, release_rx) = bounded(1);
        let (reports, reports_rx) = bounded(2);
        let mut policy = policy();
        policy.settings.mode = Mode::Terminal;
        let worker = Worker::start(
            policy,
            &Shutdown::default(),
            move |_| {
                entered.send(()).unwrap();
                release_rx.recv().unwrap();
                true
            },
            move |id, result, _, _| {
                reports.send((id, result)).unwrap();
            },
        )
        .unwrap();
        worker.submit(1, Origin::User, "first".into());
        entered_rx.recv_timeout(crate::testing::PATIENCE).unwrap();
        worker.submit(2, Origin::User, "second".into());
        worker.submit(3, Origin::User, "third".into());
        assert_eq!(worker.tx.len(), 1);
        drop(worker);
        release.send(()).unwrap();
        assert!(
            reports_rx.recv_timeout(crate::testing::PATIENCE).is_err(),
            "disconnected work must not report or replay"
        );
    }

    #[test]
    fn shutdown_cancels_current_workers_and_workers_registered_afterwards() {
        let shutdown = Shutdown::default();
        let first = Arc::new(Cancellation::default());
        shutdown.register(&first);
        shutdown.cancel();
        assert!(first.stopped.load(Ordering::SeqCst));
        let late = Arc::new(Cancellation::default());
        shutdown.register(&late);
        assert!(late.stopped.load(Ordering::SeqCst));
        assert!(run(&mut shell("exit 0"), b"", false, &late, TIMEOUT).is_none());
    }

    #[test]
    fn settings_are_optional_bounded_and_reject_unknown_modes() {
        let old = crate::config::Config::from_user_toml("mouse = false").unwrap();
        assert_eq!((old.clipboard.mode, old.clipboard.max_bytes), (Mode::Auto, 1 << 20));
        for value in [0, MAX_BYTES + 1] {
            assert!(crate::config::Config::from_user_toml(&format!("[clipboard]\nmax_bytes = {value}")).is_err());
        }
        for mode in ["auto", "terminal", "off"] {
            assert!(
                crate::config::Config::from_user_toml(&format!(
                    "[clipboard]\nmode = '{mode}'\nmax_bytes = {MAX_BYTES}"
                ))
                .is_ok()
            );
        }
        assert!(crate::config::Config::from_user_toml("[clipboard]\nmode = 'native'").is_err());
    }
}
