//! Filesystem locations: sockets, persisted state, config and logs.

use std::path::{Path, PathBuf};

/// Longest Unix socket path both Linux (108 bytes) and macOS (104) accept, its final NUL aside.
const SOCKET_PATH_MAX: usize = 103;
/// Longest session name kraal accepts. Never lowered: a session named when it was allowed must
/// stay reachable by its name.
const SESSION_NAME_MAX: usize = 64;
/// Longest socket file name before `.sock`: a longer session name gets a shortened one.
const SOCKET_STEM_MAX: usize = 32;
/// Room for the runtime base directory: `<base>/kraal/<stem>.sock` must fit.
const RUNTIME_BASE_MAX: usize = SOCKET_PATH_MAX - "/kraal/".len() - SOCKET_STEM_MAX - ".sock".len();

/// Where sockets go when `$XDG_RUNTIME_DIR` is missing, too long or not safe: `kraal-<uid>` in
/// `$TMPDIR` when that is short and safe, else in /tmp (macOS's `$TMPDIR` is long). Other users
/// can write there, so [`private_runtime_dir`] checks it.
fn fallback_runtime_base() -> PathBuf {
    // SAFETY: getuid cannot fail.
    let name = format!("kraal-{}", unsafe { libc::getuid() });
    let tmp = checked_base(&std::env::temp_dir(), RUNTIME_BASE_MAX - 1 - name.len());
    // /tmp itself (a link owned by root on macOS); the directory in it is checked as created.
    tmp.unwrap_or_else(|| PathBuf::from("/tmp")).join(name)
}

/// `dir` as a path checked as it is returned ([`safe_chain`]), when it is short enough: links
/// resolved, or, when that is too long, as given (a short link of ours to a deep directory).
fn checked_base(dir: &Path, max_len: usize) -> Option<PathBuf> {
    let path = short_spelling(dir, max_len)?;
    (path.is_dir() && safe_chain(&path)).then_some(path)
}

/// `path` with its links resolved when that fits in `max_len` bytes, else as given when that
/// fits (an absolute path): the spelling to check, then use.
fn short_spelling(path: &Path, max_len: usize) -> Option<PathBuf> {
    let fits = |p: &Path| p.as_os_str().len() <= max_len;
    match path.canonicalize() {
        Ok(real) if fits(&real) => Some(real),
        Ok(_) if path.is_absolute() && fits(path) => Some(path.to_path_buf()),
        _ => None,
    }
}

/// Whether nobody else can swap `path`, or what is in it, read as it is spelt (never resolved to
/// another path first, which a swap between the two would defeat): from the root down, every
/// directory is owned by us or by root, and others cannot write to it, unless it is sticky like
/// /tmp (then they cannot move what is ours); a link on the way is ours or root's and leads to
/// a place that passes the same check. A socket under a directory another user controls could be
/// replaced by theirs.
fn safe_chain(path: &Path) -> bool {
    safe_chain_at(path, 0)
}

fn safe_chain_at(path: &Path, depth: usize) -> bool {
    use std::os::unix::fs::MetadataExt;
    use std::path::Component;
    if depth > 8 || !path.is_absolute() {
        return false;
    }
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let mut at = PathBuf::new();
    for part in path.components() {
        match part {
            Component::RootDir => at.push("/"),
            Component::Normal(name) => at.push(name),
            // `.` and `..` would go through links: each step of the path is named.
            _ => return false,
        }
        let Ok(m) = std::fs::symlink_metadata(&at) else { return false };
        let owner_ok = m.uid() == uid || m.uid() == 0;
        let ok = if m.file_type().is_symlink() {
            owner_ok && at.canonicalize().is_ok_and(|target| safe_chain_at(&target, depth + 1))
        } else {
            m.is_dir() && owner_ok && (m.mode() & 0o022 == 0 || m.mode() & 0o1000 != 0)
        };
        if !ok {
            return false;
        }
    }
    true
}

/// Directory holding one control socket per running session.
pub fn runtime_dir() -> PathBuf {
    runtime_dir_in(std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from))
}

fn runtime_dir_in(xdg: Option<PathBuf>) -> PathBuf {
    let base = xdg.and_then(|p| checked_base(&p, RUNTIME_BASE_MAX)).unwrap_or_else(fallback_runtime_base);
    base.join("kraal")
}

/// Why the sockets are not in `$XDG_RUNTIME_DIR` although it is set, and where they are: said
/// when a server is started (to who starts it, and in its log) and by `kraal config check`,
/// since whoever set it expects them there. `None` when it is used, or not set: the sockets
/// then go where they go without a word.
pub fn runtime_dir_refused() -> Option<String> {
    let xdg = std::env::var_os("XDG_RUNTIME_DIR").filter(|dir| !dir.is_empty()).map(PathBuf::from)?;
    let why = base_refused(&xdg, RUNTIME_BASE_MAX)?;
    Some(format!("XDG_RUNTIME_DIR ({}) is not used: {why}. Sockets are in {}", xdg.display(), runtime_dir().display()))
}

/// Why [`checked_base`] does not take `dir`, if it does not.
fn base_refused(dir: &Path, max_len: usize) -> Option<String> {
    if checked_base(dir, max_len).is_some() {
        return None;
    }
    Some(if !dir.is_dir() {
        "it is not a directory".into()
    } else if short_spelling(dir, max_len).is_none() {
        format!("it is {} bytes long, and a socket path leaves it {max_len}", dir.as_os_str().len())
    } else {
        "another user could replace it, or what is in it".into()
    })
}

/// Create `dir` (and missing parents) and make sure it is a real directory owned by us and
/// private (0700). Refuses symlinks and directories owned by someone else: a socket in such a
/// place could be read or replaced by another user.
pub fn ensure_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    std::fs::create_dir_all(dir)?;
    let meta = std::fs::symlink_metadata(dir)?;
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    if !meta.is_dir() || meta.uid() != uid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{} is not a directory owned by you; refusing to use it", dir.display()),
        ));
    }
    if meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// The runtime directory, made private ([`ensure_private_dir`]). Under the fallback base
/// (`kraal-<uid>` in a temporary directory every user can write to), that base is checked first:
/// another user who made it could otherwise swap the directory under it.
pub fn private_runtime_dir() -> std::io::Result<PathBuf> {
    let dir = runtime_dir();
    if dir.parent() == Some(fallback_runtime_base().as_path()) {
        ensure_private_dir(&fallback_runtime_base())?;
    }
    ensure_private_dir(&dir)?;
    Ok(dir)
}

/// Whether `name` is the temporary file of an atomic write ([`write_private`], [`write_atomic`])
/// whose process no longer runs: `.<file>.<pid>.<n>.tmp` ([`temp_beside`]), left by a kraal that
/// was killed while it wrote. A file a running process is writing never counts.
pub fn abandoned_temp(name: &str) -> bool {
    abandoned_temp_of(name).is_some()
}

/// The file whose write left `name` behind, when it is an [`abandoned_temp`].
fn abandoned_temp_of(name: &str) -> Option<&str> {
    let inner = name.strip_prefix('.')?.strip_suffix(".tmp")?;
    let mut parts = inner.rsplitn(3, '.');
    let (n, pid, file) = (parts.next()?, parts.next()?, parts.next()?);
    let number = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if file.is_empty() || !number(n) || !number(pid) {
        return None;
    }
    let pid = pid.parse::<libc::pid_t>().ok()?;
    // SAFETY: signal 0 only checks that the process exists; another user's says EPERM.
    let gone = pid > 0
        && pid as u32 != std::process::id()
        && unsafe { libc::kill(pid, 0) } != 0
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
    gone.then_some(file)
}

/// Remove the temporary files that writes of `path` abandoned (see [`abandoned_temp`]): they
/// may hold what `path` held.
pub fn remove_abandoned_temps(path: &Path) -> std::io::Result<()> {
    let (Some(dir), Some(file)) = (path.parent(), path.file_name()) else { return Ok(()) };
    let file = file.to_string_lossy();
    match remove_files_in(dir, |name| abandoned_temp_of(name) == Some(&*file)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        removed => removed.map(drop),
    }
}

