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
        state.join("sessions-v2"),
        state.join("buffers"),
        state.join("runs"),
        state.join("usage"),
        state.join("usage/by-id"),
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

/// The bytes and resolved destination a file operation was prepared against. A confirmation
/// must not authorize a new file reached through a changed symlink or a replaced directory.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FileRevision {
    pub target: PathBuf,
    pub bytes: Option<Vec<u8>>,
    inode: Option<(u64, u64)>,
}

impl FileRevision {
    pub fn read(path: &Path) -> std::io::Result<Self> {
        let target = match std::fs::canonicalize(path) {
            Ok(target) => target,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // A dangling link is not an invitation to replace the link itself.
                if std::fs::symlink_metadata(path).is_ok() {
                    return Err(e);
                }
                let parent = path.parent().unwrap_or(Path::new("."));
                let parent = std::fs::canonicalize(parent)?;
                return Ok(Self {
                    target: parent.join(path.file_name().unwrap_or_default()),
                    bytes: None,
                    inode: None,
                });
            }
            Err(e) => return Err(e),
        };
        let inode = file_id(&target);
        let bytes = read_regular(&target, u64::MAX)?;
        if inode != file_id(&target) {
            return Err(file_changed(path));
        }
        Ok(Self { target, bytes: Some(bytes), inode })
    }
}

fn file_changed(path: &Path) -> std::io::Error {
    std::io::Error::other(format!("File changed while saving; retry to review {}", path.display()))
}

/// Save only the revision that was read or confirmed. The lock serializes kraal writers,
/// including sessions using symlinks to the same file. An editor need not take that lock, so
/// the old entry is moved aside and checked before a no-replace publication. An editor that
/// creates the destination meanwhile wins; writes through its old descriptor stay in the
/// backup. There is briefly no file at the destination, also after a crash at that point.
pub fn write_checked(path: &Path, data: &[u8], expected: &FileRevision) -> std::io::Result<()> {
    locked(&expected.target, || {
        if FileRevision::read(path)? != *expected {
            return Err(file_changed(path));
        }
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&expected.target).map(|m| m.permissions().mode() & 0o7777).unwrap_or(0o644);
        let tmp = temp_beside(&expected.target);
        let result = write_with_mode(&tmp, data, mode).and_then(|()| {
            if expected.bytes.is_none() {
                return rename_no_replace(&tmp, &expected.target);
            }
            // Unlike an abandoned .tmp, this file must survive cleanup after a crash.
            let kept = temp_beside(&expected.target).with_extension("recover");
            rename_no_replace(&expected.target, &kept)?;
            sync_dir(expected.target.parent().unwrap());
            replace_kept(path, &tmp, &kept, expected)
        });
        let _ = std::fs::remove_file(&tmp);
        if result.is_ok() {
            sync_dir(expected.target.parent().unwrap());
        }
        result
    })?
}

fn replace_kept(path: &Path, tmp: &Path, kept: &Path, expected: &FileRevision) -> std::io::Result<()> {
    let mut recovery = kept.to_path_buf();
    let result = (|| {
        if file_id(kept) != expected.inode || read_regular(kept, u64::MAX)? != *expected.bytes.as_ref().unwrap() {
            return Err(file_changed(path));
        }
        // Resolve again while the destination is absent: a retargeted link or directory must
        // not send the new layout elsewhere. A symlink to the held file is temporarily dangling.
        if file_target(path)? != expected.target {
            return Err(file_changed(path));
        }
        let backup = expected.target.with_extension("kdl.bak");
        // Rename the entry, never copy through a symlink at the backup's name.
        std::fs::rename(kept, &backup).map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!("keeping a backup at {}: {e}; fix that destination and retry", backup.display()),
            )
        })?;
        recovery = backup;
        sync_dir(recovery.parent().unwrap());
        rename_no_replace(tmp, &expected.target)
    })();
    if let Err(error) = result {
        if rename_no_replace(&recovery, &expected.target).is_ok() {
            sync_dir(expected.target.parent().unwrap());
            return Err(error);
        }
        // Do not leave a failed save's recovery at .bak, which a later save would replace.
        if recovery != kept && rename_no_replace(&recovery, kept).is_ok() {
            recovery = kept.to_path_buf();
        }
        sync_dir(recovery.parent().unwrap());
        return Err(std::io::Error::other(format!(
            "{error}; the previous file is kept at {}; inspect that file before retrying",
            recovery.display()
        )));
    }
    Ok(())
}

// Unlike canonicalize, this also resolves a symlink to a temporarily absent destination.
fn file_target(path: &Path) -> std::io::Result<PathBuf> {
    let mut entry = path.to_path_buf();
    for _ in 0..40 {
        let parent = std::fs::canonicalize(entry.parent().unwrap_or(Path::new(".")))?;
        entry = parent.join(entry.file_name().unwrap_or_default());
        match std::fs::read_link(&entry) {
            Ok(to) => entry = if to.is_absolute() { to } else { parent.join(to) },
            Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::InvalidInput) => {
                return Ok(entry);
            }
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::from_raw_os_error(libc::ELOOP))
}

