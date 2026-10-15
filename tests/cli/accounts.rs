use super::support::{
    PATIENCE, RawClient, account_switch_agent, click_account_text, existing_sign_in_agent, login_agent, raw_request,
};
use crate::common;
use crate::common::{Env, claude_hook, git_repo, wait_until, write_script};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn account_rules_see_a_worktree_anywhere_as_its_repository() {
    let env = Env::new("acctrule");
    let repo = env.root.join("company/repo");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["commit", "-q", "--allow-empty", "-m", "init"]);
    // Linked worktrees far from the repository: only git can tell where they belong.
    let feature = env.root.join("elsewhere/feature");
    let other = env.root.join("elsewhere/other");
    git(&["worktree", "add", "-q", "-b", "feature", feature.to_str().unwrap()]);
    git(&["worktree", "add", "-q", "-b", "other", other.to_str().unwrap()]);
    // A stand-in for claude that says which config home it was given.
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(
        &bin.join("claude"),
        "#!/bin/sh\n[ \"$2\" = status ] && exit 1\necho \"home=${CLAUDE_CONFIG_DIR:-default}\"\nexec sleep 100\n",
    );
    let pro = env.root.join("pro-home");
    std::fs::create_dir_all(&pro).unwrap();
    let repo = std::fs::canonicalize(&repo).unwrap();
    std::fs::write(
        env.root.join("config/kraal/accounts.toml"),
        format!(
            "[[account]]\nid = 'test-claude-pro'\nname = \"pro\"\nagent = \"claude\"\nhome = {:?}\n\n[[rule]]\npath = {:?}\naccounts = [\"pro\"]\n",
            pro.display().to_string(),
            repo.display().to_string()
        ),
    )
    .unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let run = |args: &[&str], dir: &Path| {
        let out = env.cmd(args).env("PATH", &path).current_dir(dir).output().unwrap();
        assert!(out.status.success(), "kraal {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    // From a definition: the pane waits for git, then starts on the account the rule names.
    env.workspace("w", &format!("layout {{\n    pane name=\"ag\" agent=\"claude\" cwd={:?}\n}}\n", feature.display()));
    run(&["open", "w"], &env.root);
    env.wait_for_text("ag", &format!("home={}", pro.display()));
    // From the CLI inside the session, in the other worktree: the same.
    let socket = env.root.join("run/kraal/acctrule.sock");
    let out = env
        .cmd(&["agent", "claude", "--tab"])
        .env("PATH", &path)
        .env("KRAAL_SOCKET", &socket)
        .current_dir(&other)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    // It prints the id of the pane it opened, as `kraal run` does.
    let opened = String::from_utf8(out.stdout).unwrap();
    let pane = panes.as_array().unwrap().iter().find(|p| format!("{}\n", p["id"]) == opened);
    let dir = pane.and_then(|p| p["cwd"].as_str()).map(|dir| Path::new(dir).canonicalize().unwrap());
    assert_eq!(dir, Some(other.canonicalize().unwrap()), "{opened:?} among {panes}");
    let ids: Vec<String> = panes.as_array().unwrap().iter().map(|p| p["id"].to_string()).collect();
    let start = Instant::now();
    loop {
        let screens: Vec<String> = ids.iter().map(|id| env.run(&["read", "-p", id.as_str()])).collect();
        if screens.iter().filter(|s| s.contains(&format!("home={}", pro.display()))).count() == 2 {
            break;
        }
        assert!(
            !screens.iter().any(|s| s.contains("home=default")),
            "an agent started on an account the rule forbids: {screens:?}"
        );
        assert!(start.elapsed() < Duration::from_secs(10), "{screens:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn choosing_the_current_account_keeps_running_and_exited_panes_as_they_are() {
    let env = Env::new("sameacct");
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let mut accounts = String::new();
    for agent in ["claude", "codex"] {
        write_script(
            &bin.join(agent),
            "#!/bin/sh\n[ \"$1\" = app-server ] && exit 0\n[ \"$2\" = status ] && exit 0\nprintf '%s\\n' \"$$\" >> \"$HOME/starts\"\necho READY\nwhile IFS= read -r line; do\n    [ \"$line\" = quit ] && exit 0\n    printf 'reply:%s\\n' \"$line\"\ndone\n",
        );
        for name in ["other", "work"] {
            let home = env.root.join(format!("{agent}-{name}-home"));
            std::fs::create_dir_all(&home).unwrap();
            accounts.push_str(&format!(
                "[[account]]\nid = 'test-{agent}-{name}'\nname = {name:?}\nagent = {agent:?}\nhome = {:?}\n",
                home.display().to_string()
            ));
        }
    }
    std::fs::write(env.root.join("config/kraal/accounts.toml"), accounts).unwrap();
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let socket = env.root.join("run/kraal/sameacct.sock");
    for (agent, size) in [("claude", (124, 36)), ("codex", (80, 24))] {
        let out = env
            .cmd(&["agent", agent, "--account", "work", "--tab"])
            .env("PATH", &path)
            .env("KRAAL_SOCKET", &socket)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let pane = String::from_utf8(out.stdout).unwrap().trim().to_string();
        let pane_id: u32 = pane.parse().unwrap();
        env.wait_for_text(&pane, "READY");
        let starts = std::fs::read_to_string(env.root.join("home/starts")).unwrap();
        let mut client = RawClient::attach_sized(&env, Some("w"), size).traced();
        for exited in [false, true] {
            client.send(b"\x02Ac\x1b[B");
            let rows = client.wait_for_row("Enter Keep current account");
            assert!(rows.iter().any(|r| r.contains(&format!("{agent}·work")) && r.contains("current")));
            assert!(!rows.iter().any(|r| r.contains("more actions")));
            // Tab moves to the other account rather than opening launch or login actions.
            client.send(b"\t");
            let rows = client.wait_for_row("Enter Switch account");
            assert!(!rows.iter().any(|r| r.contains("more actions")));
            client.send(b"\x1b[A");
            client.wait_for_row("Enter Keep current account");
            client.send(b"\r");
            client.wait_for_rows("the account picker closed", |rows| {
                !rows.iter().any(|r| r.contains("Enter Keep current account"))
            });
            if !exited {
                client.send(b"alive\r");
                env.wait_for_text(&pane, "reply:alive");
            }
            let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
            let info = panes.as_array().unwrap().iter().find(|p| p["id"] == pane_id).unwrap();
            assert_eq!(!info["exit_code"].is_null(), exited);
            assert_eq!(std::fs::read_to_string(env.root.join("home/starts")).unwrap(), starts, "{agent} restarted");
            if !exited {
                client.send(b"quit\r");
                client.wait_for_row("exited (0)");
            }
        }
    }
}

#[test]
fn an_account_switch_never_resends_a_prompt_by_itself_and_a_failed_transfer_resumes_the_source() {
    let env = Env::new("switchacct");
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    // A stand-in for claude that says which config home and arguments it was given.
    write_script(
        &bin.join("claude"),
        r#"#!/bin/sh
[ "$2" = status ] && exit 0
echo "home=${CLAUDE_CONFIG_DIR:-default} args=$*"
if [ "$1" = --session-id ]; then
    mkdir -p "$CLAUDE_CONFIG_DIR/projects/-work"
    printf '{}\n' > "$CLAUDE_CONFIG_DIR/projects/-work/$2.jsonl"
fi
while IFS= read -r line; do :; done
"#,
    );
    let (alpha, beta) = (env.root.join("alpha-home"), env.root.join("beta-home"));
    std::fs::create_dir_all(&alpha).unwrap();
    std::fs::create_dir_all(beta.join("projects")).unwrap();
    std::fs::write(
        env.root.join("config/kraal/accounts.toml"),
        format!(
            "[[account]]\nid = 'test-claude-alpha'\nname = \"alpha\"\nagent = \"claude\"\nhome = {:?}\n\n[[account]]\nid = 'test-claude-beta'\nname = \"beta\"\nagent = \"claude\"\nhome = {:?}\n",
            alpha.display().to_string(),
            beta.display().to_string()
        ),
    )
    .unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let socket = env.root.join("run/kraal/switchacct.sock");
    let agent = |args: &[&str]| -> String {
        let mut all = vec!["agent", "claude", "--account", "alpha", "--tab", "--"];
        all.extend_from_slice(args);
        let out = env.cmd(&all).env("PATH", &path).env("KRAAL_SOCKET", &socket).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let p = panes.as_array().unwrap().iter().find(|p| p["command"].to_string().contains(args[0])).unwrap();
        p["id"].to_string()
    };
    let info = |id: &str| -> serde_json::Value {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let id: u64 = id.parse().unwrap();
        panes.as_array().unwrap().iter().find(|p| p["id"] == id).unwrap().clone()
    };
    let on_alpha = format!("home={}", alpha.display());
    let on_beta = format!("home={}", beta.display());
    let mut client = RawClient::attach(&env, "w").traced();

    // 1. An option kraal does not know, then a word: its value, or a prompt. Switching stops the
    // agent and prepares it on the other account, but shows the command: Enter runs it.
    let odd = agent(&["--brand-new-flag", "Deploy now"]);
    env.wait_for_text(&odd, &on_alpha);
    env.run(&["focus", &odd]);
    client.send(b"\x02Ac");
    client.wait_for_row("Change account");
    client.send(b"beta");
    client.wait_for_row("1/");
    client.send(b"\r");
    client.wait_for_row("Stop and switch?");
    client.send(b"y");
    wait_until(Duration::from_secs(10), "the switched agent waits", || info(&odd)["suspended"] == true);
    let screen = env.wait_for_text(&odd, "cannot tell");
    assert!(!screen.contains(&on_beta), "it ran by itself: {screen}");
    client.send(b"\r");
    env.wait_for_text(&odd, &on_beta);
    let screen = env.run(&["read", "-p", &odd]).replace('\n', "");
    assert!(screen.contains("--brand-new-flag Deploy now"), "everything after the unknown option stays: {screen}");

    // 2. If copying fails after stopping, the source resumes without another keystroke.
    let plain = agent(&["--verbose"]);
    env.wait_for_text(&plain, &on_alpha);
    let command: Vec<String> =
        info(&plain)["command"].as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_string()).collect();
    let session = command.iter().skip_while(|a| *a != "--session-id").nth(1).expect("a session id").clone();
    std::fs::create_dir_all(alpha.join("projects/-work")).unwrap();
    std::fs::write(alpha.join(format!("projects/-work/{session}.jsonl")), "{}\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    // The first switch already created this project: it, not its parent, receives the copy.
    std::fs::set_permissions(beta.join("projects/-work"), std::fs::Permissions::from_mode(0o555)).unwrap();
    env.run(&["focus", &plain]);
    client.send(b"\x02Ac");
    client.wait_for_row("Change account");
    client.send(b"beta");
    client.wait_for_row("1/");
    client.send(b"\r");
    client.wait_for_row("Stop and switch?");
    client.send(b"y");
    // Lines may wrap in the pane: compare without line ends.
    let resumed = format!("{on_alpha} args=--resume {session} --verbose");
    wait_until(Duration::from_secs(10), "it resumes on alpha", || {
        env.run(&["read", "-p", &plain]).replace('\n', "").contains(&resumed)
    });
    assert_eq!(info(&plain)["suspended"], false);
    assert!(info(&plain)["exit_code"].is_null());
    assert!(!env.run(&["read", "-p", &plain]).replace('\n', "").contains(&on_beta));
    std::fs::set_permissions(beta.join("projects/-work"), std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn choose_switch_target(client: &mut RawClient, effect: &str) {
    client.send(b"\x02Ac");
    client.wait_for_row("Change account");
    client.send(b"beta");
    client.wait_for_rows("the target account's effect", |rows| {
        rows.iter().any(|r| r.contains("beta") && r.contains(effect) && !r.contains("checking sign-in"))
    });
    client.send(b"\r");
}

fn account_still_runs(env: &Env, pane: &str, message: &str) {
    env.run(&["send", "-p", pane, &format!("{message}\r")]);
    env.wait_for_text(pane, &format!("reply:{message}"));
    assert_eq!(std::fs::read_to_string(env.root.join("alpha/starts")).unwrap().lines().count(), 1);
    assert!(!env.root.join("beta/starts").exists());
}

#[test]
fn an_unsigned_target_offers_sign_in_without_stopping_the_agent() {
    let env = Env::new("switchlogin");
    let pane = account_switch_agent(&env, "claude", "notify");
    std::fs::write(env.root.join("beta/status"), "1").unwrap();
    let mut client = RawClient::attach_sized(&env, Some("w"), (80, 24)).traced();
    choose_switch_target(&mut client, "Sign-in required");
    client.wait_for_row("The current agent keeps running");
    client.send(b"\r");
    account_still_runs(&env, &pane, "cancel-sign-in");
    assert!(!env.root.join("beta/logins").exists());
    choose_switch_target(&mut client, "Sign-in required");
    client.wait_for_row("The current agent keeps running");
    client.send(b"y");
    wait_until(PATIENCE, "sign-in was offered and started", || {
        common::read_complete_lines(env.root.join("beta/logins")).is_some()
    });
    account_still_runs(&env, &pane, "during-sign-in");
}

#[test]
fn unverified_accounts_and_broken_transcripts_never_stop_the_current_agent() {
    use std::os::unix::fs::PermissionsExt;
    for reason in ["status", "missing", "unreadable", "diverged"] {
        let env = Env::new(&format!("switch-{reason}"));
        let pane = account_switch_agent(&env, "claude", "notify");
        let transcript =
            std::fs::read_dir(env.root.join("alpha/projects/work")).unwrap().next().unwrap().unwrap().path();
        let expected = match reason {
            "status" => {
                std::fs::write(env.root.join("beta/status"), "2").unwrap();
                "could not check sign-in"
            }
            "missing" => {
                std::fs::remove_file(&transcript).unwrap();
                "is missing"
            }
            "unreadable" => {
                std::fs::set_permissions(&transcript, std::fs::Permissions::from_mode(0o000)).unwrap();
                if std::fs::read(&transcript).is_ok() {
                    continue;
                }
                "reading"
            }
            _ => {
                std::fs::write(env.root.join("beta/projects/work").join(transcript.file_name().unwrap()), "diverged\n")
                    .unwrap();
                "each went on"
            }
        };
        let mut client = RawClient::attach_sized(&env, Some("w"), (124, 36)).traced();
        choose_switch_target(&mut client, "Could not verify");
        client.wait_for_screen(expected);
        account_still_runs(&env, &pane, reason);
        if reason == "status" {
            client.wait_for_row("Check again");
            std::fs::write(env.root.join("beta/status"), "0").unwrap();
            client.send(b"y");
            client.wait_for_row("Stop and switch?");
            client.send(b"\r");
            account_still_runs(&env, &pane, "after-check-again");
        }
    }
}

#[test]
fn cancelling_a_check_from_another_client_ignores_its_late_success() {
    let env = Env::new("switchcheckcancel");
    let pane = account_switch_agent(&env, "claude", "notify");
    let hold = env.root.join("beta/hold-check");
    std::fs::write(&hold, "").unwrap();
    let mut first = RawClient::attach(&env, "w").traced();
    let mut second = RawClient::attach(&env, "w").traced();
    first.send(b"\x02Ac");
    first.wait_for_row("Change account");
    first.send(b"beta");
    first.wait_for_row("checking sign-in");
    first.send(b"\r");
    wait_until(PATIENCE, "both the picker and change check sign-in", || {
        common::read_complete_lines(env.root.join("beta/checks")).is_some_and(|s| s.lines().count() == 2)
    });
    second.send(b"\x02Ac");
    second.wait_for_row("Checking sign-in");
    second.send(b"\r");
    second
        .wait_for_rows("the pending change is cancelled", |rows| !rows.iter().any(|r| r.contains("Checking sign-in")));
    std::fs::remove_file(hold).unwrap();
    wait_until(PATIENCE, "both checks returned", || {
        common::read_complete_lines(env.root.join("beta/checked")).is_some_and(|s| s.lines().count() == 2)
    });
    account_still_runs(&env, &pane, "late-success");
    first.send(b"\x02Ac");
    first.wait_for_row("Change account");
    first.send(b"beta");
    first.wait_for_row("Continue conversation");
}

#[test]
fn a_switch_waits_for_the_turn_is_cancelled_by_another_client_and_rechecks_sign_in() {
    let env = Env::new("switchturn");
    let pane = account_switch_agent(&env, "claude", "notify");
    claude_hook(&env, &pane, r#"{"hook_event_name":"UserPromptSubmit"}"#);
    let mut first = RawClient::attach_sized(&env, Some("w"), (124, 36)).traced();
    let mut second = RawClient::attach_sized(&env, Some("w"), (80, 24)).traced();
    choose_switch_target(&mut first, "Continue conversation");
    first.wait_for_row("Switch after this turn");
    first.send(b"\x02Ac");
    first.wait_for_row("Cancel account change");
    second.send(b"\x02Ac");
    second.wait_for_row("Cancel account change");
    second.send(b"\r");
    first.wait_for_rows("cancellation reaches both clients", |rows| {
        !rows.iter().any(|r| r.contains("Cancel account change"))
    });
    account_still_runs(&env, &pane, "after-cancel");

    choose_switch_target(&mut first, "Continue conversation");
    // The cancelled change's notification may still show: wait for this change's pending row.
    first.send(b"\x02Ac");
    first.wait_for_rows("the new account change waits for the turn", |rows| {
        rows.iter().any(|r| r.contains("Cancel account change") && r.contains("Switch after this turn"))
    });
    std::fs::write(env.root.join("beta/status"), "1").unwrap();
    claude_hook(&env, &pane, r#"{"hook_event_name":"Stop"}"#);
    first.wait_for_row("The current agent keeps running");
    first.send(b"\r");
    account_still_runs(&env, &pane, "expired-while-waiting");

    std::fs::write(env.root.join("beta/status"), "0").unwrap();
    choose_switch_target(&mut first, "Continue conversation");
    wait_until(PATIENCE, "the same conversation starts on beta", || {
        common::read_complete_lines(env.root.join("beta/starts")).is_some()
    });
    let text = env.wait_for_text(&pane, "--resume").replace('\n', "");
    assert!(text.contains(&format!("home={}", env.root.join("beta").display())));
    assert_eq!(std::fs::read_to_string(env.root.join("beta/starts")).unwrap().lines().count(), 1);
    let transcript = std::fs::read_dir(env.root.join("alpha/projects/work")).unwrap().next().unwrap().unwrap().path();
    assert_eq!(
        std::fs::read(&transcript).unwrap(),
        std::fs::read(env.root.join("beta/projects/work").join(transcript.file_name().unwrap())).unwrap()
    );
}

#[test]
fn codex_discloses_the_new_conversation_and_stop_confirmation_selects_cancel() {
    let env = Env::new("switchcodex");
    let pane = account_switch_agent(&env, "codex", "notify");
    let mut client = RawClient::attach_sized(&env, Some("w"), (80, 24)).traced();
    choose_switch_target(&mut client, "New conversation");
    client.wait_for_row("Stop and switch?");
    client.wait_for_row("New conversation");
    client.send(b"\r");
    account_still_runs(&env, &pane, "cancel-stop");
    choose_switch_target(&mut client, "New conversation");
    client.wait_for_row("Stop and switch?");
    client.send(b"y");
    wait_until(PATIENCE, "Codex starts on beta", || {
        common::read_complete_lines(env.root.join("beta/starts")).is_some()
    });
    env.wait_for_text(&pane, &format!("home={} args=", env.root.join("beta").display()));
    assert_eq!(std::fs::read_to_string(env.root.join("beta/starts")).unwrap().lines().count(), 1);
}

#[test]
fn frame_and_bottom_bar_open_the_same_change_and_claude_undo_continues_the_latest_history() {
    for size in [(80, 24), (124, 36)] {
        let env = Env::new(&format!("switch-undo-{}", size.0));
        let pane = account_switch_agent(&env, "claude", "notify");
        let mut client = RawClient::attach_sized(&env, Some("w"), size);
        click_account_text(&mut client, "switch account");
        client.wait_for_row("Change account");
        client.send(b"\x1b");
        // Two panes make the frame visible even with the compact single-pane setting.
        // Stack them so the account details fit at both terminal widths.
        env.run(&["run", "--down", "-n", "beside", "--", "sh"]);
        env.run(&["focus", &pane]);
        claude_hook(&env, &pane, r#"{"hook_event_name":"Stop"}"#);
        let rows = client.wait_for_rows("the account in the frame", |rows| {
            rows.iter().any(|r| r.contains("claude·alpha") && r.contains('─'))
        });
        let y = rows.iter().position(|r| r.contains("claude·alpha") && r.contains('─')).unwrap();
        let x = rows[y][..rows[y].find("claude·alpha").unwrap()].chars().count();
        client.send(format!("\x1b[<0;{};{}M\x1b[<0;{};{}m", x + 1, y + 1, x + 1, y + 1).as_bytes());
        client.wait_for_row("Change account");
        client.send(b"beta");
        client.wait_for_row("Continue conversation");
        client.send(b"\r");
        client.wait_for_row("Using beta · Undo");
        let transcript =
            std::fs::read_dir(env.root.join("beta/projects/work")).unwrap().next().unwrap().unwrap().path();
        use std::io::Write;
        std::fs::OpenOptions::new().append(true).open(&transcript).unwrap().write_all(b"{\"message\":2}\n").unwrap();
        claude_hook(&env, &pane, r#"{"hook_event_name":"Stop"}"#);
        click_account_text(&mut client, "Undo");
        client.wait_for_row("Account change undone");
        let original = env.root.join("alpha/projects/work").join(transcript.file_name().unwrap());
        assert_eq!(std::fs::read(original).unwrap(), std::fs::read(transcript).unwrap());
        let starts = std::fs::read_to_string(env.root.join("alpha/starts")).unwrap();
        assert_eq!(starts.lines().count(), 2);
        assert_eq!(std::fs::read_to_string(env.root.join("beta/starts")).unwrap().lines().count(), 1);
    }
}

#[test]
fn codex_undo_from_pane_actions_reopens_the_original_and_keeps_the_new_conversation() {
    let env = Env::new("codex-undo");
    let pane = account_switch_agent(&env, "codex", "notify");
    let mut client = RawClient::attach(&env, "w");
    choose_switch_target(&mut client, "New conversation");
    client.wait_for_row("Stop and switch?");
    client.send(b"y");
    client.wait_for_row("Using beta · Undo");
    let newer = env.root.join("beta/sessions/2026/10/13/rollout-today-conversation-beta.jsonl");
    let newer_contents = std::fs::read(&newer).unwrap();
    // The action remains after its notification is dismissed.
    client.send(b"\x02p");
    client.send(b"codex\t");
    client.wait_for_row("Undo account change");
    click_account_text(&mut client, "Undo account change");
    client.wait_for_row("Stop and switch?");
    client.wait_for_row("Continue conversation");
    client.send(b"y");
    env.wait_for_text(&pane, "args=resume conversation-alpha");
    client.wait_for_row("Account change undone");
    assert_eq!(std::fs::read(newer).unwrap(), newer_contents);
    assert_eq!(std::fs::read_to_string(env.root.join("beta/starts")).unwrap().lines().count(), 1);
    for name in ["alpha", "beta"] {
        assert!(env.root.join(format!("{name}/sessions/2026/10/13/rollout-today-conversation-{name}.jsonl")).is_file());
    }
}

#[test]
fn a_replacement_that_exits_before_starting_resumes_the_source_once() {
    for recovery_fails in [false, true] {
        let env = Env::new(if recovery_fails { "switch-double-failure" } else { "switch-recovery" });
        let pane = account_switch_agent(&env, "claude", "notify");
        let mut client = RawClient::attach(&env, "w");
        std::fs::write(env.root.join("beta/fail-start"), "").unwrap();
        if recovery_fails {
            std::fs::write(env.root.join("alpha/fail-start"), "").unwrap();
        }
        claude_hook(&env, &pane, r#"{"hook_event_name":"Stop"}"#);
        choose_switch_target(&mut client, "Continue conversation");
        if recovery_fails {
            client.wait_for_row("Couldn't resume.");
        } else {
            client.wait_for_row("Resumed on alpha.");
            env.run(&["send", "-p", &pane, "still-here\r"]);
            env.wait_for_text(&pane, "reply:still-here");
        }
        assert_eq!(std::fs::read_to_string(env.root.join("alpha/starts")).unwrap().lines().count(), 2);
        assert_eq!(std::fs::read_to_string(env.root.join("beta/starts")).unwrap().lines().count(), 1);
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let info = panes.as_array().unwrap().iter().find(|p| p["id"].as_u64() == pane.parse::<u64>().ok()).unwrap();
        assert_eq!(info["account"], "claude:alpha");
        assert_eq!(!info["exit_code"].is_null(), recovery_fails);
    }
}

#[test]
fn a_notify_threshold_opens_change_for_the_pane_that_reached_it() {
    let env = Env::new("switch-notify-action");
    let pane = account_switch_agent(&env, "claude", "notify");
    let mut client = RawClient::attach_sized(&env, Some("w"), (80, 24));
    env.run(&["run", "-n", "beside", "--", "sh"]);
    write_account_usage(&env, "alpha", 95);
    client.wait_for_row("alpha is at 95% used · Change account");
    click_account_text(&mut client, "Change account");
    client.wait_for_row("Change account");
    client.wait_for_row("current · Continue conversation");
    client.send(b"\x1b");
    account_still_runs(&env, &pane, "only-notified");
}

#[test]
fn a_shell_agent_can_switch_only_with_an_exact_account_and_saved_conversation() {
    for agent in ["claude", "codex"] {
        let env = Env::new(&format!("manual-switch-{agent}"));
        account_switch_agent(&env, agent, "notify");
        let mut client = RawClient::attach(&env, "w");
        env.run(&["focus", "shell"]);
        let variable = if agent == "claude" { "CLAUDE_CONFIG_DIR" } else { "CODEX_HOME" };
        let args = if agent == "claude" { "--session-id manual-session" } else { "" };
        let command = format!(
            "{variable}='{}' '{}' {args}\r",
            env.root.join("alpha").display(),
            env.root.join(format!("bin/{agent}")).display()
        );
        env.run(&["send", "-p", "shell", &command]);
        env.wait_for_text("shell", "READY");
        if agent == "codex" {
            // The fake reports its conversation id through the hook after its first turn.
            env.run(&["send", "-p", "shell", "first turn\r"]);
            env.wait_for_text("shell", "reply:first turn");
        }
        client.wait_for_row("switch account");
        choose_switch_target(&mut client, if agent == "claude" { "Continue conversation" } else { "New conversation" });
        client.wait_for_row("Stop and switch?");
        client.send(b"y");
        client.wait_for_row("Using beta · Undo");
        env.run(&["send", "-p", "shell", "new-account\r"]);
        env.wait_for_text("shell", "reply:new-account");
        assert_eq!(
            std::fs::read_to_string(env.root.join("alpha/starts")).unwrap().lines().count(),
            2,
            "the original managed agent also keeps running"
        );
        assert_eq!(std::fs::read_to_string(env.root.join("beta/starts")).unwrap().lines().count(), 1);
    }
}

#[test]
fn an_ambiguous_shell_agent_offers_another_pane_without_stopping_it() {
    for reason in ["missing-session", "credential", "wrapper", "background-job"] {
        let env = Env::new(&format!("manual-refused-{reason}"));
        account_switch_agent(&env, "claude", "notify");
        let mut client = RawClient::attach(&env, "w");
        env.run(&["focus", "shell"]);
        let program = env.root.join("bin/claude");
        let args = if reason == "missing-session" { "" } else { "--session-id manual-session" };
        let extra = if reason == "credential" { "ANTHROPIC_API_KEY=override " } else { "" };
        if reason == "background-job" {
            env.run(&["send", "-p", "shell", "sleep 60 &\r"]);
        }
        let command = if reason == "wrapper" {
            format!(
                "CLAUDE_CONFIG_DIR='{}' sh -c '\"{}\" {args} </dev/tty & wait'\r",
                env.root.join("alpha").display(),
                program.display()
            )
        } else {
            format!("{extra}CLAUDE_CONFIG_DIR='{}' '{}' {args}\r", env.root.join("alpha").display(), program.display())
        };
        env.run(&["send", "-p", "shell", &command]);
        env.wait_for_text("shell", "READY");
        client.send(b"\x02Ac");
        client.wait_for_row("Can't switch this process.");
        client.wait_for_row("Open with another account");
        assert_eq!(std::fs::read_to_string(env.root.join("alpha/starts")).unwrap().lines().count(), 2);
        assert!(!env.root.join("beta/starts").exists());
        client.send(b"\r");
        client.wait_for_row("Open with another account");
        client.send(b"beta\r");
        env.wait_for_text("shell", "READY");
        wait_until(PATIENCE, "another pane starts on beta", || {
            common::read_complete_lines(env.root.join("beta/starts")).is_some()
        });
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let original = panes.as_array().unwrap().iter().find(|p| p["name"] == "shell").unwrap();
        assert!(original["exit_code"].is_null());
        assert!(original["account"].is_null());
    }
}

#[test]
fn switching_waits_for_the_replacement_to_produce_output_before_offering_undo() {
    let env = Env::new("switch-start-signal");
    let pane = account_switch_agent(&env, "claude", "notify");
    let mut client = RawClient::attach(&env, "w");
    std::fs::write(env.root.join("beta/hold-start"), "").unwrap();
    claude_hook(&env, &pane, r#"{"hook_event_name":"Stop"}"#);
    choose_switch_target(&mut client, "Continue conversation");
    wait_until(PATIENCE, "the replacement was spawned", || {
        common::read_complete_lines(env.root.join("beta/starts")).is_some()
    });
    std::thread::sleep(Duration::from_secs(2));
    assert!(!client.rows().iter().any(|r| r.contains("Using beta") || r.contains("Switched")));
    std::fs::remove_file(env.root.join("beta/hold-start")).unwrap();
    client.wait_for_row("Using beta · Undo");
}

fn write_account_usage(env: &Env, name: &str, percent: u8) {
    let dir = env.root.join("state/kraal/usage/by-id");
    std::fs::create_dir_all(&dir).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let file: toml::Value =
        toml::from_str(&std::fs::read_to_string(env.root.join("config/kraal/accounts.toml")).unwrap()).unwrap();
    let id = file["account"].as_array().unwrap().iter().find(|a| a["name"].as_str() == Some(name)).unwrap()["id"]
        .as_str()
        .unwrap();
    std::fs::write(
        dir.join(format!("claude-{id}.json")),
        serde_json::json!({
            "five_hour": { "used_percent": percent, "resets_at": now + 3600 },
            "weekly": null, "observed_at": now, "source": "statusline"
        })
        .to_string(),
    )
    .unwrap();
}

#[test]
fn threshold_ask_checks_the_destination_then_needs_one_confirmation() {
    let env = Env::new("switchask");
    let pane = account_switch_agent(&env, "claude", "ask");
    let mut client = RawClient::attach(&env, "w").traced();
    claude_hook(&env, &pane, r#"{"hook_event_name":"Stop"}"#);
    write_account_usage(&env, "beta", 0);
    write_account_usage(&env, "alpha", 95);
    client.wait_for_row("reached its usage threshold");
    client.wait_for_row("Stop and switch?");
    assert!(env.root.join("beta/checked").exists());
    assert!(!env.root.join("beta/starts").exists());
    client.send(b"y");
    wait_until(PATIENCE, "one confirmation starts the verified alternative", || {
        common::read_complete_lines(env.root.join("beta/starts")).is_some()
    });
    env.wait_for_text(&pane, "--resume");
    assert_eq!(std::fs::read_to_string(env.root.join("beta/starts")).unwrap().lines().count(), 1);
}

#[test]
fn automatic_switches_need_known_usage_and_do_not_retry_a_failed_check_each_tick() {
    let env = Env::new("switchauto");
    let pane = account_switch_agent(&env, "claude", "auto");
    let client = RawClient::attach(&env, "w").traced();
    claude_hook(&env, &pane, r#"{"hook_event_name":"Stop"}"#);
    let usage = |name, percent| write_account_usage(&env, name, percent);
    usage("alpha", 95);
    std::thread::sleep(Duration::from_secs(3));
    assert!(!env.root.join("beta/checks").exists(), "unknown usage is not free capacity");
    account_still_runs(&env, &pane, "unknown-usage");
    std::fs::write(env.root.join("beta/status"), "1").unwrap();
    usage("beta", 0);
    wait_until(PATIENCE, "the automatic switch checks sign-in", || {
        common::read_complete_lines(env.root.join("beta/checks")).is_some()
    });
    client.wait_for_row("The current agent keeps running");
    std::thread::sleep(Duration::from_secs(3));
    let checks = std::fs::read_to_string(env.root.join("beta/checks")).unwrap();
    assert_eq!(checks.lines().count(), 1);
    account_still_runs(&env, &pane, "failed-auto");
    std::fs::write(env.root.join("beta/status"), "0").unwrap();
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(std::fs::read_to_string(env.root.join("beta/checks")).unwrap(), checks);
    account_still_runs(&env, &pane, "no-retry");
}

fn open_account_login(client: &mut RawClient) {
    client.send(b"\x02Al");
    client.wait_for_row("Accounts");
    client.send(b"work");
    client.wait_for_row("work");
    client.send(b"\x0c");
    client.wait_for_row("login ");
}

fn login_panes(env: &Env) -> Vec<serde_json::Value> {
    let panes: Vec<serde_json::Value> = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    panes.into_iter().filter(|p| p["name"].as_str().is_some_and(|s| s.starts_with("login "))).collect()
}

#[test]
fn login_panes_verify_success_once_and_never_save_the_terminal() {
    for agent in ["claude", "codex"] {
        let env = Env::new(&format!("login-{agent}"));
        let (home, path) = login_agent(&env, agent);
        let mut client = RawClient::attach_sized(&env, Some("w"), (124, 36)).traced();
        open_account_login(&mut client);
        client.wait_for_row("PRIVATE-LOGIN-OUTPUT");
        let first = login_panes(&env);
        assert_eq!(first.len(), 1);
        let mut second = RawClient::attach_sized(&env, Some("w"), (80, 24)).traced();
        open_account_login(&mut second);
        assert_eq!(login_panes(&env)[0]["id"], first[0]["id"], "another client must reuse the login");
        assert_eq!(std::fs::read_to_string(home.join("logins")).unwrap(), "login\n");
        env.run(&["export", env.root.join("config/kraal/workspaces/saved.kdl").to_str().unwrap(), "--workspace", "w"]);
        let layout = std::fs::read_to_string(env.root.join("config/kraal/workspaces/saved.kdl")).unwrap();
        // The shell's cwd contains "login" too: check the pane name and command instead.
        assert!(!layout.contains("name=\"login "), "{layout}");
        assert!(!layout.contains("command="), "{layout}");
        assert!(!layout.contains("PRIVATE-LOGIN-OUTPUT"), "{layout}");
        std::fs::write(home.join("hold-check"), "").unwrap();
        std::fs::write(home.join("status"), "0").unwrap();
        std::fs::write(home.join("finish-login"), "0").unwrap();
        second.wait_for_row("checking sign-in");
        assert_eq!(login_panes(&env).len(), 1, "exit zero alone must not close the pane");
        let duplicate = env.cmd(&["accounts", "login", &format!("{agent}:work")]).env("PATH", &path).output().unwrap();
        assert!(!duplicate.status.success());
        assert!(String::from_utf8_lossy(&duplicate.stderr).contains("sign-in already running"));
        assert_eq!(std::fs::read_to_string(home.join("logins")).unwrap(), "login\n");
        std::fs::remove_file(home.join("hold-check")).unwrap();
        second.wait_for_row(&format!("{agent}·work is signed in"));
        common::wait_until(PATIENCE, "verified login closes", || login_panes(&env).is_empty());
        second.send(b"\r");
        assert_eq!(std::fs::read_to_string(home.join("logins")).unwrap(), "login\n");
        assert!(
            !std::fs::read_to_string(env.root.join("config/kraal/accounts.toml")).unwrap().contains("sign_in_required")
        );

        // A live login must also be excluded when the server saves on exit.
        std::fs::remove_file(home.join("finish-login")).unwrap();
        open_account_login(&mut second);
        second.wait_for_row("PRIVATE-LOGIN-OUTPUT");
        second.send(b"\x02Vf\x02Vj");
        let messages = second.wait_for_row("Messages").join("\n");
        assert!(!messages.contains("PRIVATE-LOGIN-OUTPUT"), "{messages}");
        env.run(&["kill"]);
        fn private_files(dir: &Path) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    private_files(&path);
                } else {
                    let text = String::from_utf8_lossy(&std::fs::read(&path).unwrap()).into_owned();
                    assert!(!text.contains("PRIVATE-LOGIN-OUTPUT"), "{}: {text}", path.display());
                    if path.extension().is_some_and(|e| e == "json") {
                        assert!(!text.contains("login "), "{}: {text}", path.display());
                    }
                }
            }
        }
        private_files(&env.root.join("state"));
    }
}

#[test]
fn failed_and_cancelled_logins_close_on_enter_and_retry_only_when_asked() {
    let env = Env::new("login-failed");
    let (home, path) = login_agent(&env, "claude");
    let mut client = RawClient::attach_sized(&env, Some("w"), (80, 24)).traced();
    std::fs::write(home.join("finish-login"), "1").unwrap();
    open_account_login(&mut client);
    client.wait_for_row("sign-in failed");
    client.wait_for_row("enter close · r try again");
    assert!(!home.join("checks").exists(), "failure does not run a status check");
    client.send(b"\r");
    common::wait_until(PATIENCE, "Enter closes failed login", || login_panes(&env).is_empty());
    assert_eq!(std::fs::read_to_string(home.join("logins")).unwrap(), "login\n");
    client.resize((124, 36));
    client.wait_for_row("Sign-in required");
    client.send(b"\x02Al");
    client.wait_for_row("Claude · work");
    client.wait_for_row("Sign-in required");
    client.close_overlay("Enter Details");

    std::fs::remove_file(home.join("finish-login")).unwrap();
    open_account_login(&mut client);
    client.wait_for_row("PRIVATE-LOGIN-OUTPUT");
    client.send(b"\x03");
    client.wait_for_row("sign-in cancelled");
    common::wait_until(PATIENCE, "cancelled program exits", || !login_panes(&env)[0]["exit_code"].is_null());
    client.send(b"r");
    common::wait_until(PATIENCE, "explicit retry starts once", || {
        let panes = login_panes(&env);
        common::read_complete_lines(home.join("logins")).is_some_and(|s| s.lines().count() == 3)
            && panes.len() == 1
            && panes[0]["exit_code"].is_null()
    });
    // Running logins show the native terminal, with no banner. Acknowledge the retry's
    // redraw before finishing it, so the old cancelled banner cannot satisfy the next wait.
    client.wait_for_rows("the retried login is running", |rows| {
        rows.iter().any(|r| r.contains("PRIVATE-LOGIN-OUTPUT")) && !rows.iter().any(|r| r.contains("sign-in cancelled"))
    });
    std::fs::write(home.join("finish-login"), "130").unwrap();
    client.wait_for_row("sign-in cancelled");
    client.send(b"\r");
    common::wait_until(PATIENCE, "Enter closes cancelled login", || login_panes(&env).is_empty());
    assert_eq!(std::fs::read_to_string(home.join("logins")).unwrap().lines().count(), 3);
    assert!(!home.join("checks").exists(), "cancellation does not run a status check");
    assert!(
        std::fs::read_to_string(env.root.join("config/kraal/accounts.toml"))
            .unwrap()
            .contains("sign_in_required = true")
    );
    // Listing accounts explicitly checks sign-in: do it after checking that neither
    // failure nor cancellation ran a status command.
    let out = env.cmd(&["accounts"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|l| l.contains("claude·work") && l.contains("sign-in required"))
    );
}

#[test]
fn an_unverified_login_can_check_again_without_another_login() {
    let env = Env::new("login-check");
    let (home, _) = login_agent(&env, "codex");
    let mut client = RawClient::attach_sized(&env, Some("w"), (80, 24)).traced();
    std::fs::write(home.join("finish-login"), "0").unwrap();
    std::fs::write(home.join("status"), "1").unwrap();
    open_account_login(&mut client);
    client.wait_for_row("sign-in required");
    client.wait_for_row("c check again");
    std::fs::write(home.join("status"), "2").unwrap();
    client.send(b"c");
    client.wait_for_row("could not check sign-in");
    std::fs::write(home.join("status"), "0").unwrap();
    client.send(b"c");
    client.wait_for_row("codex·work is signed in");
    assert_eq!(std::fs::read_to_string(home.join("logins")).unwrap(), "login\n");
    common::wait_until(PATIENCE, "verified retry closes", || login_panes(&env).is_empty());
}

#[test]
fn a_verified_login_that_cannot_save_can_finish_without_logging_in_again() {
    let env = Env::new("login-save");
    let (home, _) = login_agent(&env, "claude");
    let mut client = RawClient::attach(&env, "w").traced();
    std::fs::write(home.join("status"), "0").unwrap();
    open_account_login(&mut client);
    client.wait_for_row("PRIVATE-LOGIN-OUTPUT");
    let file = env.root.join("config/kraal/accounts.toml");
    let backup = file.with_extension("kept");
    std::fs::rename(&file, &backup).unwrap();
    std::fs::create_dir(&file).unwrap();
    // Make saving fail before the login exits, without holding a check against its timeout.
    std::fs::write(home.join("finish-login"), "0").unwrap();
    client.wait_for_row("signed in, could not save account");
    assert_eq!(login_panes(&env).len(), 1);
    assert!(std::fs::read_to_string(&backup).unwrap().contains("sign_in_required = true"));
    std::fs::remove_dir(&file).unwrap();
    std::fs::rename(&backup, &file).unwrap();
    client.send(b"c");
    client.wait_for_row("claude·work is signed in");
    assert!(login_panes(&env).is_empty(), "a saved login must be closed");
    assert_eq!(std::fs::read_to_string(home.join("logins")).unwrap(), "login\n");
    assert_eq!(std::fs::read_to_string(home.join("checks")).unwrap(), "check\nchecked\ncheck\nchecked\n");
    assert!(!std::fs::read_to_string(file).unwrap().contains("sign_in_required"));
}

#[test]
fn closing_a_check_ignores_its_late_success() {
    let env = Env::new("login-late");
    let (home, _) = login_agent(&env, "claude");
    let mut client = RawClient::attach(&env, "w").traced();
    std::fs::write(home.join("finish-login"), "0").unwrap();
    std::fs::write(home.join("status"), "0").unwrap();
    std::fs::write(home.join("hold-check"), "").unwrap();
    open_account_login(&mut client);
    client.wait_for_row("checking sign-in");
    common::wait_until(PATIENCE, "check is running", || common::read_complete_lines(home.join("checks")).is_some());
    client.send(b"\r");
    common::wait_until(PATIENCE, "checking pane closes", || login_panes(&env).is_empty());
    std::fs::write(home.join("status"), "2").unwrap();
    open_account_login(&mut client);
    client.wait_for_row("checking sign-in");
    common::wait_until(PATIENCE, "the replacement check read its status", || {
        common::read_complete_lines(home.join("checks"))
            .is_some_and(|s| s.lines().filter(|l| *l == "check").count() == 2)
    });
    std::fs::remove_file(home.join("hold-check")).unwrap();
    client.wait_for_row("could not check sign-in");
    assert_eq!(login_panes(&env).len(), 1);
    assert!(
        std::fs::read_to_string(env.root.join("config/kraal/accounts.toml"))
            .unwrap()
            .contains("sign_in_required = true")
    );
    assert!(!client.rows().iter().any(|r| r.contains("is signed in")));
}

#[test]
fn accounts_cli_expires_usage_without_changing_the_saved_observation() {
    let env = Env::new("usage-expiry");
    let path = existing_sign_in_agent(&env, "claude");
    existing_sign_in_agent(&env, "codex");
    let home = env.root.join("home/work");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("status"), "0").unwrap();
    std::fs::write(
        env.root.join("config/kraal/accounts.toml"),
        "[[account]]\nid = 'work'\nname = 'work'\nagent = 'claude'\nhome = '~/work'\n",
    )
    .unwrap();
    let run = |json| {
        let args = if json { vec!["accounts", "list", "--json"] } else { vec!["accounts", "list"] };
        let out = env.cmd(&args).env("PATH", &path).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    };
    assert!(run(false).contains("no usage seen yet"));
    let dir = env.root.join("state/kraal/usage/by-id");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("claude-work.json");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    for (age, reset, reported, percentages) in [
        (0, None, false, vec![]),
        (0, None, true, vec!["95%", "71%"]),
        (6 * 3600, None, true, vec!["0%", "71%"]),
        (8 * 86400, None, true, vec!["0%", "0%"]),
        (0, Some(now - 60), true, vec!["0%", "0%"]),
        (8 * 86400, Some(now + 86400), true, vec!["95%", "71%"]),
    ] {
        let window = |percent| reported.then(|| serde_json::json!({"used_percent": percent, "resets_at": reset}));
        let usage = serde_json::json!({
            "five_hour": window(95.0), "weekly": window(71.0),
            "observed_at": now - age, "source": "statusline",
        });
        let saved = usage.to_string();
        std::fs::write(&file, &saved).unwrap();
        let text = run(false);
        let row = text.lines().find(|line| line.starts_with("claude·work")).unwrap();
        if reported {
            let shown: Vec<_> = row.split(" · ").take(2).map(|part| part.split_whitespace().last().unwrap()).collect();
            assert_eq!(shown, percentages, "{usage}: {row}");
        } else {
            assert!(row.ends_with("no usage seen yet"), "{row}");
        }
        let list: serde_json::Value = serde_json::from_str(&run(true)).unwrap();
        assert_eq!(list[0]["usage"], usage);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), saved);
    }
}

fn wait_for_account_review(client: &RawClient, agent: &str) -> Vec<String> {
    let home = format!("~/.{}", agent.to_lowercase());
    client.wait_for_rows(&format!("{home} ready for Review"), |rows| {
        rows.iter()
            .any(|row| row.contains(&home) && (row.contains("? Which account is") || row.contains("? Account for")))
    })
}

#[test]
fn account_discovery_updates_each_agent_without_waiting_for_the_other() {
    let env = Env::new("accounts-concurrent");
    let path = existing_sign_in_agent(&env, "claude");
    existing_sign_in_agent(&env, "codex");
    for agent in ["claude", "codex"] {
        let home = env.root.join(format!("home/.{agent}"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("status"), "0").unwrap();
    }
    // The server unit test controls completion order by holding job results. Holding a
    // status subprocess here would race its watchdog while the client and shell start.
    env.workspace("w", "layout { pane name=\"shell\" }\n");
    let out = env.cmd(&["open", "w"]).env("PATH", path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mut client = RawClient::attach_sized(&env, Some("w"), (80, 24)).traced();
    client.send(b"\x02Al");
    client.wait_for_row("Codex · Personal");
    env.run(&["send", "-p", "shell", "printf 'server-%s\\n' responsive\r"]);
    env.wait_for_text("shell", "server-responsive");
    client.wait_for_row("Claude · Personal");
    let checks = std::fs::read_to_string(env.root.join("home/status-checks")).unwrap();
    assert_eq!(checks.lines().count(), 2);
    for _ in 0..3 {
        client.close_overlay("Enter Details");
        client.send(b"\x02Al");
        client.wait_for_row("Claude · Personal");
        client.wait_for_row("Codex · Personal");
    }
    assert_eq!(std::fs::read_to_string(env.root.join("home/status-checks")).unwrap(), checks);
    assert!(!env.root.join("home/logins").exists());
}

#[test]
fn existing_sign_ins_are_verified_adopted_and_renamed_without_login_or_credential_copy() {
    for agent in ["claude", "codex"] {
        let env = Env::new(&format!("adopt-{agent}"));
        let path = existing_sign_in_agent(&env, agent);
        let home = env.root.join(format!("home/.{agent}"));
        let run = |args: &[&str]| {
            let out = env.cmd(args).env("PATH", &path).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8(out.stdout).unwrap()
        };
        let list = || serde_json::from_str::<serde_json::Value>(&run(&["accounts", "list", "--json"])).unwrap();
        assert_eq!(list(), serde_json::json!([]), "unsigned and absent agents add no accounts");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("credential"), "PRIVATE-CREDENTIAL").unwrap();
        for (status, count) in [("1", 0), ("2", 1), ("0", 1)] {
            std::fs::write(home.join("status"), status).unwrap();
            if agent == "claude" {
                std::fs::write(home.join("public-status"), r#"{"loggedIn":true,"email":"me@example.com"}"#).unwrap();
            }
            let listed = list();
            assert_eq!(listed.as_array().unwrap().len(), count);
            if status == "2" {
                assert_eq!(listed[0]["status"], "could_not_check");
                assert!(!env.root.join("config/kraal/accounts.toml").exists());
                assert!(run(&["accounts", "list"]).contains("check again"));
            }
            if status == "0" {
                assert_eq!(listed[0]["name"], if agent == "claude" { "me@example.com" } else { "Personal" });
                assert_eq!(listed[0]["status"], "signed_in");
            }
        }
        run(&["accounts", "rename", &format!("{agent}:default"), "Mine"]);
        assert_eq!(list()[0]["name"], "Mine");
        assert_eq!(list().as_array().unwrap().len(), 1);
        assert!(!env.root.join("home/logins").exists());
        assert!(!env.root.join("home/.local/share/kraal/accounts").exists());
        assert_eq!(std::fs::read_to_string(home.join("credential")).unwrap(), "PRIVATE-CREDENTIAL");
        run(&["accounts", "rm", &format!("{agent}:Mine")]);
        assert_eq!(list(), serde_json::json!([]), "forgotten standard homes are not rediscovered");
        assert!(home.join("credential").exists());
    }
}

#[test]
fn accounts_add_adopts_a_verified_home_and_aliases_do_not_add_an_account() {
    for explicit in [false, true] {
        let env = Env::new(if explicit { "adopt-home" } else { "adopt-add" });
        let path = existing_sign_in_agent(&env, "claude");
        let home = env.root.join("home/.claude");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("status"), "0").unwrap();
        std::fs::write(home.join("credential"), "kept").unwrap();
        let mut args = vec!["accounts", "add", "claude", "Mine"];
        if explicit {
            args.extend(["--home", home.to_str().unwrap()]);
        }
        let out = env.cmd(&args).env("PATH", &path).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert!(!String::from_utf8_lossy(&out.stdout).contains("logging in"));
        let alias = env.root.join("alias");
        std::os::unix::fs::symlink(&home, &alias).unwrap();
        let out = env
            .cmd(&["accounts", "add", "claude", "Another", "--home", alias.to_str().unwrap()])
            .env("PATH", &path)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let out = env.cmd(&["accounts", "list", "--json"]).env("PATH", &path).output().unwrap();
        let list: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(list.as_array().unwrap().len(), 1);
        assert_eq!(list[0]["name"], "Mine");
        assert!(!env.root.join("home/logins").exists());
        assert_eq!(std::fs::read_to_string(home.join("credential")).unwrap(), "kept");
        let out = env.cmd(&["accounts", "rename", "Another", "Personal"]).env("PATH", &path).output().unwrap();
        assert!(out.status.success(), "the old alias is still usable");
    }
}

#[test]
fn accounts_add_checks_the_home_chosen_by_review() {
    for signed_in in [false, true] {
        let env = Env::new(&format!("add-reviewed-{signed_in}"));
        let path = existing_sign_in_agent(&env, "claude");
        let standard = env.root.join("home/.claude");
        let work = env.root.join("work");
        for home in [&standard, &work] {
            std::fs::create_dir_all(home).unwrap();
        }
        std::fs::write(standard.join("status"), if signed_in { "1" } else { "0" }).unwrap();
        std::fs::write(work.join("status"), if signed_in { "0" } else { "1" }).unwrap();
        let file = env.root.join("config/kraal/accounts.toml");
        std::fs::write(&file, format!(
            "[[account]]\nid = 'default-claude'\nagent = 'claude'\nname = 'Personal'\nhome = '~/.claude'\nlinked_to = 'work-id'\n\n[[account]]\nid = 'work-id'\nagent = 'claude'\nname = 'work'\nhome = {:?}\nsign_in_required = true\n",
            work.to_str().unwrap()
        )).unwrap();
        let out = env
            .cmd(&["accounts", "add", "claude", "Another", "--home", "~/.claude"])
            .env("PATH", &path)
            .output()
            .unwrap();
        assert_eq!(out.status.success(), signed_in, "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(env.root.join("home/logins").exists(), !signed_in, "only a signed-out target needs login");
        assert_eq!(
            std::fs::read_to_string(env.root.join("home/status-checks")).unwrap().trim(),
            work.to_str().unwrap()
        );
        let saved: toml::Value = toml::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        let work = saved["account"].as_array().unwrap().iter().find(|a| a["id"].as_str() == Some("work-id")).unwrap();
        assert_eq!(work.get("sign_in_required").and_then(toml::Value::as_bool).unwrap_or(false), !signed_in);
    }
}

#[test]
fn accounts_add_revalidates_the_checked_identity_and_home_before_saving() {
    for change in ["identity", "home"] {
        let env = Env::new(&format!("add-checked-{change}"));
        let path = existing_sign_in_agent(&env, "claude");
        let file = env.root.join("config/kraal/accounts.toml");
        let source = "[[account]]\nid = 'default-claude'\nagent = 'claude'\nname = 'Personal'\nhome = '~/.claude'\nlinked_to = 'work-id'\n\n[[account]]\nid = 'work-id'\nagent = 'claude'\nname = 'work'\nhome = '~/work'\nsign_in_required = true\n";
        std::fs::write(&file, source).unwrap();
        let changed = if change == "identity" {
            source.replace("work-id", "replacement-id")
        } else {
            source.replace("~/work", "~/moved")
        };
        std::fs::write(env.root.join("home/replacement"), &changed).unwrap();
        write_script(
            &env.root.join("bin/claude"),
            r#"#!/bin/sh
[ "$1 $2" = 'auth status' ] || exit 2
cat "$HOME/replacement" > "$XDG_CONFIG_HOME/kraal/accounts.toml"
exit 0
"#,
        );
        let out = env
            .cmd(&["accounts", "add", "claude", "Another", "--home", "~/.claude"])
            .env("PATH", &path)
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("account changed while checking"));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), changed, "the stale check saves nothing");
    }
}

#[test]
fn accounts_refresh_discovers_the_standard_codex_sign_in_and_fetches_usage() {
    let env = Env::new("refresh-existing-codex");
    let path = existing_sign_in_agent(&env, "codex");
    let home = env.root.join("home/.codex");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("status"), "0").unwrap();
    write_script(
        &env.root.join("bin/codex"),
        r#"#!/bin/sh
home="${CODEX_HOME:-$HOME/.codex}"
case "$*" in
    'login status') exit "$(cat "$home/status")" ;;
    'app-server --listen stdio://') ;;
    *) exit 2 ;;
esac
echo "$home" > "$HOME/usage-home"
IFS= read -r line
printf '%s\n' '{"id":1,"result":{}}'
IFS= read -r line
IFS= read -r line
[ "$line" = '{"id":2,"method":"account/rateLimits/read","params":null}' ] || exit 3
printf '%s\n' '{"id":2,"result":{"rateLimits":{"primary":{"usedPercent":42,"windowDurationMins":300}}}}'
"#,
    );
    assert!(!env.root.join("config/kraal/accounts.toml").exists());
    let out = env.cmd(&["accounts", "refresh"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("42%"));
    assert_eq!(std::fs::read_to_string(env.root.join("home/usage-home")).unwrap().trim(), home.to_str().unwrap());
    let out = env.cmd(&["accounts", "list", "--json"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let list: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["name"], "Personal");
    assert_eq!(list[0]["usage"]["five_hour"]["used_percent"], 42.0);
}

#[test]
fn an_uncertain_existing_sign_in_never_starts_login_or_creates_an_account() {
    let env = Env::new("adopt-uncertain");
    let path = existing_sign_in_agent(&env, "claude");
    let home = env.root.join("home/.claude");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("status"), "2").unwrap();
    for explicit in [false, true] {
        let mut args = vec!["accounts", "add", "claude", "Mine"];
        if explicit {
            args.extend(["--home", home.to_str().unwrap()]);
        }
        let out = env.cmd(&args).env("PATH", &path).output().unwrap();
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("could not verify"));
        assert!(!env.root.join("config/kraal/accounts.toml").exists());
        assert!(!env.root.join("home/logins").exists());
    }
    std::fs::write(home.join("status"), "0").unwrap();
    let out = env.cmd(&["accounts", "add", "claude", "Mine"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!env.root.join("home/logins").exists());
}

#[test]
fn account_list_joins_only_one_exact_identity_and_reports_the_joined_home_and_usage() {
    for case in ["unique", "review", "two", "unreadable", "failed", "organization", "plan", "personal"] {
        let env = Env::new(&format!("identity-{case}"));
        let path = existing_sign_in_agent(&env, "claude");
        let standard = env.root.join("home/.claude");
        let mut config = String::new();
        let homes = if case == "personal" {
            vec![standard.clone()]
        } else {
            vec![standard.clone(), env.root.join("patron"), env.root.join("gmail")]
        };
        for (i, home) in homes.iter().enumerate() {
            std::fs::create_dir_all(home).unwrap();
            std::fs::write(home.join("status"), if i == 0 && case == "failed" { "2" } else { "0" }).unwrap();
            std::fs::write(home.join("credential"), "untouched").unwrap();
            if !(i == 0 && matches!(case, "unreadable" | "personal")) {
                let email = if i == 2 && case != "two" { "other@example.com" } else { "me@example.com" };
                let org = if i == 1 && case == "organization" { "org-2" } else { "org-1" };
                let plan = if i == 1 && case == "plan" { "pro" } else { "max" };
                std::fs::write(
                    home.join("public-status"),
                    serde_json::json!({
                        "loggedIn": true, "email": email, "orgId": org, "subscriptionType": plan,
                    })
                    .to_string(),
                )
                .unwrap();
            }
            if i > 0 {
                let name = if i == 1 { "patron.bzh" } else { "gmail" };
                config.push_str(&format!(
                    "[[account]]\nid = 'test-claude-{name}'\nagent = 'claude'\nname = {name:?}\nhome = {:?}\n",
                    home.display().to_string()
                ));
            }
        }
        let file = env.root.join("config/kraal/accounts.toml");
        std::fs::write(&file, config).unwrap();
        let usage = env.root.join("state/kraal/usage/by-id");
        std::fs::create_dir_all(&usage).unwrap();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        std::fs::write(usage.join("claude-default-claude.json"), serde_json::json!({
            "five_hour": { "used_percent": 57, "resets_at": now + 3600 }, "observed_at": now, "source": "statusline",
        }).to_string()).unwrap();
        if case == "review" {
            let out = env.cmd(&["accounts", "review", "claude"]).env("PATH", &path).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            assert!(String::from_utf8_lossy(&out.stderr).contains("~/.claude is Claude · patron.bzh"));
        }
        let out = env.cmd(&["accounts", "list", "--json"]).env("PATH", &path).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let list: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let list = list.as_array().unwrap();
        if case == "unique" || case == "review" {
            assert_eq!(list.len(), 2);
            let account = list.iter().find(|a| a["name"] == "patron.bzh").unwrap();
            assert_eq!(account["usage"]["five_hour"]["used_percent"], 57.0);
            assert_eq!(account["joined_homes"], serde_json::json!(["~/.claude"]));
            assert_eq!(
                String::from_utf8_lossy(&out.stderr).matches("~/.claude is Claude · patron.bzh").count(),
                usize::from(case == "unique")
            );
            let saved = std::fs::read_to_string(&file).unwrap();
            let again = env.cmd(&["accounts", "list"]).env("PATH", &path).output().unwrap();
            assert!(again.status.success());
            assert!(!String::from_utf8_lossy(&again.stderr).contains("is Claude"));
            assert!(String::from_utf8_lossy(&again.stdout).contains("~/.claude (joined sign-in)"));
            assert_eq!(std::fs::read_to_string(&file).unwrap(), saved);
        } else if case == "personal" {
            assert_eq!(list.len(), 1);
            assert_eq!(list[0]["name"], "Personal");
        } else {
            assert_eq!(list.iter().filter(|a| a["status"] == "review_required").count(), 1, "{case}: {list:?}");
            assert!(String::from_utf8_lossy(&out.stderr).is_empty());
            let question = list.iter().find(|a| a["status"] == "review_required").unwrap();
            assert_eq!(question["name"], "? Which account is ~/.claude");
        }
        for home in homes {
            assert_eq!(std::fs::read_to_string(home.join("credential")).unwrap(), "untouched");
        }
        assert!(!env.root.join("home/logins").exists());
    }
}

#[test]
fn automatic_join_rejects_pending_and_completed_cli_logins() {
    for pending in [true, false] {
        let env = Env::new(if pending { "join-login-pending" } else { "join-login-completed" });
        let path = existing_sign_in_agent(&env, "claude");
        let standard = env.root.join("home/.claude");
        let named = env.root.join("work");
        for home in [&standard, &named] {
            std::fs::create_dir_all(home).unwrap();
            std::fs::write(home.join("identity"), r#"{"loggedIn":true,"email":"a@example.com"}"#).unwrap();
        }
        let file = env.root.join("config/kraal/accounts.toml");
        std::fs::write(
            &file,
            format!(
                "[[account]]\nid = 'test-claude-work'\nagent = 'claude'\nname = 'work'\nhome = {:?}\n",
                named.display().to_string()
            ),
        )
        .unwrap();
        let assert_not_joined = || {
            let saved: toml::Value = toml::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
            let work =
                saved["account"].as_array().unwrap().iter().find(|a| a["name"].as_str() == Some("work")).unwrap();
            assert_eq!(work["home"].as_str(), named.to_str());
            assert!(work.get("linked_to").is_none());
            // Adopting the standard home keeps its old name as a reference under its own review.
            assert!(work.get("references").is_none(), "the standard home must not join the named account");
        };
        write_script(
            &env.root.join("bin/claude"),
            r#"#!/bin/sh
home="${CLAUDE_CONFIG_DIR:-$HOME/.claude}"
case "$*" in
    'auth status')
        cat "$home/identity"
        echo checked > "$home/checked"
        while test -f "$home/hold-check"; do sleep 0.01; done
        ;;
    'auth login')
        echo started > "$home/login-started"
        while test -d "$home" && test ! -f "$home/finish-login"; do sleep 0.01; done
        test -d "$home" || exit 1
        printf '%s' '{"loggedIn":true,"email":"b@example.com"}' > "$home/identity"
        ;;
    *) exit 2 ;;
esac
"#,
        );
        let start_login = || {
            env.cmd(&["accounts", "login", "claude:work"])
                .env("PATH", &path)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        };
        let start_discovery = || {
            env.cmd(&["accounts", "list", "--json"])
                .env("PATH", &path)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        };
        let (login, discovery) = if pending {
            let mut login = start_login();
            wait_until(PATIENCE, "CLI login started", || {
                assert!(login.try_wait().unwrap().is_none(), "CLI login exited before authentication started");
                named.join("login-started").exists()
            });
            assert!(std::fs::read_to_string(&file).unwrap().contains("sign_in_required = true"));
            (login, start_discovery())
        } else {
            // Keep A's standard-home result pending while the named home completes a login to B.
            std::fs::write(standard.join("hold-check"), "").unwrap();
            let discovery = start_discovery();
            wait_until(PATIENCE, "both identities were inspected", || {
                standard.join("checked").exists() && named.join("checked").exists()
            });
            let mut login = start_login();
            wait_until(PATIENCE, "CLI login started", || {
                assert!(login.try_wait().unwrap().is_none(), "CLI login exited before authentication started");
                named.join("login-started").exists()
            });
            std::fs::write(named.join("finish-login"), "").unwrap();
            (login, discovery)
        };
        if pending {
            let out = discovery.wait_with_output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            assert_not_joined();
            assert!(!String::from_utf8_lossy(&out.stderr).contains("is Claude"));
            std::fs::write(named.join("finish-login"), "").unwrap();
            let out = login.wait_with_output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        } else {
            let out = login.wait_with_output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            assert!(!std::fs::read_to_string(&file).unwrap().contains("sign_in_required = true"));
            std::fs::remove_file(standard.join("hold-check")).unwrap();
            let out = discovery.wait_with_output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            assert_not_joined();
            assert!(!String::from_utf8_lossy(&out.stderr).contains("is Claude"));
        }
        let out = env.cmd(&["accounts", "list", "--json"]).env("PATH", &path).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let list: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let list = list.as_array().unwrap();
        assert_eq!(list.len(), 2, "A's standard home stays separate from B's named account");
        assert_eq!(list.iter().filter(|a| a["status"] == "review_required").count(), 1);
        let review = list.iter().find(|a| a["status"] == "review_required").unwrap();
        assert_eq!(review["home"], "~/.claude");
        let work = list.iter().find(|a| a["name"] == "work").unwrap();
        assert_eq!(work["joined_homes"], serde_json::json!([]));
        assert_not_joined();
    }
}

#[test]
fn automatic_join_preserves_a_live_agent_and_caches_the_named_home_checks() {
    let env = Env::new("auto-join-live");
    let path = existing_sign_in_agent(&env, "claude");
    let standard = env.root.join("home/.claude");
    let named = env.root.join("patron");
    for home in [&standard, &named] {
        std::fs::create_dir_all(home).unwrap();
        std::fs::write(home.join("status"), if home == &standard { "1" } else { "0" }).unwrap();
        std::fs::write(
            home.join("public-status"),
            r#"{"loggedIn":true,"email":"me@example.com","orgId":"org-1","subscriptionType":"max"}"#,
        )
        .unwrap();
    }
    std::fs::write(
        env.root.join("config/kraal/accounts.toml"),
        format!(
            "[[account]]\nid = 'test-claude-patron.bzh'\nagent = 'claude'\nname = 'patron.bzh'\nhome = {:?}\n",
            named.display().to_string()
        ),
    )
    .unwrap();
    env.workspace("w", "layout { pane name=\"agent\" agent=\"claude:default\" }\n");
    let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    env.wait_for_text("agent", &format!("READY home={}", standard.display()));
    let starts = std::fs::read_to_string(standard.join("starts")).unwrap();
    let mut client = RawClient::attach_sized(&env, Some("w"), (124, 36)).traced();
    client.send(b"\x02Al");
    client.wait_for_rows("unsigned standard home check finished", |rows| {
        rows.iter().any(|r| r.contains("Claude · patron.bzh")) && !rows.iter().any(|r| r.contains("Checking"))
    });
    client.close_overlay("Enter Details");
    std::fs::write(standard.join("status"), "0").unwrap();
    client.send(b"\x02Al");
    client.wait_for_rows("the live pane has joined patron.bzh", |rows| {
        rows.iter().any(|r| r.contains("claude·patron.bzh"))
            && rows.windows(2).any(|pair| pair[0].contains("Claude · patron.bzh") && pair[1].contains("1 pane"))
    });
    assert!(!client.rows().iter().any(|r| r.contains("Which account is this?") || r.contains("? Account for")));
    for _ in 0..2 {
        client.close_overlay("Enter Details");
        client.send(b"\x02Al");
        client.wait_for_row("Claude · patron.bzh");
    }
    // The toast can expire or be replaced before the next frame. Messages retains the
    // notice independently of background check order.
    client.close_overlay("Enter Details");
    client.send(b"\x02Vj");
    client.wait_for_row("Messages");
    client.wait_for_row("~/.claude is Claude · patron.bzh");
    let checks = std::fs::read_to_string(env.root.join("home/status-checks")).unwrap();
    assert_eq!(checks.lines().filter(|line| *line == named.to_str().unwrap()).count(), 1);
    env.run(&["send", "-p", "agent", "alive\r"]);
    env.wait_for_text("agent", "reply:alive");
    assert_eq!(std::fs::read_to_string(standard.join("starts")).unwrap(), starts);
    assert!(!env.root.join("home/logins").exists());
}

#[test]
fn command_line_review_can_keep_a_different_account_without_login() {
    let env = Env::new("review-cli");
    let path = existing_sign_in_agent(&env, "claude");
    let home = env.root.join("home/.claude");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("status"), "0").unwrap();
    let file = env.root.join("config/kraal/accounts.toml");
    std::fs::write(
        &file,
        format!(
            "[[account]]\nid = 'test-claude-work'\nagent = 'claude'\nname = 'work'\nhome = {:?}\n",
            env.root.join("work").display().to_string()
        ),
    )
    .unwrap();
    let out = env.cmd(&["accounts", "list"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("? Which account is ~/.claude"));
    let before = std::fs::read_to_string(&file).unwrap();
    let out = env.cmd(&["accounts", "review", "claude"]).env("PATH", &path).output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("in a terminal"));
    assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
    env.workspace("w", "layout { pane name=\"shell\" }\n");
    let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success());
    let pane = env.run(&["run", "--tab", "--", env!("CARGO_BIN_EXE_kraal"), "accounts", "review", "claude"]);
    let pane = pane.trim();
    env.wait_for_text(pane, "Which account is this?");
    env.wait_for_text(pane, "2. A different account");
    env.run(&["send", "-p", pane, "2\r"]);
    env.wait_for_text(pane, "Use Personal?");
    env.run(&["send", "-p", pane, "y\r"]);
    env.wait_for_text(pane, "Account updated");
    let out = env.cmd(&["accounts", "list", "--json"]).env("PATH", &path).output().unwrap();
    let list: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 2);
    assert!(list.as_array().unwrap().iter().any(|a| a["name"] == "Personal" && a["status"] == "signed_in"));
    assert!(!std::fs::read_to_string(&file).unwrap().contains("review = true"));
    assert!(!env.root.join("home/logins").exists());
}

