use std::path::{Path, PathBuf};

use super::locations::{ensure_private_dir, private_runtime_dir, state_dir};
use super::writes::file_id;

/// Lock `file` exclusively, without waiting: false when another open file holds it. The lock
/// goes when its last descriptor closes, including duplicates inherited by a forked child.
pub(crate) fn try_lock(file: &std::fs::File) -> std::io::Result<bool> {
    flock(file, false)
}

/// An exclusive flock(2) on `file`, waiting for it when `wait`: false when another open file
/// holds it and `wait` is false.
pub(crate) fn flock(file: &std::fs::File, wait: bool) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;
    let how = if wait { libc::LOCK_EX } else { libc::LOCK_EX | libc::LOCK_NB };
    loop {
        // SAFETY: flock on a descriptor we own.
        if unsafe { libc::flock(file.as_raw_fd(), how) } == 0 {
            return Ok(true);
        }
        let e = std::io::Error::last_os_error();
        match e.kind() {
            std::io::ErrorKind::WouldBlock => return Ok(false),
            std::io::ErrorKind::Interrupted => continue,
            _ => return Err(e),
        }
    }
}

/// A session held by one process: its server, from before it reads what the session saved until
/// nothing of it can write there any more, or `kraal forget` while it deletes that. Among the
/// kraal that take it, a session so never has two servers, nor loses its files to `forget` while
/// a server restores them, which a check that its socket answers cannot promise (a server not
/// listening yet, a socket deleted, a server whose socket is in another runtime directory).
///
/// It is held where the session saves, in the state directory: two kraal of one user that do not
/// resolve the same runtime directory (a desktop terminal with `$XDG_RUNTIME_DIR`, a cron job
/// without it) write the same files, and so do the machines that share a home directory. The
/// one that comes second is refused, and told who has the session ([`Holder`]). The lock of the
/// runtime directory is held as well: the only one kraal 0.3.0 takes, which so keeps out and is
/// kept out in that directory, and nowhere else. Kraal 0.2.0 takes none ([`super::sockets::listen_alone`]).
///
/// The state directory's lock file goes with its holder (every session name ever used would
/// leave one otherwise): only a kraal that looks at the file again once it has locked it takes
/// it. The runtime directory's stays: kraal 0.3.0 does not look again, and would hold a file
/// that is gone beside the holder of the one made since. That directory goes with the login.
pub struct SessionLock {
    _state: LockFile,
    /// `None` when it is the state directory's file: both directories are one.
    pub(super) _runtime: Option<LockFile>,
}

/// How a file is locked for this process alone: [`try_lock`], or what a test has in its place.
type Locker = fn(&std::fs::File) -> std::io::Result<bool>;

impl SessionLock {
    /// Hold `session`, unless another process does (`None`). An error when the state directory
    /// cannot hold it (its file system locks no file): a session is then neither started nor
    /// forgotten, where a second server or a `forget` could not be kept away from its files.
    pub fn acquire(session: &str) -> std::io::Result<Option<SessionLock>> {
        let runtime = private_runtime_dir()?;
        // What a session saves, and who holds it, is only ours to read.
        let state = state_dir();
        ensure_private_dir(&state)?;
        let holder = Holder {
            pid: std::process::id(),
            host: crate::context::hostname().unwrap_or_default(),
            runtime: runtime.clone(),
        };
        let lock = format!("{session}.lock");
        SessionLock::acquire_at(state.join(&lock), runtime.join(&lock), &holder, try_lock)
    }

    pub(super) fn acquire_at(
        state: PathBuf,
        runtime: PathBuf,
        holder: &Holder,
        lock: Locker,
    ) -> std::io::Result<Option<SessionLock>> {
        let dir = state.parent().unwrap_or(&state).display().to_string();
        let unheld = |e: std::io::Error| {
            let why = format!(
                "no session can be held in {dir} ({e}): kraal starts no server and forgets nothing where it \
                 cannot keep another kraal out. Keep its state on a file system that locks files (XDG_STATE_HOME)"
            );
            std::io::Error::new(e.kind(), why)
        };
        let Some(mut state) = LockFile::acquire(state, false, lock).map_err(unheld)? else { return Ok(None) };
        // One file for both where the state and the runtime directory are one, however each is
        // spelt: locked once (a second lock on it would wait for the first), and left in place
        // as the runtime directory's is.
        let runtime = if is_file_at(&state.file, &runtime) {
            state.stays = true;
            None
        } else {
            let Some(runtime) = LockFile::acquire(runtime, true, lock)? else { return Ok(None) };
            Some(runtime)
        };
        // What it says is for a user to read: the session is held whether or not it is written.
        let _ = state.say(holder);
        Ok(Some(SessionLock { _state: state, _runtime: runtime }))
    }
}

/// What the holder of a session says of itself in the state directory: for the kraal it keeps
/// out to tell its user, who alone acts on it. Nothing there is connected to or signalled.
#[derive(Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Holder {
    pub pid: u32,
    /// The machine it runs on: one that shares the state directory may be another.
    pub host: String,
    /// Where its sockets are.
    pub runtime: PathBuf,
}

/// A session kept while it is restarted, from before its server stops until the next one has
/// it: nobody holds the session in between ([`SessionLock`] is its server's), and a
/// `kraal forget` that came then would delete what the next server is about to restore.
/// `forget` takes this too, for the moment it takes, and so does nothing during a restart.
/// Kraal 0.3.0 and older do not take it.
///
/// The server a restart starts inherits it ([`Restarting::inherited`]): the lock is that of the
/// open file, which the two processes then share, so the session stays kept until that server
/// has it, even when the kraal that restarts is killed or gives up waiting before. The file
/// goes with whoever knows the session is held again: that server, or the kraal that restarts
/// once a server listens.
pub struct Restarting {
    lock: LockFile,
}

