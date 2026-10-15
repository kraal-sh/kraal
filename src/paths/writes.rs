use std::path::{Path, PathBuf};

use super::locks::try_lock;

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
pub fn write_checked(path: &Path, data: &[u8], expected: &FileRevision, backup: &Path) -> std::io::Result<()> {
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
            replace_kept(path, &tmp, &kept, expected, backup)
        });
        let _ = std::fs::remove_file(&tmp);
        if result.is_ok() {
            sync_dir(expected.target.parent().unwrap());
        }
        result
    })?
}

pub(super) fn replace_kept(
    path: &Path,
    tmp: &Path,
    kept: &Path,
    expected: &FileRevision,
    backup: &Path,
) -> std::io::Result<()> {
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
        // Rename the entry, never copy through a symlink at the backup's name.
        std::fs::rename(kept, backup).map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!("keeping a backup at {}: {e}; fix that destination and retry", backup.display()),
            )
        })?;
        recovery = backup.to_path_buf();
        sync_dir(recovery.parent().unwrap());
        rename_no_replace(tmp, &expected.target)
    })();
    if let Err(error) = result {
        if rename_no_replace(&recovery, &expected.target).is_ok() {
            sync_dir(expected.target.parent().unwrap());
            return Err(error);
        }
        // A later save can replace the backup, so failed recovery must keep its own path.
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

/// Resolve the parent directory while keeping the final entry, so renaming a symlink
/// acts on the link itself. Missing directories retain their original spelling.
pub fn normalized_entry(path: &Path) -> PathBuf {
    match (path.parent().and_then(|dir| std::fs::canonicalize(dir).ok()), path.file_name()) {
        (Some(dir), Some(name)) => dir.join(name),
        _ => path.to_path_buf(),
    }
}

/// Whether two paths reach the same file, including symlinks and hard links.
pub fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    if normalized_entry(a) == normalized_entry(b) {
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
