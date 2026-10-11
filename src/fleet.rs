//! Run one command on many servers at once (`kraal exec @prod -- uptime`).

use std::collections::VecDeque;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::servers::Server;

#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub server: String,
    /// Exit status of the remote command (255 is usually ssh failing to connect).
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub elapsed: Duration,
    /// kraal-side problem: timeout, ssh not runnable.
    pub error: Option<String>,
}

impl Outcome {
    pub fn ok(&self) -> bool {
        self.error.is_none() && self.code == Some(0)
    }
}

/// How to run a command on many servers.
pub struct Plan<'a> {
    /// A remote shell command line, as `ssh host command` takes it.
    pub command: &'a str,
    /// Each server's ssh options (without destination).
    pub ssh_options: &'a (dyn Fn(&Server) -> Vec<String> + Sync),
    pub parallel: usize,
    pub timeout: Duration,
    /// Set to stop: nothing more starts, and what runs is killed (with its descendants).
    pub cancel: &'a AtomicBool,
}

/// Run `plan.command` on every server, `plan.parallel` at a time. `done` is called as each
/// server finishes, in completion order. Servers not started because of a cancellation have no
/// outcome.
pub fn run_all(servers: Vec<Server>, plan: &Plan, done: &(dyn Fn(&Outcome) + Sync)) -> Vec<Outcome> {
    run_all_with("ssh", servers, plan, done)
}

/// [`run_all`] with another `ssh` program (tests).
fn run_all_with(ssh: &str, servers: Vec<Server>, plan: &Plan, done: &(dyn Fn(&Outcome) + Sync)) -> Vec<Outcome> {
    let queue = Arc::new(Mutex::new(servers.into_iter().collect::<VecDeque<_>>()));
    let results = Arc::new(Mutex::new(Vec::new()));
    std::thread::scope(|scope| {
        for _ in 0..plan.parallel.max(1) {
            let queue = queue.clone();
            let results = results.clone();
            scope.spawn(move || {
                loop {
                    if plan.cancel.load(Ordering::SeqCst) {
                        break;
                    }
                    let Some(server) = queue.lock().unwrap().pop_front() else { break };
                    let mut args = (plan.ssh_options)(&server);
                    // Never wait for a password or a host key prompt: fail fast instead.
                    args.extend(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "-T"].map(String::from));
                    args.push(server.destination());
                    args.push(plan.command.to_string());
                    let outcome = run_one(ssh, &server.name, &args, plan.timeout, plan.cancel);
                    done(&outcome);
                    results.lock().unwrap().push(outcome);
                }
            });
        }
    });
    Arc::try_unwrap(results).map(|m| m.into_inner().unwrap()).unwrap_or_default()
}

/// Output kept per server and stream; the rest is dropped (a noisy command on a big fleet must
/// not exhaust memory).
const OUTPUT_LIMIT: usize = 1 << 20;

fn run_one(ssh: &str, name: &str, ssh_args: &[String], timeout: Duration, cancel: &AtomicBool) -> Outcome {
    let start = Instant::now();
    let mut outcome = Outcome {
        server: name.to_string(),
        code: None,
        stdout: String::new(),
        stderr: String::new(),
        elapsed: Duration::ZERO,
        error: None,
    };
    match crate::exec::run_cancellable(Command::new(ssh).args(ssh_args), timeout, OUTPUT_LIMIT, cancel) {
        Ok(out) => {
            outcome.code = out.code;
            outcome.stdout = out.stdout_text();
            outcome.stderr = out.stderr_text();
            if out.truncated {
                outcome.stderr.push_str(&format!("[kraal: output cut at {} KiB]\n", OUTPUT_LIMIT >> 10));
            }
            if out.timed_out {
                outcome.error = Some(format!("timed out after {}s", timeout.as_secs()));
            }
            if out.cancelled {
                outcome.error = Some("cancelled".into());
            }
        }
        Err(e) => outcome.error = Some(format!("cannot run ssh: {e}")),
    }
    outcome.elapsed = start.elapsed();
    outcome
}

