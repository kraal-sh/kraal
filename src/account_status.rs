//! Sign-in checks shared by the account list and login panes. The agent's output stays private.

use std::process::{Command, Stdio};
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

pub fn check(account: &Account) -> Status {
    match accounts::login_status(account) {
        Ok(launch) => run(&launch, Duration::from_secs(10)),
        Err(_) => Status::CouldNotCheck,
    }
}

fn run(launch: &Launch, limit: Duration) -> Status {
    let mut cmd = Command::new(&launch.argv[0]);
    cmd.args(&launch.argv[1..]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    for key in &launch.env_remove {
        cmd.env_remove(key);
    }
    cmd.envs(launch.env.iter().cloned());
    let child = match crate::exec::own_group(&mut cmd).spawn() {
        Ok(child) => child,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Status::NotInstalled,
        Err(_) => return Status::CouldNotCheck,
    };
    let start = Instant::now();
    loop {
        // Keep the pid until the whole group is stopped, including a wrapper's children.
        if crate::exec::has_exited(&child) {
            let status = crate::exec::kill_group_nowait(child);
            return match status.and_then(|s| s.code()) {
                Some(0) => Status::SignedIn,
                Some(1) => Status::NotSignedIn,
                _ => Status::CouldNotCheck,
            };
        }
        if start.elapsed() >= limit {
            crate::exec::kill_group_nowait(child);
            return Status::CouldNotCheck;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn checks_use_the_accounts_environment() {
        let launch = Launch {
            argv: vec!["sh".into(), "-c".into(), "test \"$KRAAL_STATUS_HOME\" = account && test -z \"$HOME\"".into()],
            env: vec![("KRAAL_STATUS_HOME".into(), "account".into())],
            env_remove: vec!["HOME".into()],
        };
        assert_eq!(run(&launch, crate::testing::PATIENCE), Status::SignedIn);
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
