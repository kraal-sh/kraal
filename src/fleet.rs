//! Run one command on many servers at once (`kraal exec @prod -- uptime`).

use std::collections::VecDeque;
use std::io::Read;
use std::process::{Command, Stdio};
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

/// Run `command` (a remote shell command line, as `ssh host command` takes it) on every server,
/// `parallel` at a time. `ssh_options` gives each server's ssh options (without destination).
/// `done` is called as each server finishes, in completion order.
pub fn run_all(
    servers: Vec<Server>,
    command: &str,
    ssh_options: &(dyn Fn(&Server) -> Vec<String> + Sync),
    parallel: usize,
    timeout: Duration,
    done: &(dyn Fn(&Outcome) + Sync),
) -> Vec<Outcome> {
    let queue = Arc::new(Mutex::new(servers.into_iter().collect::<VecDeque<_>>()));
    let results = Arc::new(Mutex::new(Vec::new()));
    std::thread::scope(|scope| {
        for _ in 0..parallel.max(1) {
            let queue = queue.clone();
            let results = results.clone();
            scope.spawn(move || {
                loop {
                    let Some(server) = queue.lock().unwrap().pop_front() else { break };
                    let mut args = ssh_options(&server);
                    // Never wait for a password or a host key prompt: fail fast instead.
                    args.extend(["-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "-T"].map(String::from));
                    args.push(server.destination());
                    args.push(command.to_string());
                    let outcome = run_one(&server.name, &args, timeout);
                    done(&outcome);
                    results.lock().unwrap().push(outcome);
                }
            });
        }
    });
    Arc::try_unwrap(results).map(|m| m.into_inner().unwrap()).unwrap_or_default()
}

fn run_one(name: &str, ssh_args: &[String], timeout: Duration) -> Outcome {
    let start = Instant::now();
    let mut outcome = Outcome {
        server: name.to_string(),
        code: None,
        stdout: String::new(),
        stderr: String::new(),
        elapsed: Duration::ZERO,
        error: None,
    };
    let child =
        Command::new("ssh").args(ssh_args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            outcome.error = Some(format!("cannot run ssh: {e}"));
            return outcome;
        }
    };
    // Read both pipes on threads so a chatty command cannot block on a full pipe.
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let out_t = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = out.read_to_end(&mut b);
        b
    });
    let err_t = std::thread::spawn(move || {
        let mut b = Vec::new();
        let _ = err.read_to_end(&mut b);
        b
    });
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                outcome.code = status.code();
                break;
            }
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                outcome.error = Some(format!("timed out after {}s", timeout.as_secs()));
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => {
                outcome.error = Some(e.to_string());
                break;
            }
        }
    }
    outcome.stdout = String::from_utf8_lossy(&out_t.join().unwrap_or_default()).into_owned();
    outcome.stderr = String::from_utf8_lossy(&err_t.join().unwrap_or_default()).into_owned();
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
        std::fs::write(&fake, "#!/bin/sh\nfor a; do dest=$cmd; cmd=$a; done\ncase $dest in *bad*) echo oops >&2; exit 3;; esac\necho \"$dest says $cmd\"\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // SAFETY: tests in this module do not read PATH concurrently with this change.
        let path = format!("{}:{}", dir.display(), std::env::var("PATH").unwrap_or_default());
        unsafe { std::env::set_var("PATH", path) };
        let mk = |n: &str| Server { name: n.into(), host: format!("{n}.lan"), ..Default::default() };
        let seen = Mutex::new(Vec::new());
        let results =
            run_all(vec![mk("a"), mk("b"), mk("bad")], "uptime", &|_| Vec::new(), 2, Duration::from_secs(10), &|o| {
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
}