/// Human-readable report of one outcome (ANSI colours when `color`).
pub fn format_outcome(o: &Outcome, color: bool) -> String {
    let (green, red, dim, reset) =
        if color { ("\x1b[32m", "\x1b[31m", "\x1b[2m", "\x1b[0m") } else { ("", "", "", "") };
    let status = match (&o.error, o.code) {
        (Some(e), _) => format!("{red}✗ {e}{reset}"),
        (None, Some(0)) => format!("{green}✓{reset}"),
        (None, Some(255)) => format!("{red}✗ ssh failed (255){reset}"),
        (None, Some(c)) => format!("{red}✗ exit {c}{reset}"),
        (None, None) => format!("{red}✗ killed{reset}"),
    };
    let mut s = format!(
        "{status} {}{} {dim}({:.1}s){reset}\n",
        if color { "\x1b[1m" } else { "" },
        o.server,
        o.elapsed.as_secs_f64()
    );
    if color {
        s = s.replacen(&o.server, &format!("{}{reset}", o.server), 1);
    }
    for line in o.stdout.lines() {
        s.push_str(&format!("  {line}\n"));
    }
    for line in o.stderr.lines() {
        s.push_str(&format!("  {red}{line}{reset}\n"));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_in_parallel_and_reports() {
        // A fake ssh: the last argument is the command, the one before the destination.
        let dir = std::env::temp_dir().join(format!("kraal-fleet-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("ssh");
        crate::testing::write_script(
            &fake,
            "#!/bin/sh\nfor a; do dest=$cmd; cmd=$a; done\ncase $dest in *bad*) echo oops >&2; exit 3;; esac\necho \"$dest says $cmd\"\n",
        );
        let mk = |n: &str| Server { name: n.into(), host: format!("{n}.lan"), ..Default::default() };
        let seen = Mutex::new(Vec::new());
        let cancel = AtomicBool::new(false);
        let plan = Plan {
            command: "uptime",
            ssh_options: &|_| Vec::new(),
            parallel: 2,
            timeout: Duration::from_secs(10),
            cancel: &cancel,
        };
        let results = run_all_with(&fake.display().to_string(), vec![mk("a"), mk("b"), mk("bad")], &plan, &|o| {
            seen.lock().unwrap().push(o.server.clone())
        });
        assert_eq!(results.len(), 3);
        assert_eq!(seen.lock().unwrap().len(), 3);
        let a = results.iter().find(|o| o.server == "a").unwrap();
        assert!(a.ok());
        assert_eq!(a.stdout.trim(), "a.lan says uptime");
        let bad = results.iter().find(|o| o.server == "bad").unwrap();
        assert_eq!((bad.ok(), bad.code), (false, Some(3)));
        assert!(format_outcome(bad, false).contains("✗ exit 3 bad"));
        assert!(format_outcome(bad, false).contains("  oops"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_cancelled_run_stops_everything_it_started() {
        let dir = std::env::temp_dir().join(format!("kraal-fleet-cancel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // A fake ssh that leaves a process behind it, and says which.
        let fake = dir.join("ssh");
        let pids = dir.join("pids");
        crate::testing::write_script(
            &fake,
            &format!(
                "#!/bin/sh
sleep 30 &
echo $! >> {}
wait
",
                pids.display()
            ),
        );
        let mk = |n: &str| Server { name: n.into(), host: format!("{n}.lan"), ..Default::default() };
        let cancel = AtomicBool::new(false);
        let plan = Plan {
            command: "true",
            ssh_options: &|_| Vec::new(),
            parallel: 2,
            timeout: Duration::from_secs(60),
            cancel: &cancel,
        };
        let start = Instant::now();
        let results = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_millis(500));
                cancel.store(true, Ordering::SeqCst);
            });
            run_all_with(&fake.display().to_string(), vec![mk("a"), mk("b"), mk("c")], &plan, &|_| {})
        });
        assert!(start.elapsed() < Duration::from_secs(10));
        assert_eq!(results.len(), 2, "the third never started");
        assert!(results.iter().all(|o| o.error.as_deref() == Some("cancelled")), "{results:?}");
        for pid in std::fs::read_to_string(&pids).unwrap().lines() {
            let pid: libc::pid_t = pid.trim().parse().unwrap();
            let start = Instant::now();
            // SAFETY: signal 0 only checks that the process exists.
            while unsafe { libc::kill(pid, 0) } == 0 && start.elapsed() < Duration::from_secs(5) {
                std::thread::sleep(Duration::from_millis(20));
            }
            // SAFETY: as above.
            assert_ne!(unsafe { libc::kill(pid, 0) }, 0, "sleep {pid} was left running");
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
