//! Starting a pane's program, and holding the programs of many panes to start them after.
//!
//! portable-pty starts a program with `std::process::Command`, which forks the server: the more
//! memory the server has, the longer each fork. On Linux a pane's program is started here with
//! `posix_spawn` instead, which does not copy it. Either way the start waits for the program to
//! run, so that a failure is said at once. The panes of a workspace are set up while a batch is
//! held (see [`batch`]): all of them first, then their programs are started one after the other
//! on a thread of their own (see `super::pty::launcher`), while the server shows the first
//! prompts.

use std::cell::Cell;

use anyhow::Result;
use portable_pty::{Child, CommandBuilder, MasterPty, SlavePty};

/// A pane's program, started.
pub type PaneChild = Box<dyn Child + Send + Sync>;

thread_local! {
    /// How many batches are held on this thread (the server's).
    static BATCHES: Cell<usize> = const { Cell::new(0) };
}

/// While a batch is held, the panes set up on this thread do not start their programs: the
/// server starts them all together when it is done (see `Server::with_batch`).
pub struct Batch(());

/// Hold the programs of the panes set up from now on (batches nest).
pub fn batch() -> Batch {
    BATCHES.with(|b| b.set(b.get() + 1));
    Batch(())
}

impl Drop for Batch {
    fn drop(&mut self) {
        BATCHES.with(|b| b.set(b.get().saturating_sub(1)));
    }
}

/// Whether a batch is held on this thread.
pub fn batching() -> bool {
    BATCHES.with(|b| b.get() > 0)
}

/// What starts a pane's program: the program found, and what it needs, ready to be started
/// (on any thread). It holds the terminal's slave end until then, so that the terminal does not
/// hang up meanwhile.
pub struct Launch {
    how: How,
    slave: Box<dyn SlavePty + Send>,
}

enum How {
    #[cfg(target_os = "linux")]
    Spawn(linux::Prepared),
    Portable(CommandBuilder),
}

/// Get `cmd` ready to start with the terminal of `master` (whose slave end is `slave`) as its
/// controlling terminal: what would make it fail to be found fails now.
pub(super) fn prepare(master: &dyn MasterPty, slave: Box<dyn SlavePty + Send>, cmd: CommandBuilder) -> Result<Launch> {
    #[cfg(target_os = "linux")]
    if let Some(tty) = master.tty_name().filter(|_| linux::environment_is_text()) {
        return Ok(Launch { how: How::Spawn(linux::Prepared::new(&cmd, &tty)?), slave });
    }
    let _ = master;
    Ok(Launch { how: How::Portable(cmd), slave })
}