#[test]
fn reviewing_the_existing_account_keeps_live_agents_and_reports_usage_once() {
    for size in [(124, 36), (80, 24)] {
        let env = Env::new(&format!("account-review-{}", size.0));
        let path = existing_sign_in_agent(&env, "claude");
        existing_sign_in_agent(&env, "codex");
        let codex = env.root.join("home/.codex");
        std::fs::create_dir_all(&codex).unwrap();
        std::fs::write(codex.join("status"), "0").unwrap();
        let standard = env.root.join("home/.claude");
        let mut config = String::from("# My subscriptions.\n");
        for name in ["patron.bzh", "gmail"] {
            let home = env.root.join(name);
            std::fs::create_dir_all(&home).unwrap();
            std::fs::write(home.join("status"), "0").unwrap();
            std::fs::write(home.join("credential"), name).unwrap();
            config.push_str(&format!(
                "[[account]]\nid = 'test-claude-{name}'\nagent = 'claude'\nname = '{name}'\nhome = {:?}\n",
                home.to_str().unwrap()
            ));
        }
        config.push_str("[[rule]]\nworkspace = 'legacy'\nagent = 'claude'\naccounts = ['default']\n");
        let file = env.root.join("config/kraal/accounts.toml");
        std::fs::write(&file, &config).unwrap();
        std::fs::create_dir_all(&standard).unwrap();
        std::fs::write(standard.join("status"), "0").unwrap();
        std::fs::write(standard.join("credential"), "standard").unwrap();
        let usage = env.root.join("state/kraal/usage/by-id");
        std::fs::create_dir_all(&usage).unwrap();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        for (id, percent, at) in [("default-claude", 57, now), ("test-claude-patron.bzh", 40, now - 1)] {
            std::fs::write(
                usage.join(format!("claude-{id}.json")),
                serde_json::json!({
                    "five_hour": { "used_percent": percent, "resets_at": now + 3600 },
                    "weekly": null, "observed_at": at, "source": "statusline"
                })
                .to_string(),
            )
            .unwrap();
        }
        env.workspace("w", "layout { pane name=\"agent\" agent=\"claude:default\" }\n");
        let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let pane = panes.as_array().unwrap().iter().find(|p| p["name"] == "agent").unwrap()["id"].to_string();
        env.wait_for_text(&pane, "READY");
        let mut client = RawClient::attach_sized(&env, Some("w"), size).traced();
        if size.0 == 124 {
            client.wait_for_row("Codex · Personal");
            // The question is a single sidebar action, below the named accounts.
            let rows = wait_for_account_review(&client, "Claude");
            let row = rows.iter().position(|r| r.contains("? Account for ~/.claude")).unwrap() + 1;
            client.send(format!("\x1b[<0;10;{row}M\x1b[<0;10;{row}m").as_bytes());
        } else {
            client.send(b"\x02Al");
            client.wait_for_row("Codex · Personal");
            wait_for_account_review(&client, "Claude");
            assert!(!client.rows().iter().any(|r| r.contains("claude·default") || r.contains("codex·default")));
            client.send(b"Which account is");
            client.send(b"\r");
        }
        client.wait_for_row("Which account is this?");
        client.wait_for_row("A different account");
        // Adoption already links the legacy default; cancelling the review must leave the file unchanged.
        let before_review = std::fs::read_to_string(&file).unwrap();
        client.send(b"\r");
        client.wait_for_row("Use patron.bzh?");
        client.wait_for_row("Your running agents will keep working.");
        client.send(b"\r");
        client.wait_for_row("Which account is this?");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), before_review, "Enter cancels first");
        client.send(b"\r");
        client.wait_for_row("Use patron.bzh?");
        client.send(b"y");
        client.wait_for_row("Account updated");
        if size.0 == 80 {
            client.send(b"\x1b");
            client.close_overlay("Search  Which account is");
        } else {
            client.close_overlay("Enter Details");
        }
        env.run(&["send", "-p", &pane, "still-here\r"]);
        env.wait_for_text(&pane, "reply:still-here");
        assert_eq!(std::fs::read_to_string(standard.join("starts")).unwrap().lines().count(), 1);
        assert!(!env.root.join("patron.bzh/starts").exists());
        let out = env.cmd(&["accounts", "list", "--json"]).env("PATH", &path).output().unwrap();
        let list: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(list.as_array().unwrap().len(), 3);
        let saved = std::fs::read_to_string(&file).unwrap();
        let saved: toml::Value = toml::from_str(&saved).unwrap();
        assert_eq!(saved["account"].as_array().unwrap().len(), 3);
        assert!(saved["account"].as_array().unwrap().iter().all(|a| a.get("linked_to").is_none()));
        let patron = list.as_array().unwrap().iter().find(|a| a["name"] == "patron.bzh").unwrap();
        assert_eq!(patron["usage"]["five_hour"]["used_percent"], 57.0);
        assert!(list.as_array().unwrap().iter().all(|a| a["status"] != "review_required"));
        for (home, credential) in
            [(&standard, "standard"), (&env.root.join("patron.bzh"), "patron.bzh"), (&env.root.join("gmail"), "gmail")]
        {
            assert_eq!(std::fs::read_to_string(home.join("credential")).unwrap(), credential);
        }
        assert!(!env.root.join("home/logins").exists());
        let socket = env.root.join(format!("run/kraal/{}.sock", env.session));
        let out = env
            .cmd(&["agent", "claude", "--account", "default", "--tab"])
            .env("PATH", &path)
            .env("KRAAL_SOCKET", socket)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let new_pane = String::from_utf8(out.stdout).unwrap().trim().to_string();
        env.wait_for_text(&new_pane, &format!("home={}", env.root.join("patron.bzh").display()));
        client.send(b"\x02a");
        client.wait_for_row("Start an agent");
        assert!(!client.rows().iter().any(|r| r.contains("Existing Claude account")));
    }
}

