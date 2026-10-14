//! Changes in directories, told by the kernel rather than looked for: inotify on Linux, kqueue
//! on macOS. One thread sleeps on the kernel's queue and passes on which watches saw something.
//! A network file system does not tell what other machines change (see [`local`]): callers then
//! keep looking for themselves.

use std::io;
use std::path::Path;

/// What a watch is known by.
pub type WatchId = u64;

/// Not a watch: the kernel dropped changes (its queue overflowed), any watch may have seen some.
pub const ANY: WatchId = WatchId::MAX;

/// A watch that saw a change. `gone`: what it watched went (deleted, moved away), so it sees
/// nothing more, and the path must be watched again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Change {
    pub id: WatchId,
    pub gone: bool,
}

pub struct Watcher {
    imp: imp::Watcher,
}

impl Watcher {
    /// Start watching. `changed` gets the changes as they come (those that come together at
    /// once, each watch once), on the watcher's thread, which ends with the watcher.
    pub fn new(changed: impl FnMut(Vec<Change>) + Send + 'static) -> io::Result<Watcher> {
        Ok(Watcher { imp: imp::Watcher::new(changed)? })
    }

    /// Watch the entries of directory `path`: added, removed, renamed into or out of it (not
    /// what is written to its files: what git writes, it renames into place). Watching the same
    /// path again may give the same id; each `add` takes its own `remove`.
    pub fn add(&mut self, path: &Path) -> io::Result<WatchId> {
        self.imp.add(path)
    }

    pub fn remove(&mut self, id: WatchId) {
        self.imp.remove(id)
    }
}

/// Whether `path` is on a file system that tells about every change made to it: not a network
/// one, where another machine's changes go untold. `false` when it cannot be told.
pub fn local(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return false };
    // SAFETY: statfs fills the zeroed struct for a NUL-terminated path.
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c.as_ptr(), &mut st) } != 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        // NFS, SMB and CIFS (three), FUSE (sshfs and the like), Coda, AFS (two), Ceph, 9P (WSL's
        // Windows drives), OCFS2, GFS2, Lustre, NCP, OrangeFS, GPFS, VirtualBox shared folders.
        const REMOTE: [u32; 17] = [
            0x6969,
            0x517B,
            0xFF53_4D42,
            0xFE53_4D42,
            0x6573_5546,
            0x7375_7245,
            0x5346_414F,
            0x6B41_4653,
            0x00C3_6400,
            0x0102_1997,
            0x7461_636F,
            0x0116_1970,
            0x0BD0_0BD0,
            0x564C,
            0x2003_0528,
            0x4750_4653,
            0x786F_4256,
        ];
        !REMOTE.contains(&(st.f_type as u32))
    }
    #[cfg(target_os = "macos")]
    {
        let name: Vec<u8> = st.f_fstypename.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
        st.f_flags & libc::MNT_LOCAL as u32 != 0 && !name.windows(4).any(|w| w == b"fuse")
    }
}

/// Add `change` to `out`, once per watch: gone if any of its changes says so.
fn merge(out: &mut Vec<Change>, change: Change) {
    match out.iter_mut().find(|c| c.id == change.id) {
        Some(c) => c.gone |= change.gone,
        None => out.push(change),
    }
}