impl Launch {
    /// Start the program, and wait until it runs: a failure is said now.
    pub fn run(self) -> Result<PaneChild> {
        let child = match self.how {
            #[cfg(target_os = "linux")]
            How::Spawn(prepared) => Box::new(prepared.spawn()?) as PaneChild,
            How::Portable(cmd) => self.slave.spawn_command(cmd)?,
        };
        drop(self.slave);
        Ok(child)
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::ffi::{CString, OsStr, OsString};
    use std::os::unix::ffi::OsStrExt;
    use std::path::{Component, Path, PathBuf};

    use anyhow::{Context, Result, bail};
    use portable_pty::{Child, ChildKiller, CommandBuilder, ExitStatus};

    /// Where `exe` is, looked for as portable-pty does (and with the same errors): relative to
    /// `dir` when it starts with `./` or `../`, on `path` (relative entries to `dir`) when it is a
    /// bare name or another relative path, as it is when absolute.
    pub(super) fn find_program(exe: &OsStr, dir: &Path, path: Option<&OsStr>) -> Result<PathBuf> {
        let exe_path = Path::new(exe);
        if exe_path.is_relative() {
            if matches!(exe_path.components().next(), Some(Component::CurDir | Component::ParentDir)) {
                let abs = dir.join(exe_path);
                if abs.is_dir() {
                    bail!("Unable to spawn {} because it is a directory", abs.display());
                } else if access(&abs, libc::X_OK) {
                    return Ok(abs);
                } else if access(&abs, libc::F_OK) {
                    bail!("Unable to spawn {} because it is not executable", abs.display());
                }
                bail!("Unable to spawn {} because it does not exist", abs.display());
            }
            let mut errors = Vec::new();
            match path {
                Some(path) => {
                    for entry in std::env::split_paths(path) {
                        let candidate = dir.join(entry).join(exe);
                        if candidate.is_dir() {
                            errors.push(format!("{} exists but is a directory", candidate.display()));
                        } else if access(&candidate, libc::X_OK) {
                            return Ok(candidate);
                        } else if access(&candidate, libc::F_OK) {
                            errors.push(format!("{} exists but is not executable", candidate.display()));
                        }
                    }
                    errors.push(format!("No viable candidates found in PATH {path:?}"));
                }
                None => errors.push("Unable to resolve the PATH".to_string()),
            }
            bail!("Unable to spawn {} because:\n{}", exe_path.display(), errors.join(".\n"));
        }
        if exe_path.is_dir() {
            bail!("Unable to spawn {} because it is a directory", exe_path.display());
        }
        if !access(exe_path, libc::X_OK) {
            if access(exe_path, libc::F_OK) {
                bail!("Unable to spawn {} because it is not executable", exe_path.display());
            }
            bail!("Unable to spawn {} because it doesn't exist on the filesystem", exe_path.display());
        }
        Ok(exe_path.to_path_buf())
    }

    fn access(path: &Path, mode: libc::c_int) -> bool {
        let Ok(c) = CString::new(path.as_os_str().as_bytes()) else { return false };
        // SAFETY: access only reads the NUL-terminated path.
        unsafe { libc::access(c.as_ptr(), mode) == 0 }
    }

    /// Whether this process's environment is all UTF-8: the environment a pane gets is then all
    /// in [`CommandBuilder::iter_full_env_as_str`] (what is added to it is text).
    pub(super) fn environment_is_text() -> bool {
        static TEXT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *TEXT.get_or_init(|| std::env::vars_os().all(|(k, v)| k.to_str().is_some() && v.to_str().is_some()))
    }

    /// The login shell of the user, as portable-pty takes it when `SHELL` is not set.
    fn passwd_shell() -> OsString {
        // SAFETY: getpwuid returns null or a record valid until the next call; it is copied now.
        let shell = unsafe {
            let ent = libc::getpwuid(libc::getuid());
            (!ent.is_null() && !(*ent).pw_shell.is_null())
                .then(|| OsStr::from_bytes(std::ffi::CStr::from_ptr((*ent).pw_shell).to_bytes()).to_owned())
        };
        shell.filter(|s| access(Path::new(s), libc::X_OK)).unwrap_or_else(|| "/bin/sh".into())
    }

    fn cstring(s: &OsStr) -> Result<CString> {
        CString::new(s.as_bytes()).with_context(|| format!("{s:?} holds a NUL byte"))
    }

    /// What `posix_spawn` is given, made as portable-pty makes its command.
    pub(super) struct Prepared {
        program: CString,
        argv: Vec<CString>,
        env: Vec<CString>,
        dir: CString,
        tty: CString,
        /// For a program that turns out not to be one the kernel runs (a script without `#!`):
        /// `/bin/sh` runs it, as `execvp` does.
        script: Vec<CString>,
    }

    impl Prepared {
        /// What portable-pty would run for `cmd`: the program found from the directory (home
        /// when it is not one), argv[0] as given, its environment with `SHELL` set.
        pub(super) fn new(cmd: &CommandBuilder, tty: &Path) -> Result<Prepared> {
            let argv = cmd.get_argv();
            let exe = argv.first().context("no program to run")?;
            let home =
                cmd.get_env("HOME").map(PathBuf::from).or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from("/"));
            let dir = cmd.get_cwd().map(PathBuf::from).filter(|d| d.is_dir()).unwrap_or(home);
            let program = find_program(exe, &dir, cmd.get_env("PATH"))?;
            let mut env: Vec<CString> = Vec::new();
            let mut shell = false;
            for (k, v) in cmd.iter_full_env_as_str() {
                shell |= k == "SHELL";
                env.push(CString::new(format!("{k}={v}")).with_context(|| format!("${k} holds a NUL byte"))?);
            }
            if !shell {
                let mut entry = b"SHELL=".to_vec();
                entry.extend_from_slice(passwd_shell().as_bytes());
                env.push(CString::new(entry).context("the login shell's name holds a NUL byte")?);
            }
            let program = cstring(program.as_os_str())?;
            let argv: Vec<CString> = argv.iter().map(|a| cstring(a)).collect::<Result<_>>()?;
            let mut script = vec![CString::new("/bin/sh").expect("no NUL"), program.clone()];
            script.extend(argv.iter().skip(1).cloned());
            Ok(Prepared { program, argv, env, dir: cstring(dir.as_os_str())?, tty: cstring(tty.as_os_str())?, script })
        }

        /// Start the program, set up as portable-pty sets it up: default signal handling and no
        /// signal blocked, a session of its own whose controlling terminal is the pane's (opened
        /// as its standard descriptors), the directory, and nothing else of the server's open.
        /// A program the kernel does not run (a script without `#!`) runs with `/bin/sh`, as
        /// `execvp` runs it.
        pub(super) fn spawn(self) -> Result<Forked> {
            match self.spawn_program(&self.program, &self.argv) {
                Err(e) if e.raw_os_error() == Some(libc::ENOEXEC) => {
                    Ok(self.spawn_program(&self.script[0], &self.script)?)
                }
                other => Ok(other?),
            }
        }

        fn spawn_program(&self, program: &CString, argv: &[CString]) -> std::io::Result<Forked> {
            let pointers = |s: &[CString]| -> Vec<*mut libc::c_char> {
                s.iter().map(|s| s.as_ptr().cast_mut()).chain(std::iter::once(std::ptr::null_mut())).collect()
            };
            let (argv, envp) = (pointers(argv), pointers(&self.env));
            let check = |r: libc::c_int| if r == 0 { Ok(()) } else { Err(std::io::Error::from_raw_os_error(r)) };
            // SAFETY: the attributes and file actions are initialised before use and destroyed
            // after; every string outlives the call.
            unsafe {
                let mut attr: libc::posix_spawnattr_t = std::mem::zeroed();
                let mut actions: libc::posix_spawn_file_actions_t = std::mem::zeroed();
                check(libc::posix_spawnattr_init(&mut attr))?;
                if let Err(e) = check(libc::posix_spawn_file_actions_init(&mut actions)) {
                    libc::posix_spawnattr_destroy(&mut attr);
                    return Err(e);
                }
                let result = (|| {
                    let mut all: libc::sigset_t = std::mem::zeroed();
                    let mut none: libc::sigset_t = std::mem::zeroed();
                    libc::sigfillset(&mut all);
                    libc::sigemptyset(&mut none);
                    check(libc::posix_spawnattr_setsigdefault(&mut attr, &all))?;
                    check(libc::posix_spawnattr_setsigmask(&mut attr, &none))?;
                    let flags = libc::POSIX_SPAWN_SETSID
                        | libc::POSIX_SPAWN_SETSIGDEF as libc::c_short
                        | libc::POSIX_SPAWN_SETSIGMASK as libc::c_short;
                    check(libc::posix_spawnattr_setflags(&mut attr, flags))?;
                    // Opened by the new session's leader, the terminal becomes its controlling one.
                    check(libc::posix_spawn_file_actions_addopen(&mut actions, 0, self.tty.as_ptr(), libc::O_RDWR, 0))?;
                    check(libc::posix_spawn_file_actions_adddup2(&mut actions, 0, 1))?;
                    check(libc::posix_spawn_file_actions_adddup2(&mut actions, 0, 2))?;
                    check(libc::posix_spawn_file_actions_addchdir_np(&mut actions, self.dir.as_ptr()))?;
                    // What the server inherited without close-on-exec (it marks it so when it
                    // starts where this is not there, see `close_inherited`).
                    #[cfg(target_env = "gnu")]
                    check(libc::posix_spawn_file_actions_addclosefrom_np(&mut actions, 3))?;
                    let mut pid: libc::pid_t = 0;
                    check(libc::posix_spawn(
                        &mut pid,
                        program.as_ptr(),
                        &actions,
                        &attr,
                        argv.as_ptr(),
                        envp.as_ptr(),
                    ))?;
                    Ok(Forked { pid: pid as u32, status: None })
                })();
                libc::posix_spawn_file_actions_destroy(&mut actions);
                libc::posix_spawnattr_destroy(&mut attr);
                result
            }
        }
    }

