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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub email: String,
    context: std::collections::BTreeMap<String, String>,
}

impl Identity {
    pub(crate) fn from_status(v: &serde_json::Value) -> Option<Self> {
        let email =
            v["email"].as_str().filter(|s| s.len() <= 120 && s.contains('@') && !s.chars().any(char::is_control))?;
        let mut context = std::collections::BTreeMap::new();
        for key in ["orgId", "orgName", "orgUuid", "organization", "organizationId", "plan", "subscriptionType"] {
            if let Some(value) = v.get(key).filter(|v| !v.is_null()) {
                let value =
                    value.as_str().filter(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))?;
                context.insert(key.into(), value.into());
            }
        }
        Some(Self { email: email.into(), context })
    }
}

#[derive(Debug, Clone)]
pub struct Checked {
    pub status: Status,
    pub identity: Option<Identity>,
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
        .and_then(|v| Identity::from_status(&v));
    Checked { status, identity }
}

pub fn inspect_accounts(list: &[Account]) -> Vec<(Account, Checked)> {
    std::thread::scope(|scope| {
        let tasks: Vec<_> = list.iter().map(|a| (a, scope.spawn(move || inspect(a)))).collect();
        tasks
            .into_iter()
            .map(|(a, task)| {
                (a.clone(), task.join().unwrap_or(Checked { status: Status::CouldNotCheck, identity: None }))
            })
            .collect()
    })
}

/// Only unchanged homes checked in this batch may supply a unique match.
pub fn reconcile(
    all: &mut accounts::Accounts,
    existing: &Account,
    checked: &Checked,
    named: &[(Account, Checked)],
) -> anyhow::Result<Option<String>> {
    if existing.sign_in_required
        || all
            .by_id(&existing.id)
            .is_none_or(|a| a.sign_in_required || a.login_revision != existing.login_revision || !a.same_home(existing))
    {
        return Ok(None);
    }
    let current = all.for_agent(&existing.agent);
    if checked.status == Status::SignedIn || checked.status == Status::CouldNotCheck && !current.is_empty() {
        all.adopt_checked(existing, checked.identity.as_ref().map(|i| i.email.as_str()))?;
    }
    let Some(pending) = all.by_id(&existing.id).filter(|a| a.review && a.same_home(existing)) else {
        return Ok(None);
    };
    let Some(identity) = checked.identity.as_ref().filter(|_| checked.status == Status::SignedIn) else {
        return Ok(None);
    };
    // An unchecked home could hide a second match, including one changed during the check.
    if current.iter().any(|a| {
        a.sign_in_required
            || !named.iter().any(|(b, result)| a == b && result.status == Status::SignedIn && result.identity.is_some())
    }) {
        return Ok(None);
    }
    let matches: Vec<_> = current
        .iter()
        .filter(|a| {
            named.iter().any(|(b, result)| {
                *a == b && result.status == Status::SignedIn && result.identity.as_ref() == Some(identity)
            })
        })
        .collect();
    if let [target] = matches.as_slice() {
        let target = all.review(&pending, Some(target))?;
        return Ok(Some(format!(
            "{} is {} · {}",
            crate::paths::shorten_home(&crate::paths::expand_tilde(&existing.home)),
            if target.agent == "claude" { "Claude" } else { "Codex" },
            target.name
        )));
    }
    Ok(None)
}

/// Hold the login locks through the account save, after revalidating the inspected revisions.
/// A login that already holds a home lock wins without making discovery wait for it.
pub fn save_discovery(
    checked: &[(Account, Checked)],
    named: &[(Account, Checked)],
    source_written: impl FnMut(&crate::workspace::migration::SourceWrite),
) -> anyhow::Result<Vec<String>> {
    let mut locks = Vec::new();
    accounts::Accounts::update_with_sources(
        &accounts::Accounts::path(),
        |all| {
            let mut notices = Vec::new();
            for (existing, result) in checked {
                if !matches!(result.status, Status::SignedIn | Status::CouldNotCheck) {
                    continue;
                }
                let homes =
                    std::iter::once(existing).chain(named.iter().map(|(a, _)| a).filter(|a| a.agent == existing.agent));
                let held: Result<Vec<_>, _> = homes.map(accounts::lock_login).collect();
                let Ok(held) = held else { continue };
                locks.extend(held);
                notices.extend(reconcile(all, existing, result, named)?);
            }
            Ok(notices)
        },
        source_written,
    )
}

/// Verify first, then re-read under the accounts lock before publishing any discovery.
pub fn discover(all: &accounts::Accounts) -> anyhow::Result<Vec<(Account, Status)>> {
    let candidates: Vec<_> = all.unlisted().into_iter().chain(all.reviews()).collect();
    let mut homes = candidates.clone();
    homes.extend(all.all().into_iter().filter(|a| candidates.iter().any(|b| a.agent == b.agent)));
    let mut checked = inspect_accounts(&homes);
    let named = checked.split_off(candidates.len());
    let notices = save_discovery(&checked, &named, |_| {})?;
    for notice in notices {
        eprintln!("{notice}");
    }
    Ok(checked.into_iter().map(|(a, c)| (a, c.status)).collect())
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
            assert_eq!(result.identity.as_ref().map(|i| i.email.as_str()), expected);
            assert!(!format!("{result:?}").contains("PRIVATE"));
        }
    }

    #[test]
    fn public_identity_keeps_provider_context_and_rejects_malformed_context() {
        for (extra, expected) in [
            (r#", "orgId":"org-1", "orgName":"Work", "subscriptionType":"max""#, true),
            (r#", "orgId":null, "subscriptionType":null"#, true),
            (r#", "orgId":42"#, false),
            (r#", "subscriptionType":"max\nPRIVATE""#, false),
        ] {
            let launch = Launch {
                argv: vec![
                    "sh".into(),
                    "-c".into(),
                    format!(r#"printf '%s' '{{"loggedIn":true,"email":"me@example.com"{extra}}}'"#),
                ],
                ..Default::default()
            };
            let checked = inspect_launch(&launch, crate::testing::PATIENCE, "claude");
            assert_eq!(checked.identity.is_some(), expected, "{extra}");
            if extra.contains("org-1") {
                let identity = checked.identity.unwrap();
                assert_eq!(identity.context["orgId"], "org-1");
                assert_eq!(identity.context["orgName"], "Work");
                assert_eq!(identity.context["subscriptionType"], "max");
            }
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
            assert_eq!(checked.identity.as_ref().map(|i| i.email.as_str()), (code == 0).then_some("me@example.com"));
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