/// The environment variable that gives a server the descriptor of what keeps its session while
/// it is restarted ([`Restarting::inherited`]).
pub const RESTART_FD_VAR: &str = "KRAAL_RESTART_FD";

impl Restarting {
    /// Keep `session` for a restart, or for the time `kraal forget` takes. Another `forget` has
    /// it for a moment, and is waited for; `None` when a restart, which has it for as long as
    /// it takes, still does after two seconds.
    pub fn keep(session: &str) -> std::io::Result<Option<Restarting>> {
        let state = state_dir();
        ensure_private_dir(&state)?;
        let path = state.join(format!("{session}.restart"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            if let Some(lock) = LockFile::acquire(path.clone(), false, try_lock)? {
                return Ok(Some(Restarting { lock }));
            }
            if std::time::Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// The descriptor a server started for the restart is to inherit.
    pub fn descriptor(&self) -> std::os::fd::RawFd {
        std::os::fd::AsRawFd::as_raw_fd(&self.lock.file)
    }

    /// Let go of it without removing its file: a server started for the restart may still be
    /// on its way with it, and whoever made a file in its place would get past that server.
    pub fn leave_to_the_server(mut self) {
        self.lock.stays = true;
    }

    /// What the kraal that restarts `session` gave this process, a server it started: the
    /// descriptor [`RESTART_FD_VAR`] names, when it is the file that keeps the session. Taken
    /// out of the environment, and closed in what this server starts.
    pub fn inherited(session: &str) -> Option<Restarting> {
        use std::os::fd::FromRawFd;
        use std::os::unix::fs::MetadataExt;
        let value = std::env::var_os(RESTART_FD_VAR)?;
        // SAFETY: the server has not started any thread yet.
        unsafe { std::env::remove_var(RESTART_FD_VAR) };
        let fd: libc::c_int = value.to_str()?.parse().ok().filter(|fd| *fd > 2)?;
        // SAFETY: fstat and fcntl only look at the descriptor, and fail on one that is not
        // open. A regular file among those this process was started with is no other part's:
        // it is owned here from now on.
        let file = unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            if libc::fstat(fd, &mut st) != 0 || st.st_mode & libc::S_IFMT != libc::S_IFREG {
                return None;
            }
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            std::fs::File::from_raw_fd(fd)
        };
        let path = state_dir().join(format!("{session}.restart"));
        // SAFETY: getuid cannot fail.
        let ours = file.metadata().is_ok_and(|meta| meta.uid() == unsafe { libc::getuid() });
        // Until this server has the session, the file stays whatever becomes of this server.
        (ours && is_file_at(&file, &path)).then_some(Restarting { lock: LockFile { file, path, stays: true } })
    }

    /// The session is held again: what kept it for its restart goes, file and all.
    pub fn end(mut self) {
        self.lock.stays = false;
    }
}

/// What the holder of `session` says of itself, if it says so in a file of ours. What a holder
/// killed before said is there until the next one says its own.
pub fn holder(session: &str) -> Option<Holder> {
    holder_at(&state_dir().join(format!("{session}.lock")))
}

pub(super) fn holder_at(lock: &Path) -> Option<Holder> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    // Not waited for: what is not a file there (a FIFO) would never open.
    let flags = libc::O_NOFOLLOW | libc::O_NONBLOCK;
    let file = std::fs::OpenOptions::new().read(true).custom_flags(flags).open(lock).ok()?;
    let meta = file.metadata().ok()?;
    // SAFETY: getuid cannot fail.
    if !meta.is_file() || meta.uid() != unsafe { libc::getuid() } {
        return None;
    }
    let mut said = String::new();
    file.take(4096).read_to_string(&mut said).ok()?;
    serde_json::from_str(said.lines().next()?).ok()
}

/// A lock file held by one process.
pub(super) struct LockFile {
    /// Locked while open.
    pub(super) file: std::fs::File,
    path: PathBuf,
    /// Whether the file is left in place when its holder goes.
    stays: bool,
}

impl LockFile {
    /// Hold the lock file `path`, made if needed, unless another process does (`None`).
    pub(super) fn acquire(path: PathBuf, stays: bool, lock: Locker) -> std::io::Result<Option<LockFile>> {
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        loop {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&path)?;
            let meta = file.metadata()?;
            // SAFETY: getuid cannot fail.
            if !meta.is_file() || meta.uid() != unsafe { libc::getuid() } {
                return Err(std::io::Error::other(format!("{} is not a file of ours", path.display())));
            }
            if !lock(&file)? {
                return Ok(None);
            }
            if is_file_at(&file, &path) {
                return Ok(Some(LockFile { file, path, stays }));
            }
            // The file was removed between the time it was opened here and the time it was
            // locked (by its last holder, or by a cleaner of /tmp): it locks nothing any more.
            // The lock is the file there now, as it is for whoever comes next.
        }
    }

    /// Write in the file who holds it, in place of what its last holder said.
    fn say(&self, holder: &Holder) -> std::io::Result<()> {
        use std::os::unix::fs::FileExt;
        let mut line = serde_json::to_vec(holder).map_err(std::io::Error::other)?;
        line.push(b'\n');
        self.file.set_len(0)?;
        self.file.write_all_at(&line, 0)
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        // Removed while still held: nobody else holds it then, and who opened it before finds
        // out once it has locked it (see `acquire`). A file someone else put there meanwhile
        // (a cleaner of /tmp removed this one) is theirs.
        if !self.stays && is_file_at(&self.file, &self.path) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Whether the entry at `path` is `file` itself, not another file made there since it was opened.
pub(super) fn is_file_at(file: &std::fs::File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    file.metadata().is_ok_and(|open| file_id(path) == Some((open.dev(), open.ino())))
}