    /// A pane's program started by [`Prepared::spawn`]; reaped by [`Child::try_wait`] or
    /// [`Child::wait`].
    #[derive(Debug)]
    pub(super) struct Forked {
        pid: u32,
        status: Option<ExitStatus>,
    }

    /// The exit status `waitpid` gave.
    fn exit_status(status: libc::c_int) -> ExitStatus {
        if libc::WIFSIGNALED(status) {
            ExitStatus::with_signal(&format!("signal {}", libc::WTERMSIG(status)))
        } else {
            ExitStatus::with_exit_code(libc::WEXITSTATUS(status) as u32)
        }
    }

    impl Child for Forked {
        fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
            if let Some(s) = &self.status {
                return Ok(Some(s.clone()));
            }
            let mut status = 0;
            // SAFETY: plain syscall on our own child.
            match unsafe { libc::waitpid(self.pid as libc::pid_t, &mut status, libc::WNOHANG) } {
                0 => Ok(None),
                n if n < 0 => Err(std::io::Error::last_os_error()),
                _ => {
                    self.status = Some(exit_status(status));
                    Ok(self.status.clone())
                }
            }
        }

        fn wait(&mut self) -> std::io::Result<ExitStatus> {
            if let Some(s) = &self.status {
                return Ok(s.clone());
            }
            loop {
                let mut status = 0;
                // SAFETY: plain syscall on our own child.
                if unsafe { libc::waitpid(self.pid as libc::pid_t, &mut status, 0) } >= 0 {
                    let s = exit_status(status);
                    self.status = Some(s.clone());
                    return Ok(s);
                }
                let e = std::io::Error::last_os_error();
                if e.kind() != std::io::ErrorKind::Interrupted {
                    return Err(e);
                }
            }
        }

