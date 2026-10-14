//! Helpers shared by unit tests.

/// Where the tests run: the end-to-end tests have the same.
#[path = "../tests/common/isolation.rs"]
pub mod isolation;

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

/// How long a test waits for what programs it started are to do before it fails. Far longer
/// than it takes: on a machine busy with other work (a CI runner, other tests beside), starting
/// a program or ending one takes seconds, and a wait that ends when its condition holds costs
/// nothing when it does.
pub const PATIENCE: std::time::Duration = std::time::Duration::from_secs(60);

/// Write an executable script at `path`. A child process writes it: a file this process held
/// open for writing could be inherited by a program another test starts at that moment, and
/// running the script would then fail with "text file busy".
pub fn write_script(path: &Path, body: &str) {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("cat > \"$1\" && chmod 755 \"$1\"")
        .arg("sh")
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(body.as_bytes()).unwrap();
    assert!(child.wait().unwrap().success(), "writing {}", path.display());
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
    use super::{isolation, write_script};

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