#[test]
fn a_registered_native_agent_keeps_its_environment_and_rejects_account_overrides() {
    let env = Env::new("native-agent-cli");
    let bin = env.root.join("bin");
    std::fs::create_dir(&bin).unwrap();
    write_script(
        &bin.join("gemini"),
        "#!/bin/sh\nprintf 'native=%s account=%s arg=%s\\n' \"$OPENAI_API_KEY\" \"${KRAAL_ACCOUNT-unset}\" \"$1\"\n",
    );
    let output = env
        .cmd(&["agent", "gemini", "--here", "--", "hello"])
        .env("PATH", &bin)
        .env("OPENAI_API_KEY", "stand-in-key")
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "native=stand-in-key account=unset arg=hello\n");
    let output = env.cmd(&["agent", "gemini", "--here", "--account", "work"]).env("PATH", &bin).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Accounts are managed in Gemini"));
    let output = env.cmd(&["accounts", "add", "gemini", "work"]).env("PATH", &bin).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Accounts are managed in Gemini"));
    let output = env.cmd(&["agent", "aider", "--here"]).env("PATH", &bin).output().unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Aider is not installed. Install Aider"));
    assert!(!env.root.join("config/kraal/accounts.toml").exists());
}

#[test]
fn the_launch_picker_starts_native_agents_in_panes_tabs_and_worktrees() {
    for (action, name) in [(b"\r".as_slice(), "pane"), (b"\x14", "tab"), (b"\x17", "worktree")] {
        let env = Env::new(&format!("native-agent-{name}"));
        let bin = env.root.join("bin");
        std::fs::create_dir(&bin).unwrap();
        write_script(
            &bin.join("gemini"),
            "#!/bin/sh\nprintf 'NATIVE_READY account=%s\\n' \"${KRAAL_ACCOUNT-unset}\"\nwhile IFS= read -r line; do printf 'reply:%s\\n' \"$line\"; done\n",
        );
        let repo = env.root.join("repo");
        let _git = git_repo(&repo, &[("file", "content\n")]);
        env.workspace("w", &format!("layout {{ pane name=\"shell\" cwd={:?} }}\n", repo.display().to_string()));
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        let output = env.cmd(&["open", "w"]).env("PATH", path).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let mut client = RawClient::attach_sized(&env, Some("w"), (80, 24)).traced();
        client.send(b"\x02a");
        client.wait_for_row("Start an agent");
        client.send(b"Gemini");
        client.wait_for_row("Accounts are managed in Gemini");
        client.send(b"\x04");
        client.wait_for_row("Accounts are managed in Gemini");
        if name == "worktree" {
            // Git discovery controls whether the worktree action is available.
            client.close_overlay("Start an agent");
            client.wait_for_row("main");
            client.send(b"\x02aGemini");
            client.wait_for_row("Accounts are managed in Gemini");
        }
        client.send(action);
        if name == "worktree" {
            client.wait_for_row("Name of the worktree for gemini");
            client.send(b"native\r");
        }
        client.wait_for_row("NATIVE_READY account=unset");
        wait_until(PATIENCE, "the native agent is detected", || {
            let agents: serde_json::Value = serde_json::from_str(&env.run(&["agents", "--json"])).unwrap();
            agents.as_array().unwrap().iter().any(|agent| agent["agent"] == "gemini")
        });
        client.send(b"\x02Ac");
        client.wait_for_row("Accounts are managed in Gemini");
        client.send(b"still-running\r");
        client.wait_for_row("reply:still-running");
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        assert!(panes.as_array().unwrap().iter().all(|p| p["account"].is_null()));
        if name == "worktree" {
            assert!(env.root.join("repo.worktrees/native").is_dir());
        }
    }
}