        fn process_id(&self) -> Option<u32> {
            Some(self.pid)
        }
    }

    impl ChildKiller for Forked {
        /// As portable-pty does: SIGHUP, a moment to go, then SIGKILL.
        fn kill(&mut self) -> std::io::Result<()> {
            if self.try_wait()?.is_some() {
                return Ok(());
            }
            // SAFETY: plain syscall on our own child, not reaped yet.
            if unsafe { libc::kill(self.pid as libc::pid_t, libc::SIGHUP) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            for attempt in 0..5 {
                if attempt > 0 {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                if self.try_wait()?.is_some() {
                    return Ok(());
                }
            }
            // SAFETY: as above.
            unsafe { libc::kill(self.pid as libc::pid_t, libc::SIGKILL) };
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(Killer(self.pid))
        }
    }

    /// Signals a [`Forked`] child from elsewhere.
    #[derive(Debug)]
    struct Killer(u32);

    impl ChildKiller for Killer {
        fn kill(&mut self) -> std::io::Result<()> {
            // SAFETY: plain syscall.
            if unsafe { libc::kill(self.0 as libc::pid_t, libc::SIGHUP) } != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        }

        fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
            Box::new(Killer(self.0))
        }
    }
}

/// Make room for the descriptors of many panes now, while the server has a single thread. Each
/// pane takes several; when a process has more than its table holds (64, then 128...), Linux
/// makes a larger one, and with several threads it then waits for every processor to have
/// moved on from the old one (an RCU grace period): many milliseconds, in the middle of setting
/// up a workspace. A table made larger at once is never made smaller.
pub fn reserve_descriptors() {
    // SAFETY: getrlimit fills the struct; dup2 onto a number unused, closed at once.
    unsafe {
        let mut lim: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return;
        }
        let top = lim.rlim_cur.min(1024) as libc::c_int - 1;
        if top > 64 && libc::fcntl(top, libc::F_GETFD) == -1 && libc::dup2(0, top) == top {
            libc::close(top);
        }
    }
}