/// Whether two definition paths reach the same file, including symlinks and hard links.
pub fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    if crate::workspace::definition_path(a) == crate::workspace::definition_path(b) {
        return true;
    }
    if let (Ok(a), Ok(b)) = (std::fs::metadata(a), std::fs::metadata(b))
        && (a.dev(), a.ino()) == (b.dev(), b.ino())
    {
        return true;
    }
    matches!((file_target(a), file_target(b)), (Ok(a), Ok(b)) if a == b)
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

/// Lock `file` exclusively, without waiting: false when another open file holds it. The lock
/// goes when its last descriptor closes, including duplicates inherited by a forked child.
pub(crate) fn try_lock(file: &std::fs::File) -> std::io::Result<bool> {
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
/// session saved is not forgotten. Linux tells such a server from a socket left behind; macOS
/// refuses for both. So a refusal alone removes nothing and forgets nothing: whoever acts on it
/// holds the session first ([`SessionLock`]), which a server of kraal 0.3.0 or later that is
/// only busy still has. What is left is a busy server of kraal 0.2.0 on macOS, which holds
/// nothing.
pub fn nobody_listens(socket: &Path) -> bool {
    use std::io::ErrorKind;
    connect_now(socket).is_err_and(|e| matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused))
}

/// What the system's socket address has for a path, the zero byte that ends it included: 108
/// bytes on Linux, 104 on macOS.
const SUN_PATH: usize = std::mem::size_of::<libc::sockaddr_un>() - std::mem::offset_of!(libc::sockaddr_un, sun_path);

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
    if name.len() >= SUN_PATH || name.contains(&0) {
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
    #[cfg(target_os = "macos")]
    {
        let enabled: libc::c_int = 1;
        // SAFETY: a descriptor we own and a pointer to an integer of the stated size.
        if unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                (&raw const enabled).cast(),
                std::mem::size_of_val(&enabled) as libc::socklen_t,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
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
    listen_alone_with(socket, |file, name| std::fs::hard_link(file, name), &mut |_| ())
}

/// [`listen_alone`], with what gives a file a second name (`link`), and `meanwhile`, run with
/// that name between the moment a socket refused and its removal: where the tests put a file
/// system that gives none, and a server that takes the place at that moment.
fn listen_alone_with(
    socket: &Path,
    link: fn(&Path, &Path) -> std::io::Result<()>,
    meanwhile: &mut dyn FnMut(Option<&Path>),
) -> std::io::Result<Option<std::os::unix::net::UnixListener>> {
    use std::io::ErrorKind;
    let alias = alias_of(socket);
    remove_abandoned_aliases(socket);
    // A lockless server that takes the place again and again is not raced with for ever.
    for _ in 0..5 {
        match std::os::unix::net::UnixListener::bind(socket) {
            Ok(listener) => return Ok(Some(listener)),
            Err(e) if e.kind() == ErrorKind::AddrInUse => {}
            Err(e) => return Err(e),
        }
        // The file there, before anything is asked of it: what refuses below is this one.
        let there = file_id(socket);
        // A second name for it, for as long as it is looked at: a file is told from another
        // by its number, and one made once this one is gone may be given the same (ext4 gives
        // a new file the number just freed). While this one has a name, no other has its number.
        let named = match &alias {
            Some(alias) => match link(socket, alias) {
                Ok(()) => true,
                // Gone since: the place is free.
                Err(e) if e.kind() == ErrorKind::NotFound => continue,
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    let why = format!("{} is in the way of looking at {}", alias.display(), socket.display());
                    return Err(std::io::Error::new(ErrorKind::AlreadyExists, why));
                }
                // No second name can be made here. The file is then told by the number it had
                // before it refused: sound unless a server that takes no lock replaces it in
                // that moment and is given its number. Refusing to remove it instead would
                // keep every session whose server was killed from starting again there.
                Err(_) => false,
            },
            None => false,
        };
        let (asked, seen) = match &alias {
            Some(alias) if named => (alias.as_path(), file_id(alias)),
            _ => (socket, there),
        };
        let refused = match connect_now(asked) {
            Err(e) if e.kind() == ErrorKind::ConnectionRefused => Ok(true),
            // Gone since: the place is free, and there is nothing to remove.
            Err(e) if e.kind() == ErrorKind::NotFound => {
                meanwhile(None);
                continue;
            }
            // The second name is made here to be a socket path: that it is none is a mistake
            // of this code, not a server that takes no connection.
            Err(e) if named && e.kind() == ErrorKind::InvalidInput => Err(e),
            // It answers, or will once it takes connections again.
            _ => Ok(false),
        };
        let removed = refused.and_then(|refused| {
            if refused {
                meanwhile(named.then_some(asked));
                remove_stale(socket, seen)?;
            }
            Ok(refused)
        });
        if named {
            let _ = std::fs::remove_file(asked);
        }
        if !removed? {
            return Ok(None);
        }
    }
    Err(std::io::Error::new(ErrorKind::AddrInUse, "something keeps taking that place"))
}