#[test]
fn a_missing_agent_keeps_the_add_form_and_creates_nothing() {
    let env = Env::new("login-missing");
    env.workspace("w", "layout { pane name=\"shell\" }\n");
    env.run(&["open", "w"]);
    let mut client = RawClient::attach_sized(&env, Some("w"), (80, 24)).traced();
    client.send(b"\x02An");
    client.wait_for_row("Not installed");
    client.send(b"\x15unfinished\r");
    client.wait_for_row("Install Claude");
    client.wait_for_row("Name    unfinished");
    assert!(login_panes(&env).is_empty());
    let file = env.root.join("config/kraal/accounts.toml");
    assert!(!std::fs::read_to_string(file).unwrap_or_default().contains("unfinished"));
    client.close_overlay("Name    unfinished");
    assert!(!client.rows().iter().any(|r| r.contains("Enter Add account") || r.contains("Enter Details")));
    client.send(b"\x02An\x1b[Z\x1b[C");
    client.wait_for_row("Agent   Codex");
    client.close_overlay("Agent   Codex");
    assert!(!client.rows().iter().any(|r| r.contains("Enter Add account") || r.contains("Enter Details")));
    client.send(b"\x02An");
    client.wait_for_row("Agent   Codex");
    client.close_overlay("Agent   Codex");
    assert!(!client.rows().iter().any(|r| r.contains("Enter Add account") || r.contains("Enter Details")));
    client.send(b"\x02a");
    let rows = client.wait_for_row("No accounts ready.");
    assert!(!rows.iter().any(|r| r.contains("Add account")));
}