/// The directories of kraal's own that its atomic writes go to.
pub fn written_dirs() -> Vec<PathBuf> {
    let state = state_dir();
    vec![
        state.join("sessions"),
        state.join("buffers"),
        state.join("runs"),
        state.join("usage"),
        state,
        workspaces_dir(),
        config_dir(),
    ]
}

/// A temporary path next to `path` that no other writer uses at the same time, in this process
/// (a counter) or another (the pid).
fn temp_beside(path: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    dir.join(format!(".{name}.{}.{}.tmp", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)))
}

/// Write `data` to `path` atomically, with these permissions: a fresh file next to it, renamed
/// over it. A reader sees the old or the new content, never a mix, and a symlink in its place
/// cannot redirect the data.
fn write_with_mode(path: &Path, data: &[u8], mode: u32) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = temp_beside(path);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    opts.mode(mode).custom_flags(libc::O_NOFOLLOW);
    let written = opts.open(&tmp).and_then(|mut f| {
        f.write_all(data)?;
        f.sync_all()
    });
    match written.and_then(|()| std::fs::rename(&tmp, path)) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Write a file only we can read (state can hold paths and commands), atomically.
pub fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    write_with_mode(path, data, 0o600)
}

/// Make the names created, renamed or removed in `dir` survive a crash of the machine, as far
/// as its file system allows (some cannot sync a directory).
pub fn sync_dir(dir: &Path) {
    if let Ok(d) = std::fs::File::open(dir) {
        let _ = d.sync_all();
    }
}

/// Remove the regular files of ours in `dir` whose names `remove` picks, through the directory
/// as opened: a link in its place (to a directory elsewhere) is refused, and so is a directory
/// that is not ours, and a link or directory in it is left alone. Returns how many entries it
/// left.
pub fn remove_files_in(dir: &Path, remove: impl Fn(&str) -> bool) -> std::io::Result<usize> {
    use std::ffi::CStr;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let d = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW).open(dir)?;
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    if d.metadata()?.uid() != uid {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("{} is not a directory owned by you", dir.display()),
        ));
    }
    let fd = d.as_raw_fd();
    let mut names = Vec::new();
    // SAFETY: the copy of the descriptor belongs to the stream, which closedir closes; each
    // entry is read before the next call to readdir.
    unsafe {
        let copy = libc::dup(fd);
        if copy < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let stream = libc::fdopendir(copy);
        if stream.is_null() {
            libc::close(copy);
            return Err(std::io::Error::last_os_error());
        }
        loop {
            let entry = libc::readdir(stream);
            if entry.is_null() {
                break;
            }
            names.push(CStr::from_ptr((*entry).d_name.as_ptr()).to_owned());
        }
        libc::closedir(stream);
    }
    let mut left = 0;
    for name in names {
        let text = name.to_string_lossy();
        if text == "." || text == ".." {
            continue;
        }
        // SAFETY: `st` is ours to fill; the name is looked up in `fd`, links not followed.
        let ours = unsafe {
            let mut st: libc::stat = std::mem::zeroed();
            libc::fstatat(fd, name.as_ptr(), &mut st, libc::AT_SYMLINK_NOFOLLOW) == 0
                && st.st_mode & libc::S_IFMT == libc::S_IFREG
                && st.st_uid == uid
        };
        // SAFETY: removes that name from `fd`, never a directory.
        let removed = ours && remove(&text) && unsafe { libc::unlinkat(fd, name.as_ptr(), 0) } == 0;
        if !removed {
            left += 1;
        }
    }
    Ok(left)
}

/// Write a file atomically, keeping the permissions it has (0644 when it is new): the user's
/// configuration files. A symlink (dotfiles kept in a repository) stays one: the file it
/// points to is replaced.
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let target = match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => std::fs::canonicalize(path).map_err(|e| {
            std::io::Error::new(e.kind(), format!("{} is a link to a file that is not there", path.display()))
        })?,
        _ => path.to_path_buf(),
    };
    let mode = std::fs::metadata(&target).map(|m| m.permissions().mode() & 0o7777).unwrap_or(0o644);
    write_with_mode(&target, data, mode)
}

/// The device and inode of the entry at `path` (a link's own, not its target's), which a rename
/// keeps.
pub fn file_id(path: &Path) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::symlink_metadata(path).ok().map(|m| (m.dev(), m.ino()))
}

/// rename(2) that fails with `AlreadyExists` instead of replacing `to`.
pub fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let from = CString::new(from.as_os_str().as_bytes())?;
    let to = CString::new(to.as_os_str().as_bytes())?;
    // The system call itself: musl has no renameat2 wrapper.
    // SAFETY: two NUL-terminated paths that live through the call.
    #[cfg(target_os = "linux")]
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            from.as_ptr(),
            libc::AT_FDCWD,
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    // SAFETY: as above.
    #[cfg(target_os = "macos")]
    let rc = unsafe { libc::renamex_np(from.as_ptr(), to.as_ptr(), libc::RENAME_EXCL) };
    if rc == 0 { Ok(()) } else { Err(std::io::Error::last_os_error()) }
}

/// Open `path` for appending, creating it private (0600): a log can hold secrets (a command, what
/// it printed). A link in its place is refused, and a file an older kraal made readable by
/// others is made private.
pub fn open_private_append(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut opts = std::fs::OpenOptions::new();
    opts.append(true).create(true).mode(0o600).custom_flags(libc::O_NOFOLLOW);
    let file = opts.open(path)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

/// Read a regular file of at most `cap` bytes (symlinks followed). Anything else is refused at
/// once: a FIFO would block the reader, a device like `/dev/zero` would never end, and a file
/// over `cap` is not what the caller expects.
pub fn read_regular(path: &Path, cap: u64) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    // Opening a FIFO without a writer would wait; without blocking, its type is checked first.
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut opts, libc::O_NONBLOCK);
    let file = opts.open(path)?;
    let meta = file.metadata()?;
    let refuse = |why: String| Err(std::io::Error::new(std::io::ErrorKind::InvalidData, why));
    if !meta.is_file() {
        return refuse(format!("{} is not a regular file", path.display()));
    }
    let mut bytes = Vec::new();
    file.take(cap.saturating_add(1)).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > cap {
        return refuse(format!("{} is larger than {} KiB", path.display(), cap >> 10));
    }
    Ok(bytes)
}

/// [`read_regular`] as text.
pub fn read_regular_text(path: &Path, cap: u64) -> std::io::Result<String> {
    String::from_utf8(read_regular(path, cap)?).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{} is not UTF-8 text", path.display()))
    })
}

/// Run `f` holding an exclusive lock tied to `path` (a `.<name>.lock` file next to it), so that
/// kraal processes updating the same file (load, change, save) never lose each other's changes.
pub fn locked<T>(path: &Path, f: impl FnOnce() -> T) -> std::io::Result<T> {
    // Another kraal holds it for a moment at most; never wait for good.
    locked_within(path, std::time::Duration::from_secs(3), f)
}

/// [`locked`], waiting up to `limit` for whoever holds the lock: as long as what it does there
/// may take. The lock keeps threads of one process apart as well as processes.
pub fn locked_within<T>(path: &Path, limit: std::time::Duration, f: impl FnOnce() -> T) -> std::io::Result<T> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let lock =
        std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(format!(".{name}.lock")))?;
    let deadline = std::time::Instant::now() + limit;
    while !try_lock(&lock)? {
        if std::time::Instant::now() > deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("another kraal is changing {}; try again", path.display()),
            ));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let out = f();
    drop(lock);
    Ok(out)
}