/// What ends a socket's own name, and what stands there in the second name [`listen_alone`]
/// gives it: `~`, which no session name and no file of kraal's has, before four characters.
const SOCKET_SUFFIX: &str = ".sock";
const ALIAS_MARK: char = '~';

/// The second name [`listen_alone`] gives the file at `socket` while it looks at it: next to
/// it, of this process alone, and exactly as long as the socket's own, so that it is a socket
/// path wherever that one is. `None` for a path that is not named as kraal names its sockets,
/// or that no socket address holds.
fn alias_of(socket: &Path) -> Option<PathBuf> {
    const DIGITS: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let stem = socket.file_name()?.to_str()?.strip_suffix(SOCKET_SUFFIX)?;
    // Four digits hold every process id (4194304 at most on Linux, 99999 on macOS).
    let mut id = std::process::id() as usize;
    let mut digits = [0u8; SOCKET_SUFFIX.len() - 1];
    for digit in digits.iter_mut().rev() {
        *digit = DIGITS[id % DIGITS.len()];
        id /= DIGITS.len();
    }
    let alias = socket.with_file_name(format!("{stem}{ALIAS_MARK}{}", String::from_utf8_lossy(&digits)));
    (alias.as_os_str().len() < SUN_PATH).then_some(alias)
}

/// Remove the second names of `socket` that a kraal killed while it looked at it left behind.
/// Whoever calls this holds the session, so none is in use. Each is unlinked as the entry it
/// is, never followed; what cannot be unlinked so (a directory put there) stays.
fn remove_abandoned_aliases(socket: &Path) {
    let (Some(dir), Some(name)) = (socket.parent(), socket.file_name().and_then(|name| name.to_str())) else { return };
    let Some(stem) = name.strip_suffix(SOCKET_SUFFIX) else { return };
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let name = entry.file_name();
        let alias = name.to_str().and_then(|name| name.strip_prefix(stem)?.strip_prefix(ALIAS_MARK));
        if alias.is_some_and(|id| id.len() == SOCKET_SUFFIX.len() - 1 && id.chars().all(|c| c.is_ascii_alphanumeric()))
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Remove the socket at `socket` that refused a connection: the file `stale`, which whoever
/// calls this keeps under another name meanwhile, so that no other file has its number. What is
/// at `socket` is moved aside and looked at before it is deleted: when another file took the
/// place between the refusal and the move (a kraal 0.2.0, which holds no lock, started then),
/// that one is put back instead. This narrows what a check followed by a removal leaves open,
/// and does not close it: yet another server may take the place while the file is aside, and
/// the file then stays aside.
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
    saved_sessions()
        .ok()?
        .into_iter()
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
    state_dir().join("sessions-v2").join(format!("{session}.json"))
}

// Keep recognizing old files after their migration is removed, so startup can refuse them.
pub fn legacy_session_state_path(session: &str) -> PathBuf {
    state_dir().join("sessions").join(format!("{session}.json"))
}

pub fn saved_sessions() -> std::io::Result<std::collections::BTreeSet<String>> {
    saved_sessions_in(&state_dir())
}

fn saved_sessions_in(state: &Path) -> std::io::Result<std::collections::BTreeSet<String>> {
    let mut names = std::collections::BTreeSet::new();
    for dir in ["sessions-v2", "sessions"] {
        let entries = match std::fs::read_dir(state.join(dir)) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        for entry in entries {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "json")
                && let Some(name) = path.file_stem().and_then(|s| s.to_str()).filter(|s| valid_session_name(s))
            {
                let current = state.join("sessions-v2").join(format!("{name}.json"));
                if dir == "sessions" && !current.try_exists()? && current.with_extension("forgotten").try_exists()? {
                    continue;
                }
                names.insert(name.to_string());
            }
        }
    }
    Ok(names)
}

/// Where the panes' saved scrollback of `session` goes, one file per pane.
pub fn scrollback_dir(session: &str) -> PathBuf {
    state_dir().join("scrollback-v2").join(session)
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
    let Some(home) = dirs::home_dir() else { return path.display().to_string() };
    shorten_home_in(path, &home)
}

fn shorten_home_in(path: &Path, home: &Path) -> String {
    let resolved = std::fs::canonicalize(home).unwrap_or_else(|_| home.to_path_buf());
    if let Ok(rest) = path.strip_prefix(home).or_else(|_| path.strip_prefix(&resolved)) {
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
    let agents = crate::accounts::agents().map(|a| (a.home_env, std::env::var_os(a.home_env).map(PathBuf::from)));
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
mod tests;
