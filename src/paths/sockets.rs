use std::path::{Path, PathBuf};

use super::locations::{
    SOCKET_PATH_MAX, SOCKET_STEM_MAX, runtime_dir, runtime_dir_in, safe_chain, saved_sessions, short_spelling,
};
use super::writes::{file_id, rename_no_replace};

/// The socket of `session`: where its server listens, or a new one will. A server an older kraal
/// started may listen elsewhere: in the directory it used ([`legacy_runtime_dir`]), or in this
/// one under the session's whole name, which older kraals kept up to Linux's limit where kraal
/// now shortens it. While nothing is at the usual place and one of those answers, it is the one,
/// so the session stays reachable (and is not started twice) until it is restarted.
pub fn socket_path(session: &str) -> PathBuf {
    socket_path_with(session, std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from), std::env::temp_dir())
}

pub(super) fn socket_path_with(session: &str, xdg: Option<PathBuf>, tmp: PathBuf) -> PathBuf {
    let dir = runtime_dir_in(xdg.clone());
    let name = format!("{session}.sock");
    let older = vec![dir.join(&name), legacy_runtime_dir_in(xdg, tmp).join(&name)];
    reachable(socket_path_in(&dir, session), older)
}

/// `current` unless it is missing and one of `older` is a live socket of ours, checked as the
/// runtime directory is: then that socket, through the directory path that was checked.
pub(super) fn reachable(current: PathBuf, older: Vec<PathBuf>) -> PathBuf {
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
/// holds the session first ([`super::locks::SessionLock`]), which a server of kraal 0.3.0 or later that is
/// only busy still has. What is left is a busy server of kraal 0.2.0 on macOS, which holds
/// nothing.
pub fn nobody_listens(socket: &Path) -> bool {
    use std::io::ErrorKind;
    connect_now(socket).is_err_and(|e| matches!(e.kind(), ErrorKind::NotFound | ErrorKind::ConnectionRefused))
}

/// What the system's socket address has for a path, the zero byte that ends it included: 108
/// bytes on Linux, 104 on macOS.
pub(super) const SUN_PATH: usize =
    std::mem::size_of::<libc::sockaddr_un>() - std::mem::offset_of!(libc::sockaddr_un, sun_path);

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
pub(super) fn listen_alone_with(
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
pub(super) fn alias_of(socket: &Path) -> Option<PathBuf> {
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
pub(super) const LINUX_SOCKET_PATH_MAX: usize = 107;

/// Where the builds before 0.1.0 (up to 2026-10-11) put their sockets: `$XDG_RUNTIME_DIR` when it
/// was at most 60 bytes, else `$TMPDIR/kraal-<uid>`, without the checks made since, and each
/// session under its own name. `None` when that is where they go now; a socket there may still
/// have a name kraal now shortens, which [`socket_path`] looks for. No release used this place:
/// it serves sessions started by those builds only, and can go in any release.
pub fn legacy_runtime_dir() -> Option<PathBuf> {
    let old = legacy_runtime_dir_in(std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from), std::env::temp_dir());
    (old != runtime_dir()).then_some(old)
}

pub(super) fn legacy_runtime_dir_in(xdg: Option<PathBuf>, tmp: PathBuf) -> PathBuf {
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
pub(super) fn socket_path_in(dir: &Path, session: &str) -> PathBuf {
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