/// Lock `file` for this process alone, without waiting: false when another holds it. The lock
/// goes when the file is closed, by this process or as it ends, however it ends.
fn try_lock(file: &std::fs::File) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;
    loop {
        // SAFETY: flock on a descriptor we own.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
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
/// kept out in that directory, and nowhere else. Kraal 0.2.0 takes none ([`listen_alone`]).
///
/// The state directory's lock file goes with its holder (every session name ever used would
/// leave one otherwise): only a kraal that looks at the file again once it has locked it takes
/// it. The runtime directory's stays: kraal 0.3.0 does not look again, and would hold a file
/// that is gone beside the holder of the one made since. That directory goes with the login.
pub struct SessionLock {
    _state: LockFile,
    /// `None` when it is the state directory's file: both directories are one.
    _runtime: Option<LockFile>,
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

    fn acquire_at(
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

/// What the holder of `session` says of itself, if it says so in a file of ours. What a holder
/// killed before said is there until the next one says its own.
pub fn holder(session: &str) -> Option<Holder> {
    holder_at(&state_dir().join(format!("{session}.lock")))
}

fn holder_at(lock: &Path) -> Option<Holder> {
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
struct LockFile {
    /// Locked while open.
    file: std::fs::File,
    path: PathBuf,
    /// Whether the file is left in place when its holder goes.
    stays: bool,
}

impl LockFile {
    /// Hold the lock file `path`, made if needed, unless another process does (`None`).
    fn acquire(path: PathBuf, stays: bool, lock: Locker) -> std::io::Result<Option<LockFile>> {
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
fn is_file_at(file: &std::fs::File, path: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    file.metadata().is_ok_and(|open| file_id(path) == Some((open.dev(), open.ino())))
}

/// The socket of `session`: where its server listens, or a new one will. A server an older kraal
/// started may listen elsewhere: in the directory it used ([`legacy_runtime_dir`]), or in this
/// one under the session's whole name, which older kraals kept up to Linux's limit where kraal
/// now shortens it. While nothing is at the usual place and one of those answers, it is the one,
/// so the session stays reachable (and is not started twice) until it is restarted.
pub fn socket_path(session: &str) -> PathBuf {
    socket_path_with(session, std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from), std::env::temp_dir())
}

fn socket_path_with(session: &str, xdg: Option<PathBuf>, tmp: PathBuf) -> PathBuf {
    let dir = runtime_dir_in(xdg.clone());
    let name = format!("{session}.sock");
    let older = vec![dir.join(&name), legacy_runtime_dir_in(xdg, tmp).join(&name)];
    reachable(socket_path_in(&dir, session), older)
}

/// `current` unless it is missing and one of `older` is a live socket of ours, checked as the
/// runtime directory is: then that socket, through the directory path that was checked.
fn reachable(current: PathBuf, older: Vec<PathBuf>) -> PathBuf {
    if current.exists() {
        return current;
    }
    older
        .iter()
        .filter(|old| **old != current)
        .filter_map(|old| checked_socket(old))
        .find(|old| !nobody_listens(old))
        .unwrap_or(current)
}

/// Whether nobody listens at `socket`: nothing is there, or what is there refuses a connection.
/// A server that takes none for now is somebody: no other is started beside it, and what its
/// session saved is not forgotten.
pub fn nobody_listens(socket: &Path) -> bool {
    use std::io::ErrorKind;
    connect_now(socket).is_err_and(|e| matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused))
}

/// Connect to `socket` without waiting. A server that takes no connection now (it is stopped, or
/// far behind: its queue is full) is `WouldBlock` on Linux, where a plain connect would wait for
/// it without end. macOS refuses such a connection as it does one to a socket nobody listens
/// at: the two cannot be told apart there.
pub fn connect_now(socket: &Path) -> std::io::Result<std::os::unix::net::UnixStream> {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    let name = socket.as_os_str().as_bytes();
    // SAFETY: plain C data: all zero bytes are a valid value.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if name.len() >= addr.sun_path.len() || name.contains(&0) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a socket path"));
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (to, from) in addr.sun_path.iter_mut().zip(name) {
        *to = *from as libc::c_char;
    }
    // SAFETY: plain syscall.
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: a descriptor just made, owned by nothing else.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: flags of a descriptor we own.
    unsafe {
        libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
        libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
    }
    let len = std::mem::size_of::<libc::sockaddr_un>() as libc::socklen_t;
    // SAFETY: `addr` is a whole sockaddr_un, with a path that ends in a zero byte.
    while unsafe { libc::connect(fd.as_raw_fd(), (&raw const addr).cast(), len) } != 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() != std::io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
    let stream = std::os::unix::net::UnixStream::from(fd);
    stream.set_nonblocking(false)?;
    Ok(stream)
}

/// Listen at `socket`, where no server does: `None` when one listens there, or takes no
/// connection for now. A socket that refuses is removed first ([`remove_stale`]). Whoever calls
/// this holds the session, so no kraal that takes its lock listens there. What else may is a
/// server of kraal 0.2.0, which takes none: nothing here keeps such a one out for certain. One
/// that has bound its socket and does not listen yet refuses like a socket left behind, and so
/// does, on macOS, one whose queue is full.
pub fn listen_alone(socket: &Path) -> std::io::Result<Option<std::os::unix::net::UnixListener>> {
    use std::io::ErrorKind;
    // A lockless server that takes the place again and again is not raced with for ever.
    for _ in 0..5 {
        match std::os::unix::net::UnixListener::bind(socket) {
            Ok(listener) => return Ok(Some(listener)),
            Err(e) if e.kind() == ErrorKind::AddrInUse => {}
            Err(e) => return Err(e),
        }
        let there = file_id(socket);
        match connect_now(socket) {
            Err(e) if e.kind() == ErrorKind::ConnectionRefused => remove_stale(socket, there)?,
            // Gone since: the place is free.
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            // It answers, or will once it takes connections again.
            _ => return Ok(None),
        }
    }
    Err(std::io::Error::new(ErrorKind::AddrInUse, "something keeps taking that place"))
}

/// Remove the socket at `socket` that refused a connection: the file `stale`, which was there
/// before it did. It is moved aside and looked at before it is deleted: when another file took
/// the place between the refusal and the move (a kraal 0.2.0, which holds no lock, started
/// then), that one is put back instead. This narrows what a check followed by a removal leaves
/// open, and does not close it: yet another server may take the place while the file is aside,
/// and the file then stays aside.
fn remove_stale(socket: &Path, stale: Option<(u64, u64)>) -> std::io::Result<()> {
    let aside = socket.with_extension(format!("stale.{}", std::process::id()));
    match std::fs::rename(socket, &aside) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        moved => moved?,
    }
    if stale.is_some() && file_id(&aside) == stale {
        return std::fs::remove_file(&aside);
    }
    // Back under its name, unless a third server has taken it: the file then stays aside,
    // since nothing says that nobody listens at it.
    match rename_no_replace(&aside, socket) {
        Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => Err(e),
        _ => Ok(()),
    }
}

/// `socket` spelt as it was checked: links resolved, or as given when that alone fits a socket
/// path (an older kraal's runtime directory reached through a short link of ours); every
/// directory above it safe from other users ([`safe_chain`]), the last one ours and private, the
/// socket ours. A socket anywhere else could be another user's, standing in for a session, or be
/// swapped after the check through a directory they control.
fn checked_socket(socket: &Path) -> Option<PathBuf> {
    let name = socket.file_name()?;
    let dir = short_spelling(socket.parent()?, LINUX_SOCKET_PATH_MAX.saturating_sub(1 + name.len()))?;
    let socket = dir.join(name);
    (safe_chain(&dir) && ours_and_private(&socket)).then_some(socket)
}

/// Longest socket path Linux accepts (108 bytes with the final NUL): the limit the builds before
/// 0.1.0 kept to, before kraal kept to macOS's.
const LINUX_SOCKET_PATH_MAX: usize = 107;

/// Where the builds before 0.1.0 (up to 2026-10-11) put their sockets: `$XDG_RUNTIME_DIR` when it
/// was at most 60 bytes, else `$TMPDIR/kraal-<uid>`, without the checks made since, and each
/// session under its own name. `None` when that is where they go now; a socket there may still
/// have a name kraal now shortens, which [`socket_path`] looks for. No release used this place:
/// it serves sessions started by those builds only, and can go in any release.
pub fn legacy_runtime_dir() -> Option<PathBuf> {
    let old = legacy_runtime_dir_in(std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from), std::env::temp_dir());
    (old != runtime_dir()).then_some(old)
}

fn legacy_runtime_dir_in(xdg: Option<PathBuf>, tmp: PathBuf) -> PathBuf {
    // SAFETY: getuid cannot fail.
    let fallback = tmp.join(format!("kraal-{}", unsafe { libc::getuid() }));
    let base = xdg.filter(|p| p.is_dir() && p.as_os_str().len() <= 60).unwrap_or(fallback);
    base.join("kraal")
}

/// Whether `socket` is ours, in a directory of ours only we can enter: a socket elsewhere could
/// be another user's, standing in for a session.
fn ours_and_private(socket: &Path) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    // SAFETY: getuid cannot fail.
    let uid = unsafe { libc::getuid() };
    let dir_ok = socket
        .parent()
        .and_then(|d| std::fs::symlink_metadata(d).ok())
        .is_some_and(|m| m.is_dir() && m.uid() == uid && m.mode() & 0o077 == 0);
    dir_ok && std::fs::symlink_metadata(socket).is_ok_and(|m| m.file_type().is_socket() && m.uid() == uid)
}

/// `<dir>/<session>.sock`, or, when that is too long for a socket, a shorter name: the start of
/// the session's, `+` (never in a session name, so it names no other session) and a digest of
/// the whole.
fn socket_path_in(dir: &Path, session: &str) -> PathBuf {
    let plain = dir.join(format!("{session}.sock"));
    if plain.as_os_str().len() <= SOCKET_PATH_MAX {
        return plain;
    }
    let start: String = session.chars().take(SOCKET_STEM_MAX - 17).collect();
    dir.join(format!("{start}+{:016x}.sock", crate::text::fnv64(session)))
}

/// The session a socket file is for: its name, or, for a shortened one, the saved session it
/// belongs to (`None` when none is saved yet).
pub fn session_of_socket(stem: &str) -> Option<String> {
    if !stem.contains('+') {
        return Some(stem.to_string());
    }
    let dir = runtime_dir();
    let saved = std::fs::read_dir(state_dir().join("sessions")).ok()?;
    saved
        .flatten()
        .filter_map(|e| Some(e.path().file_stem()?.to_string_lossy().into_owned()))
        .find(|name| socket_path_in(&dir, name).file_stem().is_some_and(|s| s.to_string_lossy() == stem))
}

/// Bytes a shared SSH connection's socket name takes after its directory: `/ssh-` and kraal's
/// 16-character key (see `servers::ssh_args`), `-`, ssh's 40-character `%C`, and the
/// `.<16 random characters>` ssh adds while it sets the connection up.
const SSH_SOCKET_NAME: usize = "/ssh-".len() + 16 + 1 + 40 + 17;

/// Directory for the sockets of shared SSH connections: private (a socket another user could
/// replace would see every command), and short enough for ssh's socket names. `None` when no
/// such directory can be had: connections are then not shared.
pub fn ssh_control_dir() -> Option<PathBuf> {
    let dir = runtime_dir();
    if dir.as_os_str().len() + SSH_SOCKET_NAME <= SOCKET_PATH_MAX {
        return private_runtime_dir().ok();
    }
    // Directly in /tmp, whose sticky bit keeps others from moving it once it is ours.
    // SAFETY: getuid cannot fail.
    let short = PathBuf::from(format!("/tmp/kraal-{}", unsafe { libc::getuid() }));
    ensure_private_dir(&short).ok()?;
    Some(short)
}

/// An XDG base directory: `$var` when it is an absolute path, else `~/<fallback>`. The same on
/// macOS as on Linux: terminal tools keep their files there, not in `~/Library`.
fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| dirs::home_dir().map(|h| h.join(fallback)))
        .unwrap_or_else(std::env::temp_dir)
}