/// Wait until `fd` can be read; `false` once `stop` can be (its other end was closed).
#[cfg(target_os = "linux")]
fn wait(fd: std::os::fd::RawFd, stop: std::os::fd::RawFd) -> bool {
    loop {
        let mut fds = [
            libc::pollfd { fd, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: stop, events: libc::POLLIN, revents: 0 },
        ];
        // SAFETY: two valid pollfds.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if n < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return false;
        }
        if fds[1].revents != 0 || fds[0].revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            return false;
        }
        if fds[0].revents != 0 {
            return true;
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::collections::HashMap;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    use super::{Change, WatchId};

    /// What is reported: entries of a directory added, removed, moved, and the directory itself
    /// going. Anything but a directory is refused.
    const MASK: u32 = libc::IN_CREATE
        | libc::IN_DELETE
        | libc::IN_MOVED_FROM
        | libc::IN_MOVED_TO
        | libc::IN_DELETE_SELF
        | libc::IN_MOVE_SELF
        | libc::IN_ONLYDIR;

    pub struct Watcher {
        fd: OwnedFd,
        /// inotify gives a path watched twice the same descriptor: removed with its last user.
        users: HashMap<libc::c_int, usize>,
        _stop: std::io::PipeWriter,
    }

    impl Watcher {
        pub fn new(mut changed: impl FnMut(Vec<Change>) + Send + 'static) -> io::Result<Watcher> {
            // SAFETY: plain syscall; the descriptor is owned below.
            let raw = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: a fresh descriptor nothing else owns.
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            let queue = fd.try_clone()?;
            let (stop, stop_end) = std::io::pipe()?;
            crate::threads::named("watch").spawn(move || {
                let mut buf = vec![0u8; 64 << 10];
                while super::wait(queue.as_raw_fd(), stop.as_raw_fd()) {
                    let mut out = Vec::new();
                    loop {
                        // SAFETY: reading into a buffer this owns, at most its length.
                        let n = unsafe { libc::read(queue.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                        if n <= 0 {
                            // Empty (EAGAIN), or interrupted: what was read goes out first.
                            break;
                        }
                        parse(&buf[..n as usize], &mut out);
                    }
                    if !out.is_empty() {
                        changed(out);
                    }
                }
            })?;
            Ok(Watcher { fd, users: HashMap::new(), _stop: stop_end })
        }

        pub fn add(&mut self, path: &Path) -> io::Result<WatchId> {
            let c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
            // SAFETY: a valid descriptor and NUL-terminated path.
            let wd = unsafe { libc::inotify_add_watch(self.fd.as_raw_fd(), c.as_ptr(), MASK) };
            if wd < 0 {
                return Err(io::Error::last_os_error());
            }
            *self.users.entry(wd).or_insert(0) += 1;
            Ok(wd as WatchId)
        }

        pub fn remove(&mut self, id: WatchId) {
            let Ok(wd) = libc::c_int::try_from(id) else { return };
            match self.users.get_mut(&wd) {
                Some(n) if *n > 1 => *n -= 1,
                Some(_) => {
                    self.users.remove(&wd);
                    // SAFETY: plain syscall; a watch the kernel already dropped is refused, harmlessly.
                    unsafe { libc::inotify_rm_watch(self.fd.as_raw_fd(), wd) };
                }
                None => {}
            }
        }
    }

    /// The events in `buf`, as changes of their watches.
    fn parse(buf: &[u8], out: &mut Vec<Change>) {
        let head = std::mem::size_of::<libc::inotify_event>();
        let mut at = 0;
        while at + head <= buf.len() {
            // SAFETY: a whole event header lies at `at`; it is read without assuming alignment.
            let ev: libc::inotify_event = unsafe { std::ptr::read_unaligned(buf[at..].as_ptr().cast()) };
            at += head + ev.len as usize;
            if ev.mask & libc::IN_Q_OVERFLOW != 0 {
                super::merge(out, Change { id: super::ANY, gone: false });
                continue;
            }
            if ev.wd < 0 {
                continue;
            }
            let gone = ev.mask & (libc::IN_IGNORED | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF) != 0;
            super::merge(out, Change { id: ev.wd as WatchId, gone });
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::collections::HashMap;
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    use std::sync::Arc;

    use super::{Change, WatchId};

    /// What is reported: a directory's entries changing (`NOTE_WRITE`), and the directory itself
    /// going.
    const NOTES: u32 = libc::NOTE_WRITE | libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_REVOKE;

    pub struct Watcher {
        queue: Arc<OwnedFd>,
        /// The descriptor each watch holds open: closing it ends the watch.
        open: HashMap<WatchId, OwnedFd>,
        next: WatchId,
        _stop: std::io::PipeWriter,
    }

    fn register(queue: &OwnedFd, ident: libc::c_int, filter: i16, fflags: u32, id: WatchId) -> io::Result<()> {
        let change = libc::kevent {
            ident: ident as libc::uintptr_t,
            filter,
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags,
            data: 0,
            udata: id as usize as *mut libc::c_void,
        };
        // SAFETY: one valid change, no events asked back.
        let r = unsafe { libc::kevent(queue.as_raw_fd(), &change, 1, std::ptr::null_mut(), 0, std::ptr::null()) };
        if r < 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    impl Watcher {
        pub fn new(mut changed: impl FnMut(Vec<Change>) + Send + 'static) -> io::Result<Watcher> {
            // SAFETY: plain syscall; the descriptor is owned below.
            let raw = unsafe { libc::kqueue() };
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: a fresh descriptor nothing else owns.
            let queue = Arc::new(unsafe { OwnedFd::from_raw_fd(raw) });
            let (stop, stop_end) = std::io::pipe()?;
            // The other end closed (the watcher dropped), the pipe reads as ended: the thread stops.
            register(&queue, stop.as_raw_fd(), libc::EVFILT_READ, 0, super::ANY)?;
            let thread_queue = queue.clone();
            crate::threads::named("watch").spawn(move || {
                let _stop = stop;
                // SAFETY: kevent is plain data; zeroed is a valid value.
                let mut events: [libc::kevent; 32] = unsafe { std::mem::zeroed() };
                loop {
                    // SAFETY: a buffer of `events.len()` events, no changes, no timeout.
                    let n = unsafe {
                        libc::kevent(
                            thread_queue.as_raw_fd(),
                            std::ptr::null(),
                            0,
                            events.as_mut_ptr(),
                            events.len() as libc::c_int,
                            std::ptr::null(),
                        )
                    };
                    if n < 0 {
                        if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                            continue;
                        }
                        return;
                    }
                    let mut out = Vec::new();
                    for ev in &events[..n as usize] {
                        if ev.filter == libc::EVFILT_READ {
                            return;
                        }
                        let gone = ev.fflags & (libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_REVOKE) != 0;
                        super::merge(&mut out, Change { id: ev.udata as usize as WatchId, gone });
                    }
                    if !out.is_empty() {
                        changed(out);
                    }
                }
            })?;
            Ok(Watcher { queue, open: HashMap::new(), next: 1, _stop: stop_end })
        }

        pub fn add(&mut self, path: &Path) -> io::Result<WatchId> {
            let c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
            // SAFETY: a NUL-terminated path; the descriptor is owned below.
            let raw = unsafe { libc::open(c.as_ptr(), libc::O_EVTONLY | libc::O_CLOEXEC) };
            if raw < 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: a fresh descriptor nothing else owns.
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            let id = self.next;
            self.next += 1;
            register(&self.queue, fd.as_raw_fd(), libc::EVFILT_VNODE, NOTES, id)?;
            self.open.insert(id, fd);
            Ok(id)
        }

        pub fn remove(&mut self, id: WatchId) {
            self.open.remove(&id);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn next(rx: &crossbeam_channel::Receiver<Vec<Change>>) -> Vec<Change> {
        rx.recv_timeout(Duration::from_secs(5)).expect("no change seen")
    }

    /// Nothing more comes for a moment.
    fn quiet(rx: &crossbeam_channel::Receiver<Vec<Change>>) {
        while let Ok(c) = rx.recv_timeout(Duration::from_millis(300)) {
            let _ = c;
        }
    }

    #[test]
    fn a_directory_tells_its_entries_changing() {
        let dir = std::env::temp_dir().join(format!("kraal-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut w = Watcher::new(move |c| {
            let _ = tx.send(c);
        })
        .unwrap();
        let id = w.add(&dir).unwrap();
        let sub = w.add(&dir.join("sub")).unwrap();
        assert_ne!(id, sub);
        // A file written whole elsewhere and moved in, as git writes its index and refs.
        std::fs::write(dir.join("sub/index.lock"), "x").unwrap();
        std::fs::rename(dir.join("sub/index.lock"), dir.join("index")).unwrap();
        let mut seen = Vec::new();
        while !(seen.contains(&id) && seen.contains(&sub)) {
            seen.extend(next(&rx).iter().map(|c| c.id));
        }
        quiet(&rx);
        // Removed, a watch tells nothing more (Linux says once that it ended).
        w.remove(sub);
        quiet(&rx);
        std::fs::write(dir.join("sub/other"), "y").unwrap();
        std::fs::remove_file(dir.join("index")).unwrap();
        assert!(next(&rx).iter().all(|c| c.id == id));
        quiet(&rx);
        // A watched directory that goes says so.
        let gone = w.add(&dir.join("sub")).unwrap();
        std::fs::remove_dir_all(dir.join("sub")).unwrap();
        let mut said = false;
        while !said {
            said = next(&rx).iter().any(|c| c.id == gone && c.gone);
        }
        drop(w);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_local_directory_is_told_from_one_that_cannot_be_trusted() {
        assert!(local(&std::env::temp_dir()));
        assert!(!local(Path::new("/no/such/directory/kraal")), "what cannot be looked at is not trusted");
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn the_watcher_sleeps_until_something_changes() {
        let dir = std::env::temp_dir().join(format!("kraal-watch-idle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        let (tid_tx, tid_rx) = crossbeam_channel::bounded(1);
        let mut w = Watcher::new(move |c| {
            crate::testing::block_signals();
            // SAFETY: gettid has no preconditions.
            let _ = tid_tx.try_send(unsafe { libc::gettid() });
            let _ = tx.send(c);
        })
        .unwrap();
        let id = w.add(&dir).unwrap();
        std::fs::write(dir.join("a"), "1").unwrap();
        assert!(next(&rx).iter().any(|c| c.id == id));
        quiet(&rx);
        let tid = tid_rx.recv().unwrap();
        crate::testing::wait_asleep(tid);
        let switches = crate::testing::thread_switches(tid);
        let before = switches();
        crate::testing::sleep_with_signals(tid, Duration::from_millis(700));
        assert_eq!(switches(), before, "the watcher woke up while nothing changed");
        std::fs::write(dir.join("b"), "2").unwrap();
        assert!(next(&rx).iter().any(|c| c.id == id));
        drop(w);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