#[test]
fn command_line_login_verifies_and_reports_the_same_statuses() {
    let env = Env::new("login-cli");
    let (home, path) = login_agent(&env, "claude");
    std::fs::write(home.join("finish-login"), "0").unwrap();
    let file = env.root.join("config/kraal/accounts.toml");
    for (code, status, label, logged_in) in [
        ("1", "not_signed_in", "sign-in required", serde_json::json!(false)),
        ("2", "could_not_check", "could not check sign-in", serde_json::Value::Null),
        ("0", "signed_in", "is signed in", serde_json::json!(true)),
    ] {
        std::fs::write(home.join("status"), code).unwrap();
        let out = env.cmd(&["accounts", "login", "claude:work"]).env("PATH", &path).output().unwrap();
        assert_eq!(out.status.success(), code == "0");
        let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(text.contains(label), "{text}");
        assert_eq!(std::fs::read_to_string(&file).unwrap().contains("sign_in_required = true"), code != "0");
        let out = env.cmd(&["accounts", "list", "--json"]).env("PATH", &path).output().unwrap();
        assert!(out.status.success());
        assert!(!String::from_utf8_lossy(&out.stderr).contains("PRIVATE-LOGIN-OUTPUT"));
        let list: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        let account = list.as_array().unwrap().iter().find(|a| a["name"] == "work").unwrap();
        assert_eq!(account["status"], status);
        assert_eq!(account["logged_in"], logged_in);
    }
}