/// `~/.local/state/kraal`
pub fn state_dir() -> PathBuf {
    xdg_dir("XDG_STATE_HOME", ".local/state").join("kraal")
}

/// `~/.local/share`
pub fn data_home() -> PathBuf {
    xdg_dir("XDG_DATA_HOME", ".local/share")
}

/// The saved state of `session` (layout, panes, what each runs), which a restart restores.
pub fn session_state_path(session: &str) -> PathBuf {
    state_dir().join("sessions").join(format!("{session}.json"))
}

/// Where the panes' saved scrollback of `session` goes, one file per pane.
pub fn scrollback_dir(session: &str) -> PathBuf {
    state_dir().join("scrollback").join(session)
}

/// Where the paste buffers of `session` are kept across restarts, when they are (`[buffers]
/// persist`).
pub fn buffers_path(session: &str) -> PathBuf {
    state_dir().join("buffers").join(format!("{session}.json"))
}

/// The log of `session`'s server, which runs detached from any terminal: its standard output and
/// error, and those of a restart run on its own ([`crate::client::restart_detached`]).
pub fn log_path(session: &str) -> PathBuf {
    state_dir().join(format!("server-{session}.log"))
}

/// `~/.config/kraal`
pub fn config_dir() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config").join("kraal")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn workspaces_dir() -> PathBuf {
    config_dir().join("workspaces")
}

/// Expand a leading `~` or `~/` to the home directory. `~user` is left as it is.
pub fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

