//! Sign-in checks shared by the account list and login panes. The agent's output stays private.

use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::accounts::{self, Account, Launch};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    SignedIn,
    NotSignedIn,
    NotInstalled,
    CouldNotCheck,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Self::SignedIn => "signed in",
            Self::NotSignedIn => "sign-in required",
            Self::NotInstalled => "not installed",
            Self::CouldNotCheck => "could not check sign-in",
        }
    }

    pub fn logged_in(self) -> Option<bool> {
        match self {
            Self::SignedIn => Some(true),
            Self::NotSignedIn => Some(false),
            Self::NotInstalled | Self::CouldNotCheck => None,
        }
    }
}

#[derive(Debug)]
pub struct Checked {
    pub status: Status,
    pub identity: Option<String>,
}

pub fn check(account: &Account) -> Status {
    inspect(account).status
}

pub fn inspect(account: &Account) -> Checked {
    match accounts::login_status(account) {
        Ok(launch) => inspect_launch(&launch, Duration::from_secs(10), &account.agent),
        Err(_) => Checked { status: Status::CouldNotCheck, identity: None },
    }
}

fn inspect_launch(launch: &Launch, limit: Duration, agent: &str) -> Checked {
    let mut cmd = launch.command();
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match crate::exec::own_group(&mut cmd).spawn() {
        Ok(child) => child,
        Err(e) => {
            let status =
                if e.kind() == std::io::ErrorKind::NotFound { Status::NotInstalled } else { Status::CouldNotCheck };
            return Checked { status, identity: None };
        }
    };
    let drains = crate::exec::Drains::start(&mut child, 16 * 1024);
    let start = Instant::now();
    let status = loop {
        // Keep the pid until the whole group is stopped, including a wrapper's children.
        if crate::exec::has_exited(&child) {
            break match crate::exec::kill_group_nowait(child).and_then(|s| s.code()) {
                Some(0) => Status::SignedIn,
                Some(1) => Status::NotSignedIn,
                _ => Status::CouldNotCheck,
            };
        }
        if start.elapsed() >= limit {
            crate::exec::kill_group_nowait(child);
            break Status::CouldNotCheck;
        }
        std::thread::sleep(Duration::from_millis(30));
    };
    let (stdout, _, truncated) = drains.finish(Duration::from_millis(200));
    let identity = (status == Status::SignedIn && !truncated && agent == "claude")
        .then(|| serde_json::from_slice::<serde_json::Value>(&stdout).ok())
        .flatten()
        .filter(|v| v["loggedIn"] == true)
        .and_then(|v| v["email"].as_str().filter(|email| email.len() <= 120 && email.contains('@')).map(String::from));
    Checked { status, identity }
}