/// Shell definitions need no account bindings, so account changes do not need write access
/// to their directory (nor leave lock files beside them).
#[test]
fn account_updates_allow_read_only_shell_definitions() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new("readonly-layouts");
    env.workspace("shells", "layout { pane name=\"shell\" }\n");
    let dir = env.root.join("config/kraal/workspaces");
    let file = dir.join("shells.kdl");
    let original = std::fs::read(&file).unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o400)).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    let added = env.try_run(&["accounts", "add", "codex", "work", "--no-login"]);
    let limits = env.try_run(&["accounts", "limits", "--threshold", "75"]);
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(added.status.success(), "{}", String::from_utf8_lossy(&added.stderr));
    assert!(limits.status.success(), "{}", String::from_utf8_lossy(&limits.stderr));
    assert_eq!(std::fs::read(&file).unwrap(), original);
    assert_eq!(std::fs::read_dir(dir).unwrap().count(), 1, "no lock file was needed");
    let accounts: toml::Value =
        toml::from_str(&std::fs::read_to_string(env.root.join("config/kraal/accounts.toml")).unwrap()).unwrap();
    assert_eq!(accounts["account"][0]["name"].as_str(), Some("work"));
    assert_eq!(accounts["limits"]["threshold"].as_integer(), Some(75));
}