/// Replace the home directory prefix with `~` for display.
pub fn shorten_home(path: &Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        if rest.as_os_str().is_empty() {
            return "~".into();
        }
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

/// What this kraal would use of the developer's while a test runs it, if anything. Debug builds
/// only: the tests run kraal in directories of their own and name in `KRAAL_TEST_KEEP_OUT`, as
/// `PATH` names its directories, the developer's home and where their sessions are. A test that
/// lets one through (as a home, a directory of kraal's, an agent's home, a directory of `PATH`)
/// fails on this, where it would have read their accounts, run their agents or reached their
/// session.
#[cfg(debug_assertions)]
pub fn of_the_developer() -> Option<String> {
    let keep_out = std::env::var_os("KRAAL_TEST_KEEP_OUT")?;
    let followed = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let own = [
        ("HOME", dirs::home_dir()),
        ("the configuration directory", Some(config_dir())),
        ("the state directory", Some(state_dir())),
        ("the data directory", Some(data_home())),
        ("the runtime directory", Some(runtime_dir())),
    ];
    let agents = crate::accounts::AGENTS.iter().map(|a| (a.home_env, std::env::var_os(a.home_env).map(PathBuf::from)));
    let path = std::env::var_os("PATH").unwrap_or_default();
    let used: Vec<(&str, PathBuf)> = own
        .into_iter()
        .chain(agents)
        .filter_map(|(what, dir)| Some((what, dir?)))
        .chain(std::env::split_paths(&path).map(|dir| ("PATH", dir)))
        .collect();
    std::env::split_paths(&keep_out).find_map(|theirs| {
        let inside = |dir: &Path| dir.starts_with(&theirs) || followed(dir).starts_with(followed(&theirs));
        let (what, dir) = used.iter().find(|(_, dir)| inside(dir))?;
        Some(format!("{what} ({}) is the developer's: the tests stay out of {}", dir.display(), theirs.display()))
    })
}

/// Session names end up in file names; keep them tame and reasonably short.
pub fn valid_session_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= SESSION_NAME_MAX
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.starts_with('.')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_valid_session_has_a_socket_path_that_fits() {
        // Names as long as kraal ever accepted stay valid.
        let longest = "s".repeat(SESSION_NAME_MAX);
        assert!(valid_session_name(&longest) && !valid_session_name(&format!("{longest}s")));
        let long_dir = std::env::temp_dir().join("a".repeat(70));
        std::fs::create_dir_all(&long_dir).unwrap();
        let base = std::env::temp_dir().join(format!("kraal-base-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        for xdg in [None, Some(PathBuf::from("/run/user/1000")), Some(long_dir.clone()), Some(base.clone())] {
            let dir = runtime_dir_in(xdg.clone());
            for name in [longest.clone(), format!("{}t", "s".repeat(SESSION_NAME_MAX - 1)), "main".into()] {
                let socket = socket_path_in(&dir, &name);
                assert!(socket.as_os_str().len() <= SOCKET_PATH_MAX, "{xdg:?}: {}", socket.display());
            }
            // Two long names that start alike get two sockets.
            assert_ne!(
                socket_path_in(&dir, &longest),
                socket_path_in(&dir, &format!("{}t", "s".repeat(SESSION_NAME_MAX - 1)))
            );
            assert_eq!(socket_path_in(&dir, "main"), dir.join("main.sock"));
        }
        // A runtime directory too long for sockets is not used.
        assert!(!runtime_dir_in(Some(long_dir.clone())).starts_with(&long_dir));
        let _ = std::fs::remove_dir(long_dir);
        let _ = std::fs::remove_dir(base);
        assert!(fallback_runtime_base().as_os_str().len() <= RUNTIME_BASE_MAX);
    }

    /// The server of an older kraal that takes no connection for now is found as one that
    /// answers is: no other is started beside it at the usual place, and its session is not
    /// forgotten. Linux tells such a server from a socket nobody listens at.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_server_of_an_older_kraal_that_takes_no_connection_is_still_found() {
        use std::os::fd::AsRawFd;
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("kraal-legacy-full-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (now, old) = (root.join("now"), root.join("old"));
        for d in [&now, &old] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let root = root.canonicalize().unwrap();
        let (current, legacy) = (root.join("now/work.sock"), root.join("old/work.sock"));
        let server = std::os::unix::net::UnixListener::bind(&legacy).unwrap();
        // SAFETY: plain syscall on a socket this test holds: its queue takes one connection.
        assert_eq!(unsafe { libc::listen(server.as_raw_fd(), 0) }, 0);
        let _waiting = connect_now(&legacy).unwrap();
        assert_eq!(connect_now(&legacy).unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
        assert!(!nobody_listens(&legacy));
        assert_eq!(reachable(current.clone(), vec![legacy.clone()]), legacy);
        // So a server that would start for the session finds that one, and starts none.
        assert!(listen_alone(&reachable(current.clone(), vec![legacy.clone()])).unwrap().is_none());
        assert!(!current.exists());
        // Gone, its socket refuses: the usual place again.
        drop((server, _waiting));
        until_nobody_listens(&legacy);
        assert_eq!(reachable(current.clone(), vec![legacy]), current);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_session_an_older_kraal_started_stays_reachable() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("kraal-legacy-{}", std::process::id()));
        let (now, old) = (root.join("now"), root.join("old"));
        for d in [&now, &old] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let root = root.canonicalize().unwrap();
        let (now, old) = (root.join("now"), root.join("old"));
        let (current, legacy) = (now.join("work.sock"), old.join("work.sock"));
        // Nothing listens at the older place: the usual one.
        assert_eq!(reachable(current.clone(), vec![legacy.clone()]), current);
        // An older server listens there, nothing here: that one.
        let server = std::os::unix::net::UnixListener::bind(&legacy).unwrap();
        assert_eq!(reachable(current.clone(), vec![legacy.clone()]), legacy);
        // Not in a private directory: maybe not ours, so not used.
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(reachable(current.clone(), vec![legacy.clone()]), current);
        std::fs::set_permissions(&old, std::fs::Permissions::from_mode(0o700)).unwrap();
        // A directory above it others could swap (writable by all, not sticky): not used either,
        // though the socket and its own directory are ours.
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(reachable(current.clone(), vec![legacy.clone()]), current);
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Reached through a link, the socket used is the one checked.
        let alias = root.join("alias");
        std::os::unix::fs::symlink(&old, &alias).unwrap();
        assert_eq!(reachable(current.clone(), vec![alias.join("work.sock")]), legacy);
        // A server at the usual place wins.
        let _new = std::os::unix::net::UnixListener::bind(&current).unwrap();
        assert_eq!(reachable(current.clone(), vec![legacy.clone()]), current);
        drop(server);
        // Before: an XDG directory of up to 60 bytes, else $TMPDIR/kraal-<uid>; now 59 at most.
        if root.as_os_str().len() < 59 {
            let xdg60 = root.join("x".repeat(60 - root.as_os_str().len() - 1));
            std::fs::create_dir_all(&xdg60).unwrap();
            assert_eq!(legacy_runtime_dir_in(Some(xdg60.clone()), root.clone()), xdg60.join("kraal"));
            assert_ne!(runtime_dir_in(Some(xdg60.clone())), xdg60.join("kraal"));
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_path_is_checked_as_it_is_used_and_a_short_link_of_ours_may_stand_for_a_deep_one() {
        use std::os::unix::fs::PermissionsExt;
        let private = |d: &Path| std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Under /tmp: the sockets must fit, and macOS's temporary directory is long.
        let root = PathBuf::from("/tmp").join(format!("kraal-spelt-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        private(&root);
        let root = root.canonicalize().unwrap();
        // A directory others may write to (not sticky): what is in it can be swapped.
        let open = root.join("open");
        std::fs::create_dir_all(&open).unwrap();
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o777)).unwrap();
        // An older kraal's runtime directory, deep enough that its sockets only fit through a
        // short link to it.
        let deep = root.join("deep").join("d".repeat(100)).join("kraal");
        std::fs::create_dir_all(&deep).unwrap();
        for d in deep.ancestors().take_while(|d| d.starts_with(&root)) {
            private(d);
        }
        let ours = root.join("r");
        std::os::unix::fs::symlink(deep.parent().unwrap(), &ours).unwrap();
        let theirs = open.join("r");
        std::os::unix::fs::symlink(deep.parent().unwrap(), &theirs).unwrap();
        let socket = ours.join("kraal/work.sock");
        assert!(deep.join("work.sock").as_os_str().len() > LINUX_SOCKET_PATH_MAX && socket.as_os_str().len() < 80);
        let _older = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let current = root.join("now/work.sock");
        // Through our own short link, in a directory only we can change: reached that way.
        assert_eq!(reachable(current.clone(), vec![socket.clone()]), socket);
        // Through a link in a directory anyone may write to: whoever swaps it would stand in for
        // the session. The path is checked as spelt, not as where it leads today.
        assert!(!safe_chain(&theirs));
        assert_eq!(reachable(current.clone(), vec![theirs.join("kraal/work.sock")]), current);
        // Where the resolved path fits, it is the one checked and the one used.
        let near = root.join("near");
        std::fs::create_dir_all(near.join("kraal")).unwrap();
        private(&near);
        private(&near.join("kraal"));
        let _near_server = std::os::unix::net::UnixListener::bind(near.join("kraal/work.sock")).unwrap();
        let swappable = open.join("near");
        std::os::unix::fs::symlink(&near, &swappable).unwrap();
        assert_eq!(reachable(current.clone(), vec![swappable.join("kraal/work.sock")]), near.join("kraal/work.sock"));
        let _ = std::fs::remove_dir_all(root);
    }

    /// Linux only: the paths of older kraal sockets went up to Linux's limit.
    #[cfg(target_os = "linux")]
    #[test]
    fn every_socket_a_released_kraal_made_is_found() {
        use std::os::unix::fs::PermissionsExt;
        // kraal up to the morning of 2026-10-11 put `<session>.sock` in `<base>/kraal`, `<base>`
        // being $XDG_RUNTIME_DIR as spelt when it was a directory of at most 60 bytes, else
        // $TMPDIR/kraal-<uid>. Each case listens where that kraal did.
        let root = std::env::temp_dir().join(format!("kraal-released-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        let private = |d: &Path| {
            std::fs::create_dir_all(d).unwrap();
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700)).unwrap();
        };
        private(&root);
        let session = format!("released-{}", std::process::id());
        let check = |xdg: Option<PathBuf>, tmp: &Path, released_base: &Path| {
            private(released_base);
            private(&released_base.join("kraal"));
            let socket = released_base.join("kraal").join(format!("{session}.sock"));
            let _server = std::os::unix::net::UnixListener::bind(&socket).unwrap();
            let found = socket_path_with(&session, xdg.clone(), tmp.to_path_buf());
            assert_eq!(found.canonicalize().unwrap(), socket.canonicalize().unwrap(), "{xdg:?}");
            let _ = std::fs::remove_file(&socket);
        };
        // A short link of ours to a deep runtime directory: used as spelt.
        let deep = root.join("deep").join("d".repeat(70));
        private(&deep);
        for d in deep.ancestors().take_while(|d| d.starts_with(&root)) {
            private(d);
        }
        let alias = root.join("r");
        std::os::unix::fs::symlink(&deep, &alias).unwrap();
        check(Some(alias.clone()), &root, &alias);
        // A runtime directory of exactly 60 bytes, which kraal no longer uses itself.
        if root.as_os_str().len() < 58 {
            let xdg60 = root.join("x".repeat(60 - root.as_os_str().len() - 1));
            check(Some(xdg60.clone()), &root, &xdg60);
        }
        // No runtime directory, or one too long: $TMPDIR/kraal-<uid>.
        // SAFETY: getuid cannot fail.
        let fallback = root.join(format!("kraal-{}", unsafe { libc::getuid() }));
        check(None, &root, &fallback);
        let long = root.join("long").join("l".repeat(70));
        private(&long);
        check(Some(long), &root, &fallback);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_long_session_name_an_older_kraal_used_stays_reachable() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("kraal-longname-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        // A directory where a 64-character name made a socket path of 104 to 107 bytes: Linux
        // took it, kraal now shortens the name to keep to macOS's limit.
        let name = "s".repeat(SESSION_NAME_MAX);
        let pad = (SOCKET_PATH_MAX + 2).saturating_sub(root.as_os_str().len() + 2 + name.len() + ".sock".len());
        let dir = root.join("d".repeat(pad.max(1)));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        let plain = dir.join(format!("{name}.sock"));
        let fits_linux_only =
            plain.as_os_str().len() > SOCKET_PATH_MAX && plain.as_os_str().len() <= LINUX_SOCKET_PATH_MAX;
        assert!(fits_linux_only || root.as_os_str().len() > 30, "{}", plain.display());
        if fits_linux_only {
            let current = socket_path_in(&dir, &name);
            assert_ne!(current, plain);
            let _older = std::os::unix::net::UnixListener::bind(&plain).unwrap();
            assert_eq!(reachable(current, vec![plain.clone()]), plain);
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_runtime_directory_others_could_swap_is_not_used() {
        use std::os::unix::fs::PermissionsExt;
        let root = PathBuf::from("/tmp").join(format!("kraal-swap-{}", std::process::id()));
        let shared = root.join("shared");
        let xdg = shared.join("xdg");
        std::fs::create_dir_all(&xdg).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&xdg, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(safe_chain(&xdg));
        let real = xdg.canonicalize().unwrap();
        assert_eq!(runtime_dir_in(Some(xdg.clone())), real.join("kraal"));
        // Through a link, the directory used is the one checked, not the link: whoever owns the
        // link could point it elsewhere afterwards.
        let alias = root.join("alias");
        std::os::unix::fs::symlink(&xdg, &alias).unwrap();
        assert_eq!(runtime_dir_in(Some(alias.clone())), real.join("kraal"));
        // Anyone may write in its parent, which is not sticky: anyone could move it away.
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(!safe_chain(&xdg));
        assert!(!runtime_dir_in(Some(xdg.clone())).starts_with(&xdg));
        // Sticky, like /tmp: what is ours stays ours.
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();
        assert!(safe_chain(&xdg));
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o700)).unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn only_small_regular_files_are_read() {
        let dir = std::env::temp_dir().join(format!("kraal-regular-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("ok.toml");
        std::fs::write(&file, "name = 1\n").unwrap();
        assert_eq!(read_regular_text(&file, 1024).unwrap(), "name = 1\n");
        assert!(read_regular(&file, 4).is_err(), "over the cap");
        // A link to a device that never ends, and a FIFO nobody writes to: refused at once.
        let zero = dir.join("zero.toml");
        let _ = std::fs::remove_file(&zero);
        std::os::unix::fs::symlink("/dev/zero", &zero).unwrap();
        assert!(read_regular(&zero, 1 << 20).is_err());
        let fifo = dir.join("fifo.toml");
        let _ = std::fs::remove_file(&fifo);
        let c = std::ffi::CString::new(fifo.display().to_string()).unwrap();
        // SAFETY: plain syscall on a path we own.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        let start = std::time::Instant::now();
        assert!(read_regular(&fifo, 1 << 20).is_err());
        assert!(start.elapsed() < std::time::Duration::from_secs(1));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn private_writes_replace_links_and_loose_files() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("kraal-private-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("state.json");
        std::fs::write(&target, "old").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_private(&target, b"new").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
        assert_eq!(std::fs::metadata(&target).unwrap().permissions().mode() & 0o777, 0o600);
        let elsewhere = dir.join("elsewhere");
        std::fs::write(&elsewhere, "keep").unwrap();
        let link = dir.join("link.json");
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
        write_private(&link, b"data").unwrap();
        assert_eq!(std::fs::read_to_string(&elsewhere).unwrap(), "keep", "the link target is untouched");
        assert!(!std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn atomic_writes_keep_permissions_and_locked_updates_lose_nothing() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("kraal-atomic-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("servers.toml");
        write_atomic(&file, b"a").unwrap();
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o644);
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        write_atomic(&file, b"b").unwrap();
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        // Concurrent read-modify-write: every increment survives.
        let counter = dir.join("counter");
        std::fs::write(&counter, "0").unwrap();
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let counter = counter.clone();
                std::thread::spawn(move || {
                    for _ in 0..20 {
                        locked(&counter, || {
                            let n: u32 = std::fs::read_to_string(&counter).unwrap().parse().unwrap();
                            write_atomic(&counter, (n + 1).to_string().as_bytes()).unwrap();
                        })
                        .unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(std::fs::read_to_string(&counter).unwrap(), "160");
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(left.is_empty(), "no temporary file left behind");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// What `take` gives once nobody holds what it locks. A lock file that stays is, for a
    /// moment after its holder let go, still locked by a program another test is starting: that
    /// one has this process's files until it runs.
    fn once_free<T>(take: impl Fn() -> std::io::Result<Option<T>>) -> T {
        let start = std::time::Instant::now();
        loop {
            if let Some(held) = take().unwrap() {
                return held;
            }
            assert!(start.elapsed() < crate::testing::PATIENCE, "never let go");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// Wait until nobody listens at `socket` any more: a listener this test closed is, for the
    /// same moment, still open in a program another test is starting.
    fn until_nobody_listens(socket: &Path) {
        let start = std::time::Instant::now();
        while !nobody_listens(socket) {
            assert!(start.elapsed() < crate::testing::PATIENCE, "still listened at");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    #[test]
    fn a_session_lock_goes_with_its_holder_and_no_other_file_does() {
        let dir = std::env::temp_dir().join(format!("kraal-lock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("main.lock");
        let held = LockFile::acquire(path.clone(), false, try_lock).unwrap().expect("nobody holds it");
        // Whoever else asks is refused, and leaves the holder its file.
        assert!(LockFile::acquire(path.clone(), false, try_lock).unwrap().is_none());
        assert!(is_file_at(&held.file, &path));
        drop(held);
        assert!(!path.exists(), "the lock file was left");
        // The file a killed holder left is taken, then goes too.
        std::fs::write(&path, "").unwrap();
        drop(LockFile::acquire(path.clone(), false, try_lock).unwrap().expect("nobody holds it"));
        assert!(!path.exists(), "the file of a killed holder was left");
        // A file put there while this one was held (this one removed by a cleaner of /tmp) is
        // the lock of whoever made it.
        let held = LockFile::acquire(path.clone(), false, try_lock).unwrap().unwrap();
        std::fs::remove_file(&path).unwrap();
        let other = LockFile::acquire(path.clone(), false, try_lock).unwrap().expect("the file there is free");
        drop(held);
        assert!(is_file_at(&other.file, &path), "the file of another holder was removed");
        drop(other);
        assert!(!path.exists());
        // One that stays (the runtime directory's, which kraal 0.3.0 takes without looking at it
        // again) is the same file for every holder.
        let held = LockFile::acquire(path.clone(), true, try_lock).unwrap().unwrap();
        let file = file_id(&path);
        drop(held);
        assert!(file.is_some() && file_id(&path) == file, "the lock file went with its holder");
        drop(once_free(|| LockFile::acquire(path.clone(), true, try_lock)));
        assert_eq!(file_id(&path), file);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Where [`a_contender_for_the_session_lock`] contends, when a test started it for that.
    const CONTEND_IN: &str = "KRAAL_TEST_CONTEND_IN";

    /// Taken and given back by many processes at once, over and over, a session is never held
    /// by two, although its file goes with each holder, and nothing is left of it.
    #[test]
    fn a_session_lock_taken_by_many_processes_is_held_by_one_at_a_time() {
        let dir = std::env::temp_dir().join(format!("kraal-lock-race-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // This test binary again, for the one test that contends.
        let contenders: Vec<std::process::Child> = (0..8)
            .map(|_| {
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", "paths::tests::a_contender_for_the_session_lock", "--ignored", "--nocapture"])
                    .env(CONTEND_IN, &dir)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        // They start together, however long each took to get there.
        std::fs::write(dir.join("go"), "").unwrap();
        for contender in contenders {
            let out = contender.wait_with_output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        }
        assert!(!dir.join("main.lock").exists(), "a lock file was left");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// One of the processes of the test above: it takes the lock and gives it back many times,
    /// and checks each time that it holds it alone.
    #[test]
    #[ignore = "started by a_session_lock_taken_by_many_processes_is_held_by_one_at_a_time"]
    fn a_contender_for_the_session_lock() {
        let Some(dir) = std::env::var_os(CONTEND_IN).map(PathBuf::from) else { return };
        let (lock, alone) = (dir.join("main.lock"), dir.join("alone"));
        let start = std::time::Instant::now();
        while !dir.join("go").exists() {
            assert!(start.elapsed() < std::time::Duration::from_secs(60), "never told to start");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        for _ in 0..5000 {
            let Some(held) = LockFile::acquire(lock.clone(), false, try_lock).unwrap() else { continue };
            // A file only one process can make: another holder would have made it.
            let made = std::fs::OpenOptions::new().write(true).create_new(true).open(&alone);
            assert!(made.is_ok(), "held by two at once");
            std::thread::yield_now();
            std::fs::remove_file(&alone).unwrap();
            drop(held);
        }
    }

    /// A session is held in the state directory: a kraal with another runtime directory does
    /// not hold it too, and reads who does. The state directory's lock file goes with its
    /// holder; the runtime directory's stays, for kraal 0.3.0.
    #[test]
    fn a_session_is_held_in_the_state_directory_whatever_the_runtime_one() {
        let dir = std::env::temp_dir().join(format!("kraal-held-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (state, here, there) = (dir.join("state"), dir.join("here"), dir.join("there"));
        for d in [&state, &here, &there] {
            std::fs::create_dir_all(d).unwrap();
        }
        let lock = |d: &Path| d.join("main.lock");
        let holder = |runtime: &Path| Holder { pid: 4242, host: "box".into(), runtime: runtime.to_path_buf() };
        let hold = |runtime: &Path| SessionLock::acquire_at(lock(&state), lock(runtime), &holder(runtime), try_lock);
        let held = hold(&here).unwrap().expect("nobody has it");
        assert!(hold(&there).unwrap().is_none());
        assert_eq!(holder_at(&lock(&state)), Some(holder(&here)), "the one refused wrote nothing");
        assert!(!lock(&there).exists(), "the one refused left a lock");
        let runtime_lock = file_id(&lock(&here));
        drop(held);
        assert!(!lock(&state).exists(), "the state directory's lock file was left");
        assert!(runtime_lock.is_some() && file_id(&lock(&here)) == runtime_lock, "the runtime directory's went");
        // What a holder killed before said is replaced by the next one's.
        std::fs::write(lock(&state), "{\"pid\":1,\"host\":\"gone\",\"runtime\":\"/elsewhere\"}\nmore\n").unwrap();
        let held = once_free(|| hold(&there));
        assert_eq!(std::fs::read_to_string(lock(&state)).unwrap().lines().count(), 1);
        assert_eq!(holder_at(&lock(&state)), Some(holder(&there)));
        drop(held);
        // A kraal 0.3.0 holds the runtime directory's lock alone: the session is its own there,
        // and the same file is the lock once it is gone.
        let old = once_free(|| LockFile::acquire(lock(&here), true, try_lock));
        assert!(hold(&here).unwrap().is_none());
        assert!(!lock(&state).exists(), "the one refused kept the state directory's lock");
        drop(old);
        assert_eq!(file_id(&lock(&here)), runtime_lock);
        drop(once_free(|| hold(&here)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Where no lock can be had in the state directory (its file system locks no file, or the
    /// lock file cannot be made), the session is not held at all: an error says what to do, and
    /// the runtime directory's lock is not taken in its place.
    #[test]
    fn a_session_is_not_held_where_the_state_directory_cannot_lock() {
        let dir = std::env::temp_dir().join(format!("kraal-unheld-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (state, runtime) = (dir.join("state"), dir.join("runtime"));
        for d in [&state, &runtime] {
            std::fs::create_dir_all(d).unwrap();
        }
        let (state, runtime) = (state.join("main.lock"), runtime.join("main.lock"));
        let holder = Holder { pid: 4242, host: "box".into(), runtime: dir.join("runtime") };
        // As a file system that locks no file answers.
        fn no_locks(_: &std::fs::File) -> std::io::Result<bool> {
            Err(std::io::Error::from_raw_os_error(libc::ENOLCK))
        }
        let unheld = SessionLock::acquire_at(state.clone(), runtime.clone(), &holder, no_locks).err().expect("held");
        let said = unheld.to_string();
        assert!(said.contains("no session can be held in") && said.contains("XDG_STATE_HOME"), "{said}");
        assert_eq!(unheld.raw_os_error(), None, "{said}");
        assert!(said.contains(&std::io::Error::from_raw_os_error(libc::ENOLCK).to_string()), "{said}");
        assert!(!runtime.exists(), "the runtime directory's lock was taken");
        // A lock file that cannot be made is said the same way.
        std::fs::remove_file(&state).unwrap();
        std::fs::create_dir(&state).unwrap();
        let unheld = SessionLock::acquire_at(state, runtime.clone(), &holder, try_lock).err().expect("held");
        assert!(unheld.to_string().contains("no session can be held in"), "{unheld}");
        assert!(!runtime.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A state directory and a runtime directory that are one, as given or through a link, have
    /// one lock file for both: it is locked once, and stays as the runtime directory's does.
    #[test]
    fn a_state_and_a_runtime_directory_that_are_one_hold_a_session_once() {
        let dir = std::env::temp_dir().join(format!("kraal-onedir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (both, alias) = (dir.join("both"), dir.join("alias"));
        std::fs::create_dir_all(&both).unwrap();
        std::os::unix::fs::symlink(&both, &alias).unwrap();
        let holder = Holder { pid: 4242, host: "box".into(), runtime: both.clone() };
        for runtime in [&both, &alias] {
            let hold = || SessionLock::acquire_at(both.join("main.lock"), runtime.join("main.lock"), &holder, try_lock);
            let held = once_free(hold);
            assert!(hold().unwrap().is_none(), "held twice");
            // A kraal 0.3.0, which knows the runtime directory's lock alone, is kept out too.
            assert!(LockFile::acquire(runtime.join("main.lock"), true, try_lock).unwrap().is_none());
            assert_eq!(holder_at(&both.join("main.lock")).map(|said| said.pid), Some(4242));
            let file = file_id(&both.join("main.lock"));
            drop(held);
            assert!(file.is_some() && file_id(&both.join("main.lock")) == file, "the lock file went with its holder");
            drop(once_free(hold));
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// What says who holds a session is read only from a file of ours, and never waited for.
    #[test]
    fn who_holds_a_session_is_read_from_a_file_only() {
        let dir = std::env::temp_dir().join(format!("kraal-holder-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (fifo, link, odd) = (dir.join("fifo.lock"), dir.join("link.lock"), dir.join("odd.lock"));
        let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a path that ends in a zero byte.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert_eq!(holder_at(&fifo), None);
        std::fs::write(&odd, "not what a kraal says\n").unwrap();
        assert_eq!(holder_at(&odd), None);
        std::fs::write(dir.join("said"), "{\"pid\":7,\"host\":\"box\",\"runtime\":\"/run\"}\n").unwrap();
        std::os::unix::fs::symlink(dir.join("said"), &link).unwrap();
        assert_eq!(holder_at(&link), None, "read through a link");
        assert!(holder_at(&dir.join("said")).is_some());
        assert_eq!(holder_at(&dir.join("none.lock")), None);
        // Nor is a lock taken on what is not a file.
        assert!(LockFile::acquire(fifo, false, try_lock).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A server listens where none does: a socket that refuses is removed for it, one a server
    /// listens at is left to that server. A socket that took the place of the one that refused
    /// is put back when the removal finds it; what a server binding at that very moment does to
    /// this is not shown here.
    #[test]
    fn a_socket_is_removed_only_when_nobody_listens_at_it() {
        use std::io::ErrorKind;
        let dir = std::env::temp_dir().join(format!("kraal-alone-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("main.sock");
        assert_eq!(connect_now(&socket).unwrap_err().kind(), ErrorKind::NotFound);
        let first = listen_alone(&socket).unwrap().expect("nothing was there");
        assert!(connect_now(&socket).is_ok());
        assert!(listen_alone(&socket).unwrap().is_none(), "a server listens there");
        assert!(connect_now(&socket).is_ok(), "its socket was touched");
        // Its server gone, the socket stays, and refuses.
        drop(first);
        until_nobody_listens(&socket);
        assert_eq!(connect_now(&socket).unwrap_err().kind(), ErrorKind::ConnectionRefused);
        let stale = file_id(&socket);
        let second = listen_alone(&socket).unwrap().expect("nobody listened there");
        assert!(file_id(&socket) != stale && connect_now(&socket).is_ok());
        // A server that took the place after a connection was refused (kraal 0.2.0 takes no
        // lock) keeps it when the removal comes after: what is removed is the file that refused.
        remove_stale(&socket, stale).unwrap();
        assert!(connect_now(&socket).is_ok(), "the socket of a server was removed");
        let left: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(left, ["main.sock"]);
        drop(second);
        remove_stale(&socket, file_id(&socket)).unwrap();
        assert!(!socket.exists() && std::fs::read_dir(&dir).unwrap().next().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A server that takes no connection for now is not waited for: on Linux the system says so
    /// at once, where a plain connect would wait until it does.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_server_that_takes_no_connection_is_not_waited_for() {
        use std::os::fd::AsRawFd;
        let dir = std::env::temp_dir().join(format!("kraal-full-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("main.sock");
        let server = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        // SAFETY: plain syscall on a socket this test holds: its queue takes one connection.
        assert_eq!(unsafe { libc::listen(server.as_raw_fd(), 0) }, 0);
        let _waiting = connect_now(&socket).unwrap();
        let start = std::time::Instant::now();
        assert_eq!(connect_now(&socket).unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
        assert!(start.elapsed() < std::time::Duration::from_secs(5));
        // It is there all the same: nobody takes its place.
        assert!(listen_alone(&socket).unwrap().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_private_log_is_never_opened_through_a_link() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("kraal-private-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("main.log");
        std::fs::write(&log, "old\n").unwrap();
        std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o644)).unwrap();
        open_private_append(&log).unwrap().write_all(b"new\n").unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "old\nnew\n");
        assert_eq!(std::fs::metadata(&log).unwrap().permissions().mode() & 0o777, 0o600);
        let elsewhere = dir.join("elsewhere");
        let link = dir.join("link.log");
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
        assert!(open_private_append(&link).is_err());
        assert!(!elsewhere.exists(), "nothing is written where the link points");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_linked_configuration_file_stays_a_link() {
        let dir = std::env::temp_dir().join(format!("kraal-linked-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("dotfiles")).unwrap();
        let real = dir.join("dotfiles/servers.toml");
        std::fs::write(&real, "old").unwrap();
        let link = dir.join("servers.toml");
        std::os::unix::fs::symlink("dotfiles/servers.toml", &link).unwrap();
        write_atomic(&link, b"new").unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&real).unwrap(), "new");
        // A link to nothing is refused, not replaced by a file.
        let dangling = dir.join("accounts.toml");
        std::os::unix::fs::symlink("dotfiles/gone.toml", &dangling).unwrap();
        let refused = write_atomic(&dangling, b"x").unwrap_err();
        assert!(refused.to_string().contains("not there"), "{refused}");
        assert!(std::fs::symlink_metadata(&dangling).unwrap().file_type().is_symlink());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn temporary_files_of_writers_gone_are_removed_and_no_others() {
        let dir = std::env::temp_dir().join(format!("kraal-abandoned-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // A process that ran and ended (and was reaped): what it was writing is abandoned.
        let mut gone = std::process::Command::new("true").spawn().unwrap();
        let dead = gone.id();
        gone.wait().unwrap();
        let me = std::process::id();
        let abandoned = [format!("..active-main.json.{dead}.38.tmp"), format!(".servers.toml.{dead}.0.tmp")];
        let kept = [
            format!(".main.json.{me}.3.tmp"),
            ".main.json.1.0.tmp".to_string(),
            format!("main.json.{dead}.0.tmp"),
            format!(".main.json.{dead}.x.tmp"),
            format!("..{dead}.0.tmp"),
            "notes.txt".to_string(),
        ];
        for name in abandoned.iter().chain(&kept) {
            std::fs::write(dir.join(name), "x").unwrap();
        }
        // Never a link, nor what it leads to.
        let link = format!(".link.{dead}.0.tmp");
        std::os::unix::fs::symlink(dir.join("notes.txt"), dir.join(&link)).unwrap();
        assert_eq!(remove_files_in(&dir, abandoned_temp).unwrap(), kept.len() + 1);
        for name in &abandoned {
            assert!(!dir.join(name).exists(), "{name}");
        }
        for name in kept.iter().chain([&link]) {
            assert!(std::fs::symlink_metadata(dir.join(name)).is_ok(), "{name}");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_abandoned_copies_of_one_file_are_removed_and_no_others() {
        let dir = std::env::temp_dir().join(format!("kraal-abandoned-of-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut gone = std::process::Command::new("true").spawn().unwrap();
        let dead = gone.id();
        gone.wait().unwrap();
        let me = std::process::id();
        let mine = format!(".main.json.{dead}.0.tmp");
        let others = [
            format!(".main.json.json.{dead}.0.tmp"),
            format!(".other.json.{dead}.1.tmp"),
            format!(".main.json.{me}.2.tmp"),
        ];
        for name in others.iter().chain([&mine]) {
            std::fs::write(dir.join(name), "x").unwrap();
        }
        remove_abandoned_temps(&dir.join("main.json")).unwrap();
        assert!(!dir.join(&mine).exists());
        for name in &others {
            assert!(dir.join(name).exists(), "{name}");
        }
        // Nothing to remove where nothing was written.
        remove_abandoned_temps(&dir.join("never/main.json")).unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }
}