/// Verify first, then re-read under the accounts lock before publishing any discovery.
pub fn discover(all: &accounts::Accounts) -> anyhow::Result<Vec<(Account, Status)>> {
    let candidates = all.unlisted();
    let checked: Vec<_> = std::thread::scope(|scope| {
        let tasks: Vec<_> = candidates.iter().map(|a| scope.spawn(move || inspect(a))).collect();
        tasks
            .into_iter()
            .map(|t| t.join().unwrap_or(Checked { status: Status::CouldNotCheck, identity: None }))
            .collect()
    });
    accounts::Accounts::update(|all| {
        for (a, checked) in candidates.iter().zip(&checked) {
            if checked.status == Status::SignedIn {
                all.adopt_checked(a, checked.identity.as_deref())?;
            }
        }
        Ok(())
    })?;
    Ok(candidates.into_iter().zip(checked.into_iter().map(|c| c.status)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(launch: &Launch, limit: Duration) -> Status {
        inspect_launch(launch, limit, "claude").status
    }

    #[test]
    fn only_successful_recognized_status_output_proposes_a_public_identity() {
        for (script, agent, expected) in [
            (
                r#"printf '%s' '{"loggedIn":true,"email":"me@example.com","token":"PRIVATE"}'"#,
                "claude",
                Some("me@example.com"),
            ),
            (r#"printf '%s' '{"loggedIn":true,"email":"me@example.com"}'; exit 1"#, "claude", None),
            (r#"printf '%s' '{"loggedIn":false,"email":"me@example.com"}'"#, "claude", None),
            (r#"printf '%s' '{"loggedIn":true,"email":"me@example.com"}'"#, "codex", None),
            ("printf 'Logged in using ChatGPT'", "codex", None),
            ("printf PRIVATE", "claude", None),
        ] {
            let launch = Launch { argv: vec!["sh".into(), "-c".into(), script.into()], ..Default::default() };
            let result = inspect_launch(&launch, crate::testing::PATIENCE, agent);
            assert_eq!(result.identity.as_deref(), expected);
            assert!(!format!("{result:?}").contains("PRIVATE"));
        }
    }

    #[test]
    fn checks_distinguish_sign_in_installation_and_errors() {
        let shell = |script: &str| Launch { argv: vec!["sh".into(), "-c".into(), script.into()], ..Default::default() };
        for (script, expected) in [
            ("printf secret; printf secret >&2; exit 0", Status::SignedIn),
            ("exit 1", Status::NotSignedIn),
            ("exit 2", Status::CouldNotCheck),
            ("kill -TERM $$", Status::CouldNotCheck),
        ] {
            assert_eq!(run(&shell(script), crate::testing::PATIENCE), expected);
        }
        let missing = Launch { argv: vec!["/nonexistent/kraal-test-agent".into()], ..Default::default() };
        assert_eq!(run(&missing, crate::testing::PATIENCE), Status::NotInstalled);
        let not_executable = Launch { argv: vec!["/".into()], ..Default::default() };
        assert_eq!(run(&not_executable, crate::testing::PATIENCE), Status::CouldNotCheck);
        assert_eq!(run(&shell("exec sleep 60"), Duration::from_millis(50)), Status::CouldNotCheck);
    }

    #[test]
    fn checks_preserve_arguments_and_the_accounts_environment() {
        let launch = Launch {
            argv: vec![
                "sh".into(),
                "-c".into(),
                r#"test "$#" = 3 && test "$1" = 'two words' && test -z "$2" && test "$3" = '$HOME;*' &&
                   test "$KRAAL_STATUS_HOME" = 'account home' && test "${HOME+x}" != x && test -n "$PATH""#
                    .into(),
                "sh".into(),
                "two words".into(),
                "".into(),
                "$HOME;*".into(),
            ],
            env: vec![
                ("KRAAL_STATUS_HOME".into(), "first".into()),
                ("KRAAL_STATUS_HOME".into(), "account home".into()),
            ],
            env_remove: vec!["HOME".into(), "KRAAL_STATUS_HOME".into()],
        };
        assert_eq!(run(&launch, crate::testing::PATIENCE), Status::SignedIn);
    }

    #[test]
    fn checks_stop_children_even_when_the_wrapper_exits_first() {
        let dir = std::env::temp_dir().join(format!("kraal-status-children-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pidfile = dir.join("pid");
        for code in [0, 1, 2] {
            let launch = Launch {
                argv: vec![
                    "sh".into(),
                    "-c".into(),
                    format!(
                        "sleep 600 & echo $! > '{}'; printf '%s' '{{\"loggedIn\":true,\"email\":\"me@example.com\"}}'; exit {code}",
                        pidfile.display()
                    ),
                ],
                ..Default::default()
            };
            let checked = inspect_launch(&launch, crate::testing::PATIENCE, "claude");
            let pid = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
            let start = std::time::Instant::now();
            while crate::process::running(pid) && start.elapsed() < Duration::from_secs(5) {
                std::thread::sleep(Duration::from_millis(10));
            }
            let running = crate::process::running(pid);
            if running {
                // SAFETY: clean up only the child this test started, also on regression.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            assert!(!running, "the status wrapper left its child running");
            assert_eq!(checked.status, [Status::SignedIn, Status::NotSignedIn, Status::CouldNotCheck][code]);
            assert_eq!(checked.identity.as_deref(), (code == 0).then_some("me@example.com"));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn json_keeps_logged_in_and_adds_an_unambiguous_status() {
        for (status, logged_in, json) in [
            (Status::SignedIn, Some(true), "signed_in"),
            (Status::NotSignedIn, Some(false), "not_signed_in"),
            (Status::NotInstalled, None, "not_installed"),
            (Status::CouldNotCheck, None, "could_not_check"),
        ] {
            assert_eq!(status.logged_in(), logged_in);
            assert_eq!(serde_json::to_value(status).unwrap(), json);
        }
    }
}
