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
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let lock =
        std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join(format!(".{name}.lock")))?;
    // Another kraal holds it for a moment at most; never wait for good.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
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

/// A session held by one process: its server, from before it listens until it is gone, or
/// `kraal forget` while it deletes what the session saved. A session so never has two servers,
/// nor loses its files to `forget` while a server restores them, which a check that its socket
/// answers cannot promise (a server not listening yet, a socket deleted).
pub struct SessionLock {
    /// Locked while open.
    _file: std::fs::File,
}

impl SessionLock {
    /// Hold `session`, unless another process does (`None`).
    pub fn acquire(session: &str) -> std::io::Result<Option<SessionLock>> {
        use std::os::unix::fs::OpenOptionsExt;
        let path = private_runtime_dir()?.join(format!("{session}.lock"));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)?;
        Ok(try_lock(&file)?.then_some(SessionLock { _file: file }))
    }
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
        .find(|old| std::os::unix::net::UnixStream::connect(old).is_ok())
        .unwrap_or(current)
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