/// An account renamed from the agent picker, then from the command line: its rule and its usage
/// follow, and the agent running on it goes on under the new name.
#[test]
fn a_renamed_account_keeps_its_rules_its_usage_and_its_agents() {
    let env = Env::new("renameacct");
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(
        &bin.join("claude"),
        "#!/bin/sh\n[ \"$2\" = status ] && exit 1\necho \"home=${CLAUDE_CONFIG_DIR:-default}\"\nexec sleep 100\n",
    );
    let home = env.root.join("alpha-home");
    std::fs::create_dir_all(&home).unwrap();
    let file = env.root.join("config/kraal/accounts.toml");
    std::fs::write(
        &file,
        format!(
            "[[account]]\nid = 'test-claude-alpha'\nname = \"alpha\"\nagent = \"claude\"\nhome = {:?}\n\n[[rule]]\nworkspace = \"w\"\naccounts = [\"alpha\"]\n",
            home.display().to_string()
        ),
    )
    .unwrap();
    let usage = env.root.join("state/kraal/usage/by-id");
    std::fs::create_dir_all(&usage).unwrap();
    std::fs::write(usage.join("claude-test-claude-alpha.json"), r#"{"observed_at":0,"source":"statusline"}"#).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let socket = env.root.join("run/kraal/renameacct.sock");
    let out = env
        .cmd(&["agent", "claude", "--account", "alpha", "--tab"])
        .env("PATH", &path)
        .env("KRAAL_SOCKET", &socket)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let agent = panes.as_array().unwrap().iter().find(|p| p["command"][0] == "claude").unwrap()["id"].to_string();
    env.wait_for_text(&agent, &format!("home={}", home.display()));
    let (mut events, seen) = env.follow(&["events", "--since", "0", "agent.*"]);
    let hook = |payload: &str| {
        let mut c = env.cmd(&["hook", "claude"]);
        c.env("KRAAL_PANE", &agent).env("KRAAL_SOCKET", &socket).stdin(std::process::Stdio::piped());
        let mut child = c.spawn().unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
        assert!(child.wait().unwrap().success());
    };
    // The account the next event of type `ty` names.
    let account_said = |ty: &str| -> String {
        loop {
            let line = seen.recv_timeout(Duration::from_secs(10)).expect("an agent event");
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            if v["type"] == ty {
                return v["account"].as_str().unwrap().to_string();
            }
        }
    };
    hook(r#"{"hook_event_name":"UserPromptSubmit","session_id":"s-1"}"#);
    assert_eq!(account_said("agent.working"), "claude:alpha");
    let mut client = RawClient::attach(&env, "w").traced();

    // Tab on the account in Accounts, then Rename and the new name.
    // The client is sent what changed since the frame before: a word is looked for only where
    // it is drawn over blank cells or other letters.
    client.send(b"\x02Al");
    client.wait_for_row("Accounts");
    client.send(b"alpha");
    client.wait_for_rows("alpha selected beside Add account", |rows| {
        rows.iter().any(|r| r.contains("Search  alpha"))
            && rows.iter().any(|r| r.contains("▸ Claude · alpha"))
            && rows.iter().any(|r| r.contains("+ Add account"))
            && rows.iter().any(|r| r.contains("Enter Details") && r.contains("1/1"))
    });
    client.send(b"\t");
    client.wait_for_row("Rename");
    client.send(b"\r\x15gamma\r");
    let named = |name: &str| std::fs::read_to_string(&file).unwrap().contains(&format!("name = \"{name}\""));
    wait_until(Duration::from_secs(10), "renamed from the picker", || named("gamma"));
    let text = std::fs::read_to_string(&file).unwrap();
    let parsed: toml::Value = toml::from_str(&text).unwrap();
    let id = parsed["account"][0]["id"].as_str().unwrap();
    assert_eq!(id, "test-claude-alpha", "the account keeps its identity");
    assert_eq!(parsed["account"][0]["name"].as_str(), Some("gamma"));
    assert_eq!(parsed["rule"][0]["accounts"], toml::Value::try_from(["gamma"]).unwrap());
    assert_eq!(parsed["rule"][0]["account_ids"], toml::Value::try_from([format!("claude:@{id}")]).unwrap());
    assert!(usage.join(format!("claude-{id}.json")).exists());
    hook(r#"{"hook_event_name":"Stop","session_id":"s-1"}"#);
    assert_eq!(account_said("agent.done"), "claude:gamma");

    // From the command line: the session reads accounts.toml again when it changes.
    assert_eq!(env.run(&["accounts", "rename", "claude:gamma", "zulu"]).trim(), "renamed claude·gamma to claude·zulu");
    client.wait_for_row("zulu");
    hook(r#"{"hook_event_name":"UserPromptSubmit","session_id":"s-1"}"#);
    assert_eq!(account_said("agent.working"), "claude:zulu");
    assert!(std::fs::read_to_string(&file).unwrap().contains(&format!("account_ids = [\"claude:@{id}\"]")));
    assert!(usage.join(format!("claude-{id}.json")).exists());
    let out = env.try_run(&["accounts", "rename", "claude:default", "mine"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("check the existing sign-in") && !out.status.success());
    let screen = env.run(&["read", "-p", &agent]);
    assert!(screen.contains(&format!("home={}", home.display())), "the agent went on: {screen}");
    let _ = events.kill();
    let _ = events.wait();
}

#[test]
fn reused_account_names_leave_running_and_saved_references_on_the_original_account() {
    let env = Env::new("account-identity");
    let pane = account_switch_agent(&env, "claude", "notify");
    let path = format!("{}:{}", env.root.join("bin").display(), std::env::var("PATH").unwrap_or_default());
    let panes = || -> serde_json::Value { serde_json::from_str(&env.run(&["panes", "--json"])).unwrap() };
    let old =
        panes().as_array().unwrap().iter().find(|p| p["id"].as_u64() == pane.parse::<u64>().ok()).unwrap().clone();
    let id = old["account_id"].as_str().unwrap();
    env.workspace("closed", "// Kept closed.\nlayout { pane agent=\"claude:alpha\" }\n");
    env.workspace("saved", &format!("layout {{ pane agent=\"claude:alpha\" account_id={id:?}; }}\n"));
    env.run(&["accounts", "rename", "claude:alpha", "work"]);
    wait_until(PATIENCE, "pane follows its renamed label", || {
        panes().as_array().unwrap().iter().any(|p| p["account"] == "claude:work" && p["account_id"] == id)
    });
    let definition = std::fs::read_to_string(env.root.join("config/kraal/workspaces/closed.kdl")).unwrap();
    assert_eq!(definition, "// Kept closed.\nlayout { pane agent=\"claude:alpha\" }\n");
    let out = env.try_run(&["open", "closed"]);
    let error = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && error.contains("claude has no account named \"alpha\""), "{error}");
    env.run(&["kill"]);

    let saved = env.state_file();
    let source = std::fs::read(&saved).unwrap();
    env.run(&["accounts", "rm", "claude:work"]);
    assert_eq!(std::fs::read(&saved).unwrap(), source);
    let replacement = env.root.join("replacement").display().to_string();
    env.run(&["accounts", "add", "claude", "alpha", "--home", &replacement, "--no-login"]);
    let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(panes().as_array().unwrap().iter().any(|p| p["account"] == "claude:work" && p["account_id"] == id));
    let out = env.try_run(&["open", "saved"]);
    let error = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && error.contains("account was removed") && !error.contains(id), "{error}");
    let refused = raw_request(
        &env,
        serde_json::json!({ "cmd": "open_agent", "agent": "claude",
        "account": "alpha", "account_id": id, "placement": "tab", "near": null, "cwd": null }),
    );
    assert!(refused["Err"].as_str().unwrap().contains("account was removed"));
    // An explicit account name selects its current identity.
    let opened = raw_request(
        &env,
        serde_json::json!({ "cmd": "open_agent", "agent": "claude",
        "account": "alpha", "placement": "tab", "near": null, "cwd": null }),
    );
    assert!(opened.get("Ok").is_some(), "{opened}");
    assert!(panes().as_array().unwrap().iter().any(|p| p["account"] == "claude:alpha" && p["account_id"] != id));
    // The untouched external definition resolves the reused name too.
    env.run(&["open", "closed"]);
    assert!(panes().as_array().unwrap().iter().any(|p| {
        p["workspace"] == "closed"
            && p["account"] == "claude:alpha"
            && p["account_id"].as_str().is_some_and(|new| new != id)
    }));
    let selected = raw_request(
        &env,
        serde_json::json!({ "cmd": "open_agent", "agent": "claude",
        "account": "alpha", "account_id": "test-claude-beta", "placement": "tab", "near": null, "cwd": null }),
    );
    assert!(selected.get("Ok").is_some(), "{selected}");
    assert!(
        panes()
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["account"] == "claude:beta" && p["account_id"] == "test-claude-beta")
    );
    let live = panes().as_array().unwrap().iter().find(|p| p["account"] == "claude:alpha").unwrap().clone();
    let live_id = live["account_id"].as_str().unwrap();
    env.run(&["accounts", "rm", "claude:alpha"]);
    env.run(&["accounts", "add", "claude", "alpha", "--home", &replacement, "--no-login"]);
    let file: toml::Value =
        toml::from_str(&std::fs::read_to_string(env.root.join("config/kraal/accounts.toml")).unwrap()).unwrap();
    let replacement_id =
        file["account"].as_array().unwrap().iter().find(|a| a["name"].as_str() == Some("alpha")).unwrap()["id"]
            .as_str()
            .unwrap();
    assert_ne!(live_id, replacement_id);
    env.run(&["config", "reload"]);
    assert!(panes().as_array().unwrap().iter().any(|p| p["id"] == live["id"] && p["account_id"] == live_id));
    let mut hook = env
        .cmd(&["statusline"])
        .env("KRAAL_ACCOUNT_ID", live_id)
        .env("CLAUDE_CONFIG_DIR", &replacement)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    use std::io::Write;
    hook.stdin.take().unwrap().write_all(br#"{"rate_limits":{"five_hour":{"used_percentage":91}}}"#).unwrap();
    assert!(hook.wait().unwrap().success());
    assert!(
        !env.root.join(format!("state/kraal/usage/by-id/claude-{replacement_id}.json")).exists(),
        "a late statusline must not write usage to the replacement, even in the same home"
    );
}

#[test]
fn accounts_add_form_finishes_in_the_list_and_actions_keep_live_conversations() {
    for size in [(80, 24), (124, 36)] {
        let env = Env::new(&format!("accounts-form-{}", size.0));
        let bin = env.root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        for agent in ["claude", "codex"] {
            write_script(
                &bin.join(agent),
                r#"#!/bin/sh
home="${CLAUDE_CONFIG_DIR:-${CODEX_HOME:-}}"
case "$*" in
    'auth status'|'login status') test -f "$home/signed-in"; exit $? ;;
    'auth login'|'login') echo login >> "$home/logins"; touch "$home/signed-in"; exit 0 ;;
    app-server) exit 1 ;;
esac
echo "READY account=$KRAAL_ACCOUNT"
while IFS= read -r line; do printf 'reply:%s\n' "$line"; done
"#,
            );
        }
        let home = env.root.join("work-home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("signed-in"), "credential").unwrap();
        std::fs::write(home.join("conversation"), "history").unwrap();
        let file = env.root.join("config/kraal/accounts.toml");
        std::fs::write(
            &file,
            format!(
                "[[account]]\nid = 'test-codex-work'\nname='work'\nagent='codex'\nhome={:?}\n",
                home.to_str().unwrap()
            ),
        )
        .unwrap();
        env.workspace("w", "layout { pane name=\"agent\" agent=\"codex:work\" }\n");
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        env.wait_for_text("agent", "READY account=codex:work");
        let mut client = RawClient::attach_sized(&env, Some("w"), size).traced();
        if size.0 == 124 {
            // Discovery removes the unsigned standard accounts and moves the heading.
            client.wait_for_rows("startup account discovery completed", |rows| {
                rows.iter().any(|r| r.contains("ACCOUNTS"))
                    && !rows.iter().any(|r| r.contains("Existing ") || r.contains("Checking"))
            });
            click_account_text(&mut client, "ACCOUNTS");
            // Opening Accounts checks the unsigned homes again. Wait for the rows to settle
            // before closing the list and taking coordinates for the sidebar detail click.
            client.wait_for_rows("Accounts discovery completed", |rows| {
                rows.iter().any(|r| r.contains("Enter Details"))
                    && !rows.iter().any(|r| r.contains("Existing ") || r.contains("Checking"))
            });
            client.send(b"\x1b");
            client.wait_for_rows("Accounts closed", |rows| !rows.iter().any(|r| r.contains("Enter Details")));
            click_account_text(&mut client, "No usage yet · 1 pane");
            client.wait_for_row("Enter Back · Tab Actions");
            client.send(b"\r");
            client.wait_for_rows("Details closed", |rows| {
                !rows.iter().any(|r| r.contains("Enter Back") || r.contains("Enter Details"))
            });
        } else {
            assert!(!client.rows().iter().any(|r| r.contains("ACCOUNTS")));
        }
        client.send(b"\x02An");
        client.wait_for_row("Agent   Codex");
        client.wait_for_row("Name    e.g. Work or Personal");
        client.send(b"\x15work\r");
        client.wait_for_row("already has an account named");
        client.wait_for_row("Name    work");
        client.wait_for_row("Agent   Codex");
        assert!(!home.join("logins").exists());
        client.send(b"\x15second");
        client.wait_for_row("Name    second");
        click_account_text(&mut client, "Sign in");
        client.wait_for_row("Account added");
        client.wait_for_row("Codex · second");
        client.send(b"\r");
        client.wait_for_row("Enter Back · Tab Actions");
        let rows = client.wait_for_row("No usage yet");
        assert!(!rows.iter().any(|r| r.contains("0 panes")));
        client.close_overlay("Enter Back · Tab Actions");
        let added: toml::Value = toml::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        let second =
            added["account"].as_array().unwrap().iter().find(|a| a["name"].as_str() == Some("second")).unwrap();
        let second_home = PathBuf::from(second["home"].as_str().unwrap());
        assert_eq!(std::fs::read_to_string(second_home.join("logins")).unwrap(), "login\n");
        assert!(second.get("sign_in_required").is_none());
        client.close_overlay("Enter Details");
        client.send(b"\x02a");
        client.wait_for_row("Start an agent");
        client.send(b"\t");
        let rows = client.wait_for_row("Start in new tab");
        assert!(rows.iter().any(|r| r.contains("Account details")));
        for absent in ["Rename", "Remove", "Start in worktree"] {
            assert!(!rows.iter().any(|r| r.contains(absent)), "{rows:?}");
        }
        if size.0 == 80 {
            assert!(!rows.iter().any(|r| r.contains("Add account")), "{rows:?}");
        }
        client.close_overlay("Start in new tab");
        client.send(b"second");
        client.wait_for_row("1/2");
        click_account_text(&mut client, "Codex · second");
        client.wait_for_row("READY account=codex:second");
        let launched: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        assert_eq!(launched.as_array().unwrap().iter().filter(|p| p["account"] == "codex:second").count(), 1);
        client.send(b"\x02Al");
        client.wait_for_row("Enter Details");
        client.send(b"second");
        client.wait_for_row("1/2");
        click_account_text(&mut client, "Codex · second");
        client.wait_for_row("Enter Back · Tab Actions");
        client.send(b"\t");
        click_account_text(&mut client, "Rename");
        client.wait_for_row("Rename Codex · second");
        client.send(b"\x15renamed\r");
        client.wait_for_row("Codex · renamed");
        client.send(b"\r");
        client.wait_for_row("Enter Back · Tab Actions");
        client.wait_for_row("╭ Codex · renamed");
        client.send(b"\x1b");
        client.wait_for_row("Enter Details");
        click_account_text(&mut client, "Codex · work");
        client.send(b"\t");
        click_account_text(&mut client, "Remove");
        client.wait_for_row("Conversations and the agent's own sign-in are kept.");
        client.send(b"\r");
        client.wait_for_row("Enter Details");
        client.send(b"\x18");
        client.wait_for_row("Conversations and the agent's own sign-in are kept.");
        client.send(b"y");
        client.wait_for_row("Account removed");
        assert_eq!(std::fs::read_to_string(home.join("signed-in")).unwrap(), "credential");
        assert_eq!(std::fs::read_to_string(home.join("conversation")).unwrap(), "history");
        env.run(&["send", "-p", "agent", "still-here\r"]);
        env.wait_for_text("agent", "reply:still-here");
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        assert_eq!(panes.as_array().unwrap().iter().find(|p| p["name"] == "agent").unwrap()["account"], "codex:work");
    }
}

#[test]
fn the_add_form_names_an_existing_sign_in_without_running_login() {
    let env = Env::new("account-form-existing");
    let path = existing_sign_in_agent(&env, "claude");
    let standard = env.root.join("home/.claude");
    std::fs::create_dir_all(&standard).unwrap();
    std::fs::write(standard.join("status"), "0").unwrap();
    let file = env.root.join("config/kraal/accounts.toml");
    std::fs::write(
        &file,
        format!(
            "[[account]]\nid = 'test-claude-work'\nagent='claude'\nname='work'\nhome={:?}\n",
            env.root.join("work").to_str().unwrap()
        ),
    )
    .unwrap();
    env.workspace("w", "layout { pane name=\"shell\" }\n");
    let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mut client = RawClient::attach_sized(&env, Some("w"), (80, 24)).traced();
    client.send(b"\x02Al");
    wait_for_account_review(&client, "Claude");
    click_account_text(&mut client, "+ Add account");
    let rows = client.wait_for_row("Enter Review");
    assert!(rows.iter().any(|r| r.contains("Existing Claude sign-in needs review.")));
    assert!(!rows.iter().any(|r| r.contains("Name    ")));
    let pending_file = std::fs::read_to_string(&file).unwrap();
    click_account_text(&mut client, "Sign in to another account");
    client.wait_for_row("Name    e.g. Work or Personal");
    client.send(b"\r\r");
    client.wait_for_row("Enter a name for this account.");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), pending_file);
    assert!(!env.root.join("home/logins").exists());
    click_account_text(&mut client, "Cancel");
    client.wait_for_row("+ Add account");
    click_account_text(&mut client, "+ Add account");
    client.wait_for_row("Enter Review");
    client.send(b"\r");
    client.wait_for_row("Which account is this?");
    click_account_text(&mut client, "A different account");
    client.wait_for_row("Name    e.g. Work or Personal");
    let before = std::fs::read_to_string(&file).unwrap();
    let checks =
        || common::read_complete_lines(env.root.join("home/status-checks")).unwrap_or_default().lines().count();
    let finished = || common::read_complete_lines(standard.join("check-finished")).unwrap_or_default().lines().count();
    let (before_checks, before_finished) = (checks(), finished());
    std::fs::write(standard.join("hold-check"), "").unwrap();
    client.send(b"\x15cancelled\r\r");
    client.wait_for_row("Checking sign-in...");
    common::wait_until(PATIENCE, "one check started", || checks() == before_checks + 1);
    click_account_text(&mut client, "Cancel");
    client.wait_for_row("+ Add account");
    std::fs::remove_file(standard.join("hold-check")).unwrap();
    common::wait_until(PATIENCE, "cancelled check finished", || finished() == before_finished + 1);
    assert_eq!(checks(), before_checks + 1, "Enter while checking did not start another check");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), before);
    click_account_text(&mut client, "+ Add account");
    client.wait_for_row("Enter Review");
    click_account_text(&mut client, "Use existing account");
    client.wait_for_row("Which account is this?");
    click_account_text(&mut client, "A different account");
    client.wait_for_row("Name    e.g. Work or Personal");
    client.send(b"\x15personal\r");
    client.wait_for_row("Account added");
    client.wait_for_row("Claude · personal");
    assert!(!env.root.join("home/logins").exists());
    let accounts: toml::Value = toml::from_str(&std::fs::read_to_string(file).unwrap()).unwrap();
    let a = accounts["account"].as_array().unwrap().iter().find(|a| a["name"].as_str() == Some("personal")).unwrap();
    assert_eq!(a["home"].as_str().unwrap(), "~/.claude");
    assert!(a.get("review").is_none());
}

