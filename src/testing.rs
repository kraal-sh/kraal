//! Helpers shared by unit tests.

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
