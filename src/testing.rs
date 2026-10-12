//! Helpers shared by unit tests.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

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