#[test]
fn a_different_account_review_after_sign_out_exposes_sign_in() {
    let env = Env::new("account-review-signed-out");
    let path = existing_sign_in_agent(&env, "claude");
    let standard = env.root.join("home/.claude");
    std::fs::create_dir_all(&standard).unwrap();
    std::fs::write(standard.join("status"), "0").unwrap();
    let file = env.root.join("config/kraal/accounts.toml");
    std::fs::write(
        &file,
        format!(
            "[[account]]\nid = 'test-claude-work'\nagent='claude'\nname='work'\nhome={:?}\n",
            env.root.join("work").to_str().unwrap()
        ),
    )
    .unwrap();
    env.workspace("w", "layout { pane name=\"shell\" }\n");
    let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mut client = RawClient::attach_sized(&env, Some("w"), (80, 24));
    client.send(b"\x02Al");
    wait_for_account_review(&client, "Claude");
    assert!(std::fs::read_to_string(&file).unwrap().contains("review = true"));
    // Discovery succeeded, but the agent signs out before the user resolves its review.
    std::fs::write(standard.join("status"), "1").unwrap();
    click_account_text(&mut client, "? Which account is ~/.claude");
    client.wait_for_row("Which account is this?");
    click_account_text(&mut client, "A different account");
    client.wait_for_row("Name    e.g. Work or Personal");
    client.send(b"personal\r");
    client.wait_for_row("Account added");
    client.wait_for_row("Claude · personal");
    client.wait_for_row("Sign-in required");
    assert!(!env.root.join("home/logins").exists());
    let accounts: toml::Value = toml::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let accounts = accounts["account"].as_array().unwrap();
    assert_eq!(accounts.len(), 2);
    let a = accounts.iter().find(|a| a["name"].as_str() == Some("personal")).unwrap();
    assert_eq!(a["home"].as_str().unwrap(), "~/.claude");
    assert_eq!(a["sign_in_required"].as_bool(), Some(true));
    assert!(a.get("review").is_none());
    client.send(b"\r");
    client.wait_for_row("Enter Back · Tab Actions");
    client.send(b"\t");
    client.wait_for_row("Sign in");
    click_account_text(&mut client, "Sign in");
    common::wait_until(PATIENCE, "the account's Sign in action starts login", || {
        common::read_complete_lines(env.root.join("home/logins")).is_some()
    });
    assert_eq!(std::fs::read_to_string(env.root.join("home/logins")).unwrap(), "unexpected\n");
}

#[test]
fn sidebar_accounts_discover_a_sign_in_completed_after_startup() {
    for entry in ["ACCOUNTS", "+ Add account"] {
        let env = Env::new(if entry == "ACCOUNTS" { "sidebar-accounts-discovery" } else { "sidebar-add-discovery" });
        let path = existing_sign_in_agent(&env, "claude");
        let standard = env.root.join("home/.claude");
        std::fs::create_dir_all(&standard).unwrap();
        std::fs::write(standard.join("status"), "1").unwrap();
        let file = env.root.join("config/kraal/accounts.toml");
        std::fs::write(
            &file,
            format!(
                "[[account]]\nid = 'test-claude-work'\nagent='claude'\nname='work'\nhome={:?}\n",
                env.root.join("work").to_str().unwrap()
            ),
        )
        .unwrap();
        env.workspace("w", "layout { pane name=\"shell\" }\n");
        let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let mut client = RawClient::attach_sized(&env, Some("w"), (124, 36)).traced();
        common::wait_until(PATIENCE, "startup checked the unsigned home", || {
            common::read_complete_lines(standard.join("check-finished")).is_some()
        });
        // The script's marker precedes the server's result. Both "Checking sign-in" and
        // "Checking ~/.claude" must be gone before the external sign-in and refresh.
        client.wait_for_rows("startup discovery completed", |rows| {
            rows.iter().any(|r| r.contains("ACCOUNTS")) && !rows.iter().any(|r| r.contains("Checking"))
        });
        assert!(!std::fs::read_to_string(&file).unwrap().contains("review = true"));

        // The agent signs in outside kraal after the server cached its unsigned status.
        std::fs::write(standard.join("status"), "0").unwrap();
        click_account_text(&mut client, entry);
        if entry == "ACCOUNTS" {
            wait_for_account_review(&client, "Claude");
            click_account_text(&mut client, "+ Add account");
        }
        client.wait_for_row("Use existing account");
        assert!(std::fs::read_to_string(&file).unwrap().contains("review = true"));
        assert!(!env.root.join("home/logins").exists());
    }
}

#[test]
fn account_changes_leave_saved_sessions_workspace_files_and_name_caches_untouched() {
    let env = Env::new("account-files");
    let accounts = env.root.join("config/kraal/accounts.toml");
    std::fs::write(&accounts, "[[account]]\nid = 'work'\nname = 'work'\nagent = 'claude'\nhome = '/work'\n").unwrap();
    let source = "// Keep the label.\nlayout { pane agent=\"claude:work\"; }\n";
    env.workspace("w", source);
    let usage = env.root.join("state/kraal/usage/claude-work.json");
    let files = [
        (env.legacy_state_file(), b"unreadable old session".as_slice()),
        (env.state_file(), b"unreadable current session".as_slice()),
        (usage, b"old usage".as_slice()),
    ];
    for (path, bytes) in &files {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    env.run(&["accounts", "rename", "claude:work", "renamed"]);
    for (path, bytes) in &files {
        assert_eq!(std::fs::read(path).unwrap(), *bytes);
    }
    assert_eq!(std::fs::read_to_string(env.root.join("config/kraal/workspaces/w.kdl")).unwrap(), source);
    assert!(!env.root.join("state/kraal/usage/by-id").exists());
    assert!(!accounts.with_extension("toml.kraal-bak").exists());
}
