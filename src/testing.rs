//! Helpers shared by unit tests.

/// Where the tests run: the end-to-end tests have the same.
#[path = "../tests/common/isolation.rs"]
pub mod isolation;

#[path = "../tests/common/script.rs"]
mod script;
pub use script::{read_complete_lines, write_script};

use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};

use ratatui::buffer::Buffer;

/// How long a test waits for what programs it started are to do before it fails. Far longer
/// than it takes: on a machine busy with other work (a CI runner, other tests beside), starting
/// a program or ending one takes seconds, and a wait that ends when its condition holds costs
/// nothing when it does.
pub const PATIENCE: std::time::Duration = std::time::Duration::from_secs(60);

pub struct TempDir(pub PathBuf);

impl TempDir {
    pub fn new(name: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("kraal-test-{}-{name}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // File-manager tests compare these paths with paths resolved by the shell on macOS.
        Self(dir.canonicalize().unwrap())
    }

    pub fn write(&self, name: &str, contents: impl AsRef<[u8]>) -> PathBuf {
        let path = self.0.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    pub fn mkdir(&self, name: &str) -> PathBuf {
        let path = self.0.join(name);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    pub fn text(&self) -> String {
        self.0.to_str().unwrap().to_owned()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn buffer_text(buf: &Buffer) -> String {
    buf.content.iter().map(|cell| cell.symbol()).collect()
}

/// Whether this is the process where test `name` (its full path) runs alone, with a home of its
/// own: for a test that writes where every test of this process looks, such as the workspaces
/// directory. Called first, it starts that process, fails unless the test ran there and passed,
/// and returns false for the test to return; in the process it started it returns true.
pub fn in_own_process(name: &str) -> bool {
    // Not a KRAAL_ name: the isolation clears those as the process loads, and the process it
    // starts would start another.
    const CHILD: &str = "UNIT_TEST_IN_OWN_PROCESS";
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    let output =
        Command::new(std::env::current_exe().unwrap()).args(["--exact", name]).env(CHILD, "1").output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains(" 1 passed"),
        "{name} in a process of its own:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    false
}

/// Keep unrelated process signals on other threads: handling one would count as waking up,
/// even when the measured worker never polls. Called only on workers that end with the test.
#[cfg(target_os = "linux")]
pub fn block_signals() {
    // SAFETY: sigfillset initializes the set; pthread_sigmask changes only this thread's mask.
    unsafe {
        let mut signals = std::mem::zeroed();
        assert_eq!(libc::sigfillset(&mut signals), 0);
        assert_eq!(libc::pthread_sigmask(libc::SIG_BLOCK, &signals, std::ptr::null_mut()), 0);
    }
}

/// Exercise signal isolation throughout a switch measurement. A handler makes SIGCHLD wake
/// an unmasked worker even when this test runs alone, where it would otherwise be ignored.
#[cfg(target_os = "linux")]
pub fn sleep_with_signals(tid: libc::pid_t, duration: std::time::Duration) {
    let handled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let signal = signal_hook::flag::register(libc::SIGCHLD, handled).unwrap();
    let start = std::time::Instant::now();
    while start.elapsed() < duration {
        // SAFETY: both targets belong to this test process; the worker lives until after the
        // measurement. Target it too so the test cannot pass just by choosing another thread.
        unsafe {
            assert_eq!(libc::kill(libc::getpid(), libc::SIGCHLD), 0);
            assert_eq!(libc::syscall(libc::SYS_tgkill, libc::getpid(), tid, libc::SIGCHLD), 0);
        }
        std::thread::sleep(std::time::Duration::from_millis(100).min(duration.saturating_sub(start.elapsed())));
    }
    signal_hook::low_level::unregister(signal);
}

/// A function counting the context switches of thread `tid` of this process (both kinds): what
/// tells a thread that sleeps from one that keeps waking up.
#[cfg(target_os = "linux")]
pub fn thread_switches(tid: libc::pid_t) -> impl Fn() -> u64 {
    move || {
        let status = std::fs::read_to_string(format!("/proc/self/task/{tid}/status")).unwrap();
        status
            .lines()
            .filter(|l| l.starts_with("voluntary_ctxt_switches") || l.starts_with("nonvoluntary_ctxt_switches"))
            .filter_map(|l| l.split_whitespace().last()?.parse::<u64>().ok())
            .sum()
    }
}

/// Wait until thread `tid` of this process sleeps: it waits for something, where a thread that
/// was just given work may still be running, or waiting for a processor on a loaded machine.
/// Its switches are counted from then on, not from a guess of when it got there.
#[cfg(target_os = "linux")]
pub fn wait_asleep(tid: libc::pid_t) {
    let start = std::time::Instant::now();
    loop {
        let status = std::fs::read_to_string(format!("/proc/self/task/{tid}/status")).unwrap();
        if status.lines().any(|l| l.starts_with("State:") && l.contains("(sleeping)")) {
            return;
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(20), "thread {tid} never went to sleep:\n{status}");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// How long thread `tid` of this process has waited for a processor while it could run, since
/// it started: the part of a delay that the machine's load explains, and no slowness of the
/// thread. Zero where the kernel does not count it.
#[cfg(target_os = "linux")]
pub fn waited_for_a_processor(tid: libc::pid_t) -> std::time::Duration {
    let counted = std::fs::read_to_string(format!("/proc/self/task/{tid}/schedstat")).unwrap_or_default();
    let waited = counted.split_whitespace().nth(1).and_then(|ns| ns.parse().ok());
    std::time::Duration::from_nanos(waited.unwrap_or(0))
}

mod tests {
    use super::{TempDir, buffer_text, isolation, write_script};
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::Command;

    #[test]
    fn temporary_directories_are_distinct_and_removed_with_their_contents() {
        let first = TempDir::new("same-name");
        let second = TempDir::new("same-name");
        assert_ne!(first.0, second.0);
        assert_eq!(first.0, first.0.canonicalize().unwrap());
        assert_eq!(first.text(), first.0.to_str().unwrap());
        first.mkdir("nested/directory");
        let file = first.write("nested/directory/data", [0, 255, b'\n']);
        assert_eq!(std::fs::read(file).unwrap(), [0, 255, b'\n']);
        let kept = second.write("kept", "still here");
        let path = first.0.clone();
        drop(first);
        assert!(!path.exists());
        assert_eq!(std::fs::read_to_string(kept).unwrap(), "still here");
        let path = second.0.clone();
        drop(second);
        assert!(!path.exists());
    }

    #[test]
    fn temporary_directory_is_removed_during_unwinding() {
        let mut path = PathBuf::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let dir = TempDir::new("unwind");
            path = dir.0.clone();
            dir.write("file", "contents");
            panic!("test failure");
        }));
        assert!(result.is_err());
        assert!(!path.exists());
    }

    #[test]
    fn scripts_preserve_contents_and_literal_paths_and_are_executable() {
        let dir = TempDir::new("script");
        let path = dir.0.join("script with 'quotes' $and ; spaces");
        for (body, expected) in [
            ("#!/bin/sh\nprintf '%s' 'hello 世界'\n", "hello 世界"),
            ("#!/bin/sh\nprintf '%s' 'replacement'\n", "replacement"),
        ] {
            write_script(&path, body);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), body);
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o755);
            let output = Command::new(&path).output().unwrap();
            assert!(output.status.success());
            assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
        }
    }

    #[test]
    fn buffer_text_preserves_all_cell_symbols_and_spaces_without_row_separators() {
        let mut buf = Buffer::empty(Rect::new(3, 5, 3, 2));
        buf[(3, 5)].set_symbol("界");
        buf[(5, 5)].set_symbol("e\u{301}");
        buf[(4, 6)].set_symbol("z");
        assert_eq!(buffer_text(&buf), "界 e\u{301} z ");
        assert_eq!(buffer_text(&Buffer::empty(Rect::new(0, 0, 0, 0))), "");
    }

    /// The unit tests run where nothing is the developer's, and kraal finds its directories
    /// there.
    #[test]
    fn the_tests_are_out_of_what_is_the_developers() {
        isolation::check();
        assert_eq!(crate::paths::of_the_developer(), None);
        assert!(std::env::var_os(isolation::KEEP_OUT).is_some(), "a kraal the tests start is told nothing");
    }

    /// The tests' `PATH` has no directory of the developer's home, even reached by a link, and
    /// none of their own programs: what else a directory with one holds is still found.
    #[test]
    fn the_tests_path_has_nothing_of_the_developers() {
        let root = std::env::temp_dir().join(format!("kraal-tests-path-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (home, system, plain) = (root.join("home"), root.join("system"), root.join("plain"));
        for dir in [&home.join("bin"), &system, &plain] {
            std::fs::create_dir_all(dir).unwrap();
        }
        for program in [home.join("bin/claude"), system.join("codex"), system.join("git"), plain.join("sh")] {
            write_script(&program, "#!/bin/sh\n");
        }
        std::os::unix::fs::symlink(home.join("bin"), root.join("link")).unwrap();
        let theirs =
            [home.join("bin"), system.clone(), "relative".into(), root.join("link"), plain.clone(), system.clone()];
        let links = root.join("links");
        let path = isolation::path(&std::env::join_paths(theirs).unwrap(), Some(&home), &links);
        let dirs: Vec<_> = std::env::split_paths(&path).collect();
        assert_eq!(dirs, [links.join("0"), plain]);
        assert_eq!(std::fs::read_link(links.join("0/git")).unwrap(), system.join("git"));
        let linked: Vec<_> = std::fs::read_dir(links.join("0")).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(linked, ["git"]);
        std::fs::remove_dir_all(&root).unwrap();
    }
}