/// Mark close-on-exec every descriptor above the standard ones this process has: what it
/// inherited stays out of the programs it starts, whatever starts them. Done once, as the
/// server starts, before any thread.
pub fn close_inherited() {
    #[cfg(target_os = "linux")]
    let dir = "/proc/self/fd";
    #[cfg(not(target_os = "linux"))]
    let dir = "/dev/fd";
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let fds: Vec<libc::c_int> =
        entries.filter_map(|e| e.ok()?.file_name().to_str()?.parse().ok()).filter(|fd| *fd > 2).collect();
    for fd in fds {
        // SAFETY: plain fcntl; one closed meanwhile (the directory's own) fails harmlessly.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFD);
            if flags >= 0 {
                libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::path::PathBuf;
    use std::time::Duration;

    use portable_pty::{PtySize, native_pty_system};

    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kraal-spawn-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    fn pty() -> portable_pty::PtyPair {
        native_pty_system().openpty(PtySize { rows: 50, cols: 200, pixel_width: 0, pixel_height: 0 }).unwrap()
    }

    /// How a program is started in the tests: here, or by portable-pty.
    #[derive(Clone, Copy)]
    enum Way {
        Here,
        Portable,
    }

    /// Start `cmd` with the terminal of `master` (slave end `slave`) the `way` asked.
    fn started(master: &dyn MasterPty, slave: Box<dyn SlavePty + Send>, cmd: CommandBuilder, way: Way) -> PaneChild {
        match way {
            Way::Here => prepare(master, slave, cmd).unwrap().run().unwrap(),
            Way::Portable => slave.spawn_command(cmd).unwrap(),
        }
    }

    /// Run `cmd` in a terminal of its own: what it printed, and its exit code.
    fn run(cmd: CommandBuilder, way: Way) -> (String, u32) {
        let pair = pty();
        let mut reader = pair.master.try_clone_reader().unwrap();
        let (tx, rx) = crossbeam_channel::bounded(1);
        std::thread::spawn(move || {
            let mut out = Vec::new();
            let _ = reader.read_to_end(&mut out);
            let _ = tx.send(out);
        });
        let mut child = started(&*pair.master, pair.slave, cmd, way);
        let code = child.wait().unwrap().exit_code();
        let out = rx.recv_timeout(Duration::from_secs(10)).unwrap();
        drop(pair.master);
        (String::from_utf8_lossy(&out).replace("\r\n", "\n"), code)
    }

    #[test]
    fn a_program_starts_as_portable_pty_starts_it() {
        let dir = temp("same");
        let cmd = || {
            let mut c = CommandBuilder::new("sh");
            c.args(["-c", "env | LC_ALL=C sort; pwd -P; echo \"$0 $1\"; exit 3", "zero", "one"]);
            c.cwd(&dir);
            c.env("KRAAL_SPAWN_TEST", "yes");
            c.env_remove("KRAAL_SPAWN_NOT_THERE");
            c
        };
        let (portable, code) = run(cmd(), Way::Portable);
        assert_eq!(code, 3);
        assert!(portable.contains("KRAAL_SPAWN_TEST=yes\n"), "{portable}");
        assert!(portable.contains(&format!("{}\nzero one\n", dir.display())), "{portable}");
        assert_eq!(run(cmd(), Way::Here), (portable, 3), "the same environment, directory and arguments");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_program_gets_its_terminal_and_session() {
        let pair = pty();
        let mut cmd = CommandBuilder::new("sleep");
        cmd.arg("5");
        let mut child = started(&*pair.master, pair.slave, cmd, Way::Here);
        let pid = child.process_id().unwrap() as libc::pid_t;
        // A session of its own, led by it, in the foreground of the terminal.
        // SAFETY: plain syscall.
        assert_eq!(unsafe { libc::getsid(pid) }, pid);
        assert_eq!(pair.master.process_group_leader(), Some(pid));
        child.kill().unwrap();
        assert!(child.try_wait().unwrap().is_some(), "ended and reaped");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn a_program_that_cannot_be_found_or_run_is_said() {
        use std::ffi::OsStr;
        use std::path::Path;

        use super::linux::find_program;

        let pair = pty();
        let err = prepare(&*pair.master, pair.slave, CommandBuilder::new("no-such-program-kraal")).err();
        let err = format!("{:#}", err.expect("not found"));
        assert!(err.contains("Unable to spawn no-such-program-kraal"), "{err}");
        let dir = temp("find");
        assert!(find_program(OsStr::new("./x"), &dir, None).is_err());
        crate::testing::write_script(&dir.join("x"), "#!/bin/sh\n");
        assert_eq!(find_program(OsStr::new("./x"), &dir, None).unwrap(), dir.join("./x"));
        let found = find_program(OsStr::new("x"), Path::new("/"), Some(dir.as_os_str())).unwrap();
        assert_eq!(found, dir.join("x"));
        // Found and executable, but its interpreter is not there: only starting it tells.
        crate::testing::write_script(&dir.join("broken"), "#!/no/such/interpreter\n");
        let mut cmd = CommandBuilder::new("./broken");
        cmd.cwd(&dir);
        let pair = pty();
        let err = prepare(&*pair.master, pair.slave, cmd).unwrap().run().expect_err("cannot run");
        assert!(format!("{err:#}").contains("No such file or directory"), "{err:#}");
        // A script without `#!` runs with /bin/sh, as execvp runs it.
        crate::testing::write_script(&dir.join("plain"), "echo from-plain-script\n");
        let mut cmd = CommandBuilder::new("./plain");
        cmd.cwd(&dir);
        let (out, code) = run(cmd, Way::Here);
        assert_eq!(code, 0);
        assert!(out.contains("from-plain-script"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_batch_holds_on_this_thread_only() {
        assert!(!batching());
        let outer = batch();
        let inner = batch();
        assert!(batching());
        assert!(!std::thread::spawn(batching).join().unwrap(), "not another thread's");
        drop(inner);
        assert!(batching(), "the outer batch still holds");
        drop(outer);
        assert!(!batching());
    }

    #[test]
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    fn nothing_of_the_server_stays_open_in_the_program() {
        // A descriptor without close-on-exec, as one the server inherited would be.
        let file = std::fs::File::open("/dev/null").unwrap();
        // SAFETY: a copy of a descriptor this test owns, closed below.
        let fd = unsafe { libc::fcntl(std::os::fd::AsRawFd::as_raw_fd(&file), libc::F_DUPFD, 300) };
        assert!(fd >= 300);
        let mut cmd = CommandBuilder::new("sh");
        cmd.args(["-c", &format!("[ -e /proc/$$/fd/{fd} ] && echo leaked || echo closed")]);
        let (out, _) = run(cmd, Way::Here);
        assert!(out.contains("closed"), "{out}");
        // SAFETY: closing the copy made above.
        unsafe { libc::close(fd) };
    }
}
