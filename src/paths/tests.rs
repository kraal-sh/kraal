use super::*;

#[test]
fn home_paths_are_shortened_through_a_symlink() {
    let root = std::env::temp_dir().join(format!("kraal-home-paths-{}", crate::accounts::new_uuid()));
    let home = root.join("home");
    let alias = root.join("alias");
    std::fs::create_dir_all(&home).unwrap();
    std::os::unix::fs::symlink(&home, &alias).unwrap();
    let resolved = std::fs::canonicalize(&home).unwrap();
    for path in [&alias, &resolved] {
        assert_eq!(shorten_home_in(path, &alias), "~");
        assert_eq!(shorten_home_in(&path.join("recipe.kdl"), &alias), "~/recipe.kdl");
    }
    let outside = root.join("home-other/recipe.kdl");
    assert_eq!(shorten_home_in(&outside, &alias), outside.display().to_string());
    let missing = root.join("missing");
    assert_eq!(shorten_home_in(&missing.join("recipe.kdl"), &missing), "~/recipe.kdl");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_session_directory_that_cannot_be_read_is_not_treated_as_empty() {
    let root = std::env::temp_dir().join(format!("kraal-saved-dirs-{}", crate::accounts::new_uuid()));
    std::fs::create_dir(&root).unwrap();
    assert!(saved_sessions_in(&root).unwrap().is_empty());
    for dir in ["sessions", "sessions-v2"] {
        let path = root.join(dir);
        std::fs::write(&path, "in the way").unwrap();
        assert!(saved_sessions_in(&root).is_err());
        std::fs::remove_file(path).unwrap();
    }
    std::fs::remove_dir(root).unwrap();
}

#[test]
fn saved_sessions_find_both_formats_once_and_resolve_shortened_sockets() {
    let id = crate::accounts::new_uuid();
    let session = format!("{}{id}", "s".repeat(SESSION_NAME_MAX - id.len()));
    let current = session_state_path(&session);
    let legacy = legacy_session_state_path(&session);
    for path in [&current, &legacy] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "{}").unwrap();
        assert!(saved_sessions().unwrap().contains(&session));
        let socket = socket_path_in(&runtime_dir(), &session);
        assert_eq!(session_of_socket(socket.file_stem().unwrap().to_str().unwrap()), Some(session.clone()));
    }
    assert_eq!(saved_sessions().unwrap().iter().filter(|name| *name == &session).count(), 1);
    assert_ne!(current, legacy);
    assert_ne!(scrollback_dir(&session), state_dir().join("scrollback").join(&session));
    std::fs::remove_file(current).unwrap();
    std::fs::remove_file(legacy).unwrap();
}

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
    let fits_linux_only = plain.as_os_str().len() > SOCKET_PATH_MAX && plain.as_os_str().len() <= LINUX_SOCKET_PATH_MAX;
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
    // Open here while it is looked for: a file made once it is gone could have its number.
    let file = std::fs::File::open(&path).unwrap();
    drop(held);
    assert!(is_file_at(&file, &path), "the lock file went with its holder");
    drop(once_free(|| LockFile::acquire(path.clone(), true, try_lock)));
    assert!(is_file_at(&file, &path));
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
    // Open here while it is looked for: a file made once it is gone could have its number.
    let runtime_lock = std::fs::File::open(lock(&here)).unwrap();
    drop(held);
    assert!(!lock(&state).exists(), "the state directory's lock file was left");
    assert!(is_file_at(&runtime_lock, &lock(&here)), "the runtime directory's went");
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
    assert!(is_file_at(&runtime_lock, &lock(&here)));
    drop(once_free(|| hold(&here)));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_session_stays_held_until_its_inherited_runtime_descriptor_closes() {
    let dir = std::env::temp_dir().join(format!("kraal-inherited-lock-{}", std::process::id()));
    let (state, runtime) = (dir.join("state"), dir.join("runtime"));
    for d in [&state, &runtime] {
        std::fs::create_dir_all(d).unwrap();
    }
    let state = state.join("main.lock");
    let runtime = runtime.join("main.lock");
    let holder = Holder { pid: 4242, host: "box".into(), runtime: dir.join("runtime") };
    let hold = || SessionLock::acquire_at(state.clone(), runtime.clone(), &holder, try_lock);
    let held = hold().unwrap().unwrap();
    // dup and fork share the open file and its flock. An independent open only keeps the
    // inode alive, and must not keep the next holder out once the duplicate closes.
    let inherited = held._runtime.as_ref().unwrap().file.try_clone().unwrap();
    let opened = std::fs::File::open(&runtime).unwrap();
    drop(held);
    assert!(!state.exists());
    assert!(is_file_at(&opened, &runtime));
    assert!(hold().unwrap().is_none(), "the inherited descriptor still holds the runtime lock");
    assert!(!state.exists(), "a failed acquisition left its state lock behind");
    drop(inherited);
    drop(once_free(hold));
    assert!(is_file_at(&opened, &runtime));
    std::fs::remove_dir_all(dir).unwrap();
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
        let file = std::fs::File::open(both.join("main.lock")).unwrap();
        drop(held);
        assert!(is_file_at(&file, &both.join("main.lock")), "the lock file went with its holder");
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

/// What gives a file a second name, as [`listen_alone`] has it.
fn link(file: &Path, name: &Path) -> std::io::Result<()> {
    std::fs::hard_link(file, name)
}

/// As a file system that gives no second name answers.
fn no_link(_: &Path, _: &Path) -> std::io::Result<()> {
    Err(std::io::Error::from_raw_os_error(libc::EPERM))
}

/// A directory of a test's own, with a socket path in it and a socket nobody listens at
/// there.
fn left_behind(name: &str) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!("kraal-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("main.sock");
    drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
    until_nobody_listens(&socket);
    (dir, socket)
}

/// The names in `dir`, in order.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> =
        std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names
}

/// A server listens where none does: a socket that refuses is removed for it, one a server
/// listens at is left to that server, and nothing else is left in the directory.
#[test]
fn a_socket_is_removed_only_when_nobody_listens_at_it() {
    use std::io::ErrorKind;
    let (dir, socket) = left_behind("alone");
    std::fs::remove_file(&socket).unwrap();
    assert_eq!(connect_now(&socket).unwrap_err().kind(), ErrorKind::NotFound);
    let first = listen_alone(&socket).unwrap().expect("nothing was there");
    assert!(connect_now(&socket).is_ok());
    // Nothing refused: nothing is removed, with or without a second name to tell it by.
    for link in [link, no_link] {
        let beside = listen_alone_with(&socket, link, &mut |_| panic!("a socket that answers was taken for one left"));
        assert!(beside.unwrap().is_none(), "a server listens there");
        assert!(connect_now(&socket).is_ok(), "its socket was touched");
    }
    // Its server gone, the socket stays, and refuses: the next server takes its place.
    drop(first);
    until_nobody_listens(&socket);
    assert_eq!(connect_now(&socket).unwrap_err().kind(), ErrorKind::ConnectionRefused);
    let second = listen_alone(&socket).unwrap().expect("nobody listened there");
    assert!(connect_now(&socket).is_ok(), "the server after it does not listen");
    assert_eq!(names(&dir), ["main.sock"]);
    drop(second);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// While it looks at a socket that refuses, `listen_alone` keeps it under a second name of
/// its own making, as long as the socket's own: a server that takes the place at that
/// moment (kraal 0.2.0 takes no lock) is another file whatever number it is given, and
/// keeps its socket. The second name is gone afterwards.
#[test]
fn a_server_that_takes_the_place_of_a_socket_being_removed_keeps_it() {
    let (dir, socket) = left_behind("replaced");
    let mut taken = None;
    let beside = listen_alone_with(&socket, link, &mut |seen| {
        if taken.is_none() {
            let seen = seen.expect("the socket has no second name while it is looked at");
            assert!(file_id(seen).is_some() && file_id(seen) == file_id(&socket), "the name of another file");
            assert_eq!(seen.as_os_str().len(), socket.as_os_str().len());
            std::fs::remove_file(&socket).unwrap();
            taken = Some(std::os::unix::net::UnixListener::bind(&socket).unwrap());
        }
    });
    assert!(beside.unwrap().is_none(), "started beside the server that took the place");
    assert!(taken.is_some() && connect_now(&socket).is_ok(), "the socket of that server was removed");
    assert_eq!(names(&dir), ["main.sock"]);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Where no second name can be made, a socket left behind is still removed, told by the
/// number it had before it refused: a server that takes its place after the refusal is
/// another file, and keeps its socket.
#[test]
fn a_socket_is_told_by_what_it_was_before_it_refused_where_it_gets_no_second_name() {
    let (dir, socket) = left_behind("nolink");
    let listening = listen_alone_with(&socket, no_link, &mut |seen| assert_eq!(seen, None));
    drop(listening.unwrap().expect("nobody listened there"));
    until_nobody_listens(&socket);
    // The file that refuses keeps a name here, so that the server which takes its place is
    // not given its number: what is shown is which file was looked at, and when.
    let kept = dir.join("kept");
    std::fs::hard_link(&socket, &kept).unwrap();
    let mut taken = None;
    let beside = listen_alone_with(&socket, no_link, &mut |_| {
        if taken.is_none() {
            std::fs::remove_file(&socket).unwrap();
            taken = Some(std::os::unix::net::UnixListener::bind(&socket).unwrap());
        }
    });
    assert!(beside.unwrap().is_none(), "started beside the server that took the place");
    assert!(taken.is_some() && connect_now(&socket).is_ok(), "the socket of that server was removed");
    assert_eq!(names(&dir), ["kept", "main.sock"]);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// The second name fits wherever the socket's does: at the longest path a socket can have,
/// a socket left behind is removed as anywhere else.
#[test]
fn a_socket_at_the_longest_path_is_removed_when_nobody_listens_at_it() {
    let root = std::env::temp_dir().join(format!("kraal-longest-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let name = "/main.sock";
    let dir = root.join("d".repeat(SUN_PATH - 1 - name.len() - root.as_os_str().len() - 1));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("main.sock");
    assert_eq!(socket.as_os_str().len(), SUN_PATH - 1);
    assert_eq!(alias_of(&socket).unwrap().as_os_str().len(), SUN_PATH - 1);
    drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
    until_nobody_listens(&socket);
    let listening = listen_alone(&socket).unwrap().expect("nobody listened there");
    assert!(connect_now(&socket).is_ok());
    assert_eq!(names(&dir), ["main.sock"]);
    drop(listening);
    std::fs::remove_dir_all(&root).unwrap();
}

/// The second names a kraal killed while it looked at a socket left behind are removed by
/// the next one that looks at it: as the entries they are, a link not followed, and no
/// other session's. A directory where this process's own name goes is said, and the socket
/// then stays as it is.
#[test]
fn second_names_left_behind_are_removed_and_one_in_the_way_is_said() {
    let (dir, socket) = left_behind("aliases");
    let precious = dir.join("precious");
    std::fs::write(&precious, "kept").unwrap();
    std::fs::write(dir.join("main~0000"), "").unwrap();
    std::os::unix::fs::symlink(&precious, dir.join("main~zzzz")).unwrap();
    for other in ["other~0000", "main~00", "main.lock", "main~00000"] {
        std::fs::write(dir.join(other), "").unwrap();
    }
    drop(listen_alone(&socket).unwrap().expect("nobody listened there"));
    assert_eq!(names(&dir), ["main.lock", "main.sock", "main~00", "main~00000", "other~0000", "precious"]);
    assert_eq!(std::fs::read_to_string(&precious).unwrap(), "kept");
    until_nobody_listens(&socket);
    let own = alias_of(&socket).unwrap();
    std::fs::create_dir(&own).unwrap();
    let said = listen_alone(&socket).unwrap_err();
    assert_eq!(said.kind(), std::io::ErrorKind::AlreadyExists);
    assert!(said.to_string().contains(&own.display().to_string()), "{said}");
    assert!(nobody_listens(&socket) && socket.exists(), "the socket was removed all the same");
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

fn checked_directory(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kraal-checked-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn concurrent_creates_and_replacements_have_one_winner() {
    use std::sync::{Arc, Barrier};
    for exists in [false, true] {
        let dir = checked_directory(if exists { "replace" } else { "create" });
        let path = dir.join("work.kdl");
        if exists {
            std::fs::write(&path, "old").unwrap();
        }
        let alias = dir.join("alias.kdl");
        std::os::unix::fs::symlink("work.kdl", &alias).unwrap();
        let expected = FileRevision::read(&path).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let writers: Vec<_> = [b"first".as_slice(), b"second".as_slice()]
            .into_iter()
            .enumerate()
            .map(|(i, data)| {
                let path = if exists && i == 1 { alias.clone() } else { path.clone() };
                let (expected, barrier) = (expected.clone(), barrier.clone());
                std::thread::spawn(move || {
                    barrier.wait();
                    write_checked(&path, data, &expected)
                })
            })
            .collect();
        let results: Vec<_> = writers.into_iter().map(|w| w.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1, "{results:?}");
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content == "first" || content == "second");
        if exists {
            assert_eq!(std::fs::read_to_string(path.with_extension("kdl.bak")).unwrap(), "old");
        } else {
            assert!(!path.with_extension("kdl.bak").exists());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn edits_with_the_same_size_and_timestamp_are_not_overwritten() {
    let dir = checked_directory("same-time");
    let path = dir.join("work.kdl");
    std::fs::write(&path, "old").unwrap();
    let expected = FileRevision::read(&path).unwrap();
    let time = std::fs::metadata(&path).unwrap().modified().unwrap();
    std::fs::write(&path, "new").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(time))
        .unwrap();
    assert!(write_checked(&path, b"ours", &expected).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
    assert!(!path.with_extension("kdl.bak").exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_edit_between_comparison_and_moving_the_file_is_restored() {
    let dir = checked_directory("displaced-edit");
    let path = dir.join("work.kdl");
    let (tmp, kept) = (dir.join("new"), dir.join("kept.recover"));
    std::fs::write(&path, "old").unwrap();
    let expected = FileRevision::read(&path).unwrap();
    std::fs::write(&tmp, "ours").unwrap();
    // The editor changed it after the initial comparison, before the entry was moved aside.
    std::fs::write(&path, "edited").unwrap();
    rename_no_replace(&path, &kept).unwrap();
    assert!(replace_kept(&path, &tmp, &kept, &expected).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "edited");
    assert!(!kept.exists());
    assert!(!path.with_extension("kdl.bak").exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn an_editor_that_creates_the_destination_wins_and_both_versions_survive() {
    let dir = checked_directory("editor-wins");
    let path = dir.join("work.kdl");
    let (tmp, kept) = (dir.join("new"), dir.join("kept.recover"));
    std::fs::write(&path, "old").unwrap();
    let expected = FileRevision::read(&path).unwrap();
    std::fs::write(&tmp, "ours").unwrap();
    rename_no_replace(&path, &kept).unwrap();
    std::fs::write(&path, "editor").unwrap();
    let error = replace_kept(&path, &tmp, &kept, &expected).unwrap_err().to_string();
    assert!(error.contains(&kept.display().to_string()), "{error}");
    assert!(error.contains("inspect that file before retrying"), "{error}");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "editor");
    assert_eq!(std::fs::read_to_string(&kept).unwrap(), "old");
    assert!(!abandoned_temp(kept.file_name().unwrap().to_str().unwrap()));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn backup_failures_restore_the_file_and_backup_links_are_not_followed() {
    let dir = checked_directory("backup");
    let path = dir.join("work.kdl");
    let backup = path.with_extension("kdl.bak");
    let precious = dir.join("precious");
    std::fs::write(&path, "old").unwrap();
    let expected = FileRevision::read(&path).unwrap();
    std::fs::create_dir(&backup).unwrap();
    assert!(write_checked(&path, b"ours", &expected).is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "old");
    std::fs::remove_dir(&backup).unwrap();
    std::fs::write(&precious, "kept").unwrap();
    std::os::unix::fs::symlink(&precious, &backup).unwrap();
    write_checked(&path, b"ours", &expected).unwrap();
    assert_eq!(std::fs::read_to_string(&precious).unwrap(), "kept");
    assert_eq!(std::fs::read_to_string(&backup).unwrap(), "old");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn checked_saves_preserve_symlinks_modes_and_writes_to_the_old_descriptor() {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let dir = checked_directory("links");
    let path = dir.join("work.kdl");
    let (alias, second) = (dir.join("alias.kdl"), dir.join("second.kdl"));
    std::fs::write(&path, "old").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
    std::os::unix::fs::symlink("work.kdl", &alias).unwrap();
    std::os::unix::fs::symlink("alias.kdl", &second).unwrap();
    let expected = FileRevision::read(&second).unwrap();
    let mut editor = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
    write_checked(&second, b"ours", &expected).unwrap();
    editor.write_all(b" edited").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "ours");
    assert_eq!(std::fs::read_to_string(path.with_extension("kdl.bak")).unwrap(), "old edited");
    assert!(std::fs::symlink_metadata(&second).unwrap().file_type().is_symlink());
    assert!(std::fs::symlink_metadata(&alias).unwrap().file_type().is_symlink());
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o640);
    let expected = FileRevision::read(&alias).unwrap();
    let other = dir.join("other.kdl");
    std::fs::write(&other, "ours").unwrap();
    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&other, &alias).unwrap();
    assert!(write_checked(&alias, b"wrong", &expected).is_err());
    assert_eq!(std::fs::read_to_string(&other).unwrap(), "ours");
    std::fs::remove_file(&other).unwrap();
    assert!(FileRevision::read(&alias).is_err());
    std::fs::remove_dir_all(dir).unwrap();
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
