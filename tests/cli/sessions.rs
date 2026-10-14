use super::support::{PATIENCE, RawClient, Stopped, click_account_text, finish};
#[cfg(target_os = "linux")]
use super::support::{in_terminal, wait_gone};
use crate::common;
use crate::common::{Env, wait_until, write_script};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

#[test]
#[cfg(target_os = "linux")]
fn supervise_ends_what_its_program_left_however_it_left() {
    let env = Env::new("supervise");
    let gone = |pidfile: &Path| {
        let pid = std::fs::read_to_string(pidfile).unwrap().trim().to_string();
        let start = Instant::now();
        while Path::new(&format!("/proc/{pid}")).exists() {
            assert!(start.elapsed() < Duration::from_secs(5), "{pid} still runs");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    // Nothing holds the test's own output if a helper survives (the test then fails, not hangs).
    let quiet = |mut c: std::process::Command| {
        c.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        c
    };
    // A helper started with a cleared environment, in a session of its own, by a program that
    // then ends: an orphan nothing tags, and outside the program's group.
    let pidfile = env.root.join("helper.pid");
    let helper = format!("env -i setsid sh -c 'sleep 600 & echo $! > {}'", pidfile.display());
    let status = quiet(env.cmd(&["supervise", "sh", "-c", &format!("{helper}; sleep 0.3; exit 3")])).status().unwrap();
    assert_eq!(status.code(), Some(3), "the program's own status");
    gone(&pidfile);
    // Stopped while the program runs: the program and the helper both go.
    let pidfile = env.root.join("helper2.pid");
    let helper = format!("env -i setsid sh -c 'sleep 600 & echo $! > {}'", pidfile.display());
    let mut supervisor =
        quiet(env.cmd(&["supervise", "sh", "-c", &format!("{helper}; exec sleep 600")])).spawn().unwrap();
    let start = Instant::now();
    while std::fs::read_to_string(&pidfile).map_or(true, |p| p.trim().is_empty()) {
        assert!(start.elapsed() < Duration::from_secs(5), "the helper never started");
        std::thread::sleep(Duration::from_millis(20));
    }
    // SAFETY: plain syscall on our own child.
    unsafe {
        libc::kill(supervisor.id() as libc::pid_t, libc::SIGTERM);
    }
    let start = Instant::now();
    while supervisor.try_wait().unwrap().is_none() {
        assert!(start.elapsed() < Duration::from_secs(5), "the supervisor did not end");
        std::thread::sleep(Duration::from_millis(20));
    }
    gone(&pidfile);
}

#[test]
fn layout_survives_a_server_restart() {
    let env = Env::new("restore");
    env.workspace(
        "fleet",
        r#"layout {
    tab name="servers" {
        pane split_direction="vertical" {
            pane name="a" command="sh" { args "-c" "echo node-a; exec sleep 1000"; }
            pane name="b" command="sh" { args "-c" "echo node-b; exec sleep 1000"; }
        }
    }
    tab name="tab 2" {
        pane name="shell"
    }
}
"#,
    );
    env.run(&["open", "fleet"]);
    env.wait_for_text("b", "node-b");
    env.run(&["rename", "-p", "shell", "renamed"]);
    let pid = env.server_pid().expect("the server runs");
    env.run(&["kill"]);
    // `kill` returns once the server has saved the layout and exited.
    assert!(env.server_pid().is_none(), "server {pid} still runs");
    assert!(env.state_file().exists());
    // Any command starting the server restores the saved layout.
    env.run(&["open", "fleet"]);
    env.wait_for_text("a", "node-a");
    let panes = env.run(&["panes"]);
    assert!(panes.contains("renamed"), "{panes}");
    assert_eq!(panes.lines().count(), 3, "{panes}");
    // A tab named "tab 2" keeps its name; older versions saved a tab without a name that way,
    // and in their states it shows what its pane runs.
    assert!(panes.contains("tab 2"), "{panes}");
    env.run(&["kill"]);
    let state = std::fs::read_to_string(env.state_file()).unwrap();
    let mut old: serde_json::Value = serde_json::from_str(&state).unwrap();
    old["version"] = serde_json::json!(1);
    std::fs::create_dir_all(env.legacy_state_file().parent().unwrap()).unwrap();
    std::fs::write(env.legacy_state_file(), serde_json::to_vec(&old).unwrap()).unwrap();
    std::fs::remove_file(env.state_file()).unwrap();
    env.run(&["open", "fleet"]);
    env.wait_for_text("a", "node-a");
    let panes = env.run(&["panes"]);
    assert!(!panes.contains("tab 2") && panes.contains("renamed"), "{panes}");
}

#[test]
fn migrated_source_status_tracks_disk_edits_across_restarts() {
    source_status_tracks_disk_edits_across_restarts(false);
}

#[test]
fn already_migrated_source_status_tracks_disk_edits_across_restarts() {
    source_status_tracks_disk_edits_across_restarts(true);
}

fn source_status_tracks_disk_edits_across_restarts(already_migrated: bool) {
    let env = Env::new(if already_migrated { "current-source-baseline" } else { "source-baseline" });
    let source = "// Original source\nlayout { pane name=\"original\"; }\n";
    env.workspace("source", source);
    env.workspace("closed", source);
    env.run(&["open", "source"]);
    env.run(&["run", "--tab", "--name", "local", "--", "sh"]);
    env.run(&["kill"]);
    let mut old: serde_json::Value = serde_json::from_slice(&std::fs::read(env.state_file()).unwrap()).unwrap();
    old["workspaces"][0].as_object_mut().unwrap().remove("definition_source");
    if already_migrated {
        std::fs::write(env.state_file(), serde_json::to_vec(&old).unwrap()).unwrap();
    }
    old["version"] = serde_json::json!(1);
    old["revision"] = serde_json::json!(1);
    old.as_object_mut().unwrap().remove("workspace_order");
    for field in ["id", "definition_source"] {
        old["workspaces"][0].as_object_mut().unwrap().remove(field);
    }
    let legacy = serde_json::to_vec(&old).unwrap();
    std::fs::create_dir_all(env.legacy_state_file().parent().unwrap()).unwrap();
    std::fs::write(env.legacy_state_file(), &legacy).unwrap();
    if !already_migrated {
        std::fs::remove_file(env.state_file()).unwrap();
    }

    for restart in [false, true] {
        env.run(&["open", "source"]);
        if !restart {
            env.run(&["run", "--tab", "--name", "later", "--", "sh"]);
        }
        let mut client = RawClient::attach(&env, "source");
        client.wait_for_message("\"attached\"");
        client.send(b"\x02w");
        let rows = client.wait_for_row("3 tabs · 3 panes");
        assert!(!rows.iter().any(|r| r.contains("File changed")), "{rows:?}");
        assert!(rows.iter().any(|r| r.contains("Closed")), "{rows:?}");
        env.run(&["kill"]);
        let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(env.state_file()).unwrap()).unwrap();
        assert_eq!(saved["workspaces"][0]["definition_source"], source);
    }

    env.run(&["open", "source"]);
    let path = env.root.join("config/kraal/workspaces/source.kdl");
    std::fs::write(&path, "layout { pane name=\"changed\"; }").unwrap();
    for restart in [false, true] {
        if restart {
            env.run(&["open", "source"]);
        }
        let mut client = RawClient::attach(&env, "source");
        client.wait_for_message("\"attached\"");
        client.send(b"\x02w");
        // In-place writes do not wake the directory watcher. An idle server scans every 30 seconds.
        wait_until(PATIENCE, "the source edit was not noticed", || {
            client.rows().iter().any(|r| r.contains("File changed"))
        });
        client.wait_for_row("File changed");
        client.send(b"\t");
        click_account_text(&mut client, "Review");
        client.wait_for_row("Saved workspace: 3 tabs · 3 panes");
        env.run(&["kill"]);
    }
    assert_eq!(std::fs::read(env.legacy_state_file()).unwrap(), legacy);
}

#[test]
fn legacy_files_stay_unchanged_through_migration_and_workspace_lifecycle() {
    let env = Env::new("wsidentity");
    let source = "// Keep this source\nlayout { pane name=\"source-pane\"; }\n";
    env.workspace("w", source);
    env.run(&["open", "w"]);
    env.run(&["rename", "-p", "source-pane", "personal-pane"]);
    env.run(&["send", "-p", "personal-pane", "--enter", "echo legacy-$((40+2))"]);
    env.wait_for_text("personal-pane", "legacy-42");
    env.run(&["kill"]);
    let mut old: serde_json::Value = serde_json::from_slice(&std::fs::read(env.state_file()).unwrap()).unwrap();
    old["version"] = serde_json::json!(1);
    old["revision"] = serde_json::json!(1);
    old["workspaces"][0].as_object_mut().unwrap().remove("id");
    let mut undo = old["workspaces"][0].clone();
    undo["tabs"][0]["root"]["Pane"]["spec"]["account"] = serde_json::json!("claude:work");
    old["workspaces"][0]["layout_undo"] = undo;
    let bytes = serde_json::to_vec_pretty(&old).unwrap();
    let path = env.legacy_state_file();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    std::fs::remove_file(env.state_file()).unwrap();
    let scrollback = env.root.join("state/kraal/scrollback").join(&env.session);
    std::fs::create_dir_all(scrollback.parent().unwrap()).unwrap();
    std::fs::rename(env.root.join("state/kraal/scrollback-v2").join(&env.session), &scrollback).unwrap();
    std::fs::write(scrollback.join("9-9-9.ans"), b"unreferenced output\0\xff").unwrap();
    let files = || {
        let mut files: Vec<_> = std::fs::read_dir(&scrollback)
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                (e.file_name(), std::fs::read(e.path()).unwrap())
            })
            .collect();
        files.sort();
        files
    };
    let original_scrollback = files();
    let unchanged = || {
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(files(), original_scrollback);
        assert_eq!(std::fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    };
    let stopped = env.run(&["ls"]);
    assert_eq!(stopped.lines().filter(|line| line.starts_with(&env.session)).count(), 1, "{stopped}");
    unchanged();
    let accounts = env.root.join("config/kraal/accounts.toml");
    let original_accounts = "[[account]]\nname = \"work\"\nagent = \"claude\"\nhome = \"~/work\"\n";
    std::fs::write(&accounts, original_accounts).unwrap();
    env.run(&["accounts", "rename", "claude:work", "renamed"]);
    let migrated: serde_json::Value = serde_json::from_slice(&std::fs::read(env.state_file()).unwrap()).unwrap();
    assert_eq!(migrated["version"], 2);
    assert!(migrated["workspaces"][0]["layout_undo"]["tabs"][0]["root"]["Pane"]["spec"]["account_id"].is_string());
    assert_eq!(std::fs::read_to_string(accounts.with_extension("toml.kraal-bak")).unwrap(), original_accounts);
    unchanged();
    env.run(&["open", "w"]);
    env.wait_for_text("personal-pane", "legacy-42");
    let list = || serde_json::from_str::<serde_json::Value>(&env.run(&["workspaces", "--json"])).unwrap();
    let id = list()[0]["id"].as_str().unwrap().to_owned();
    assert!(!id.is_empty());
    unchanged();
    env.run(&["kill"]);
    unchanged();
    env.run(&["open", "w"]);
    assert_eq!(list()[0]["id"], id);
    env.wait_for_text("personal-pane", "legacy-42");
    unchanged();
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_row("$ ");
    env.run(&["workspaces", "close", &id, "--yes"]);
    assert_eq!(list()[0]["open"], false);
    unchanged();
    client.wait_for_row("Closed");
    client.send(b"\x02w\t");
    click_account_text(&mut client, "Delete");
    client.wait_for_row("Delete w?");
    client.send(b"y");
    client.wait_for_row("Ctrl+Z Undo");
    assert!(list().as_array().unwrap().is_empty());
    unchanged();
    client.send(b"\x1a");
    client.wait_for_row("Workspace restored");
    assert_eq!(list()[0]["id"], id);
    assert_eq!(list()[0]["open"], false);
    unchanged();
    env.run(&["workspaces", "delete", &id, "--yes"]);
    env.run(&["kill"]);
    unchanged();
    env.run(&["workspaces", "restore", &id]);
    assert_eq!(list()[0]["id"], id);
    unchanged();
    env.run(&["open", "w"]);
    env.wait_for_text("personal-pane", "legacy-42");
    env.run(&["send", "-p", "personal-pane", "--enter", "exit"]);
    wait_until(PATIENCE, "the last pane ending leaves the workspace empty", || list()[0]["panes"] == 0);
    let empty = list();
    assert_eq!(empty[0]["open"], true);
    assert_eq!(empty[0]["tabs"], 0);
    assert_eq!(empty[0]["id"], id);
    unchanged();
    env.run(&["kill"]);
    unchanged();
    env.run(&["open", "w"]);
    assert_eq!(list(), empty, "the empty workspace stays empty after restoring");
    unchanged();
    env.run(&["kill"]);
    unchanged();
    let stopped = env.run(&["ls"]);
    assert_eq!(stopped.lines().filter(|line| line.starts_with(&env.session)).count(), 1, "{stopped}");
    env.run(&["forget"]);
    unchanged();
    assert!(!env.state_file().exists());
    assert!(!env.root.join("state/kraal/scrollback-v2").join(&env.session).exists());
    assert!(!env.run(&["ls"]).contains(&env.session));
    assert!(env.run(&["forget"]).contains("nothing saved"));
    env.run(&["open", "w"]);
    assert!(env.run(&["panes"]).contains("source-pane"));
    assert_ne!(list()[0]["id"], id);
    unchanged();
    assert_eq!(std::fs::read_to_string(env.root.join("config/kraal/workspaces/w.kdl")).unwrap(), source);
}

#[test]
fn a_failed_state_migration_never_starts_an_empty_session_or_replaces_the_source() {
    let env = Env::new("wsbadstate");
    env.workspace("w", "layout { pane; }\n");
    std::fs::create_dir_all(env.legacy_state_file().parent().unwrap()).unwrap();
    for bytes in [
        "{",
        r#"{"version":2,"workspaces":[]}"#,
        r#"{"version":1,"revision":3,"workspaces":[]}"#,
        r#"{"version":1,"revision":2,"workspaces":[]}"#,
    ] {
        std::fs::write(env.legacy_state_file(), bytes).unwrap();
        let out = env.try_run(&["open", "w"]);
        let said = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && said.contains("the saved session is unchanged"), "{said}");
        if bytes.contains("\"revision\":2") {
            assert!(said.contains("unsupported state revision 2; use a newer kraal"), "{said}");
        }
        assert_eq!(std::fs::read_to_string(env.legacy_state_file()).unwrap(), bytes);
        assert!(!env.state_file().exists());
        assert!(env.server_pid().is_none());
    }
    // Repairing the original is sufficient; no hidden replacement needs to be removed first.
    std::fs::write(env.legacy_state_file(), r#"{"version":1,"workspaces":[]}"#).unwrap();
    env.run(&["open", "w"]);
    assert!(env.server_pid().is_some());
}

#[test]
fn account_migration_refuses_unreadable_sessions_before_changing_the_account_file() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new("unreadable-account-state");
    let accounts = env.root.join("config/kraal/accounts.toml");
    let source = "[[account]]\nname = 'work'\nagent = 'claude'\nhome = '/work'\n";
    std::fs::write(&accounts, source).unwrap();
    for path in [env.state_file(), env.legacy_state_file()] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"version":1,"workspaces":[]}"#).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let out = env.try_run(&["accounts", "rename", "claude:work", "renamed"]);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && error.contains("accounts.toml is unchanged"), "{error}");
        assert_eq!(std::fs::read_to_string(&accounts).unwrap(), source);
        assert!(!accounts.with_extension("toml.kraal-bak").exists());
        std::fs::remove_file(path).unwrap();
    }
}

#[test]
fn newer_state_is_refused_without_falling_back_or_overwriting_either_format() {
    let env = Env::new("future-state");
    env.workspace("w", "layout { pane; }\n");
    let current = env.state_file();
    let legacy = env.legacy_state_file();
    let future = br#"{"version":3,"future_layout":[]}"#;
    let original = br#"{"version":1,"workspaces":[]}"#;
    for (path, bytes) in [(&current, future.as_slice()), (&legacy, original.as_slice())] {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    for args in [&["workspaces", "--json"][..], &["open", "w"]] {
        let out = env.try_run(args);
        let error = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && error.contains("use a newer kraal"), "{error}");
        assert_eq!(std::fs::read(&current).unwrap(), future);
        assert_eq!(std::fs::read(&legacy).unwrap(), original);
        assert!(env.server_pid().is_none());
    }
}

/// A kraal the tests run stops rather than use what is the developer's, however a test let it
/// through: as the home, as a directory of kraal's or of an agent, or in `PATH`.
#[test]
fn a_kraal_run_by_a_test_stays_out_of_what_is_the_developers() {
    let env = Env::new("keepout");
    let theirs = env.root.join("theirs");
    std::fs::create_dir_all(theirs.join("run")).unwrap();
    let kraal = || {
        let mut c = env.cmd(&["ls"]);
        c.env(common::isolation::KEEP_OUT, &theirs);
        c
    };
    let path = format!("{}:{}", theirs.join("bin").display(), std::env::var("PATH").unwrap());
    for (name, value) in [
        ("HOME", theirs.clone().into_os_string()),
        ("XDG_CONFIG_HOME", theirs.join(".config").into()),
        ("XDG_STATE_HOME", theirs.join(".local/state").into()),
        ("XDG_DATA_HOME", theirs.join(".local/share").into()),
        ("XDG_RUNTIME_DIR", theirs.join("run").into()),
        ("CLAUDE_CONFIG_DIR", theirs.join(".claude-work").into()),
        ("CODEX_HOME", theirs.join(".codex").into()),
        ("PATH", path.into()),
    ] {
        let out = kraal().env(name, &value).output().unwrap();
        let said = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{name} let through");
        let named = said.starts_with("kraal: run by a test, ") && said.contains(&*theirs.to_string_lossy());
        assert!(named, "{name}: {said}");
    }
    let out = kraal().output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!theirs.join("run/kraal").exists(), "nothing was made there");
}

#[test]
fn commands_run_in_a_pane_stay_in_its_session() {
    let a = Env::new("sessa");
    a.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    a.run(&["open", "w"]);
    let base = || a.isolated(Path::new(env!("CARGO_BIN_EXE_kraal")));
    // A second session in the same directories, whose pane has the same id.
    let b = |args: &[&str]| base().args(["-s", "sessb"]).args(args).output().unwrap();
    assert!(b(&["open", "w"]).status.success());
    let id = |out: &str| -> String {
        let v: serde_json::Value = serde_json::from_str(out).unwrap();
        v[0]["id"].to_string()
    };
    let pane_a = id(&a.run(&["panes", "--json"]));
    let pane_b = id(&String::from_utf8(b(&["panes", "--json"]).stdout).unwrap());
    assert_eq!(pane_a, pane_b);
    // As if typed in session a's pane: no `-s`, the pane's variables.
    let in_pane = |args: &[&str]| {
        base()
            .args(args)
            .env("KRAAL_SESSION", "sessa")
            .env("KRAAL_SOCKET", a.root.join("run/kraal/sessa.sock"))
            .env("KRAAL_PANE", &pane_a)
            .output()
            .unwrap()
    };
    // Another session's pane is never the one the command runs in.
    let out = in_pane(&["-s", "sessb", "close"]);
    assert!(!out.status.success(), "closed a pane of sessb");
    assert!(String::from_utf8_lossy(&b(&["panes"]).stdout).contains("one"));
    // `kill` without `-s` stops the session it runs in, even with another session named in
    // `$KRAAL_SESSION_NAME` (exported by a shell's startup files, inherited by every pane).
    let out = base()
        .arg("kill")
        .env("KRAAL_SESSION", "sessa")
        .env("KRAAL_SOCKET", a.root.join("run/kraal/sessa.sock"))
        .env("KRAAL_PANE", &pane_a)
        .env("KRAAL_SESSION_NAME", "sessb")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!a.try_run(&["panes"]).status.success(), "sessa still runs");
    assert!(b(&["panes"]).status.success(), "sessb was stopped");
    // Outside kraal, `$KRAAL_SESSION_NAME` still names the session.
    let named = base().arg("panes").env("KRAAL_SESSION_NAME", "sessb").output().unwrap();
    assert!(named.status.success() && String::from_utf8_lossy(&named.stdout).contains("one"));
    let _ = b(&["kill"]);
}

#[test]
fn kill_says_when_the_layout_was_not_saved() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new("unsaved");
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    env.run(&["open", "w"]);
    let sessions = env.root.join("state/kraal/sessions-v2");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o500)).unwrap();
    let out = env.try_run(&["kill"]);
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o700)).unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("could not be saved"), "{err}");
    assert!(env.server_pid().is_none(), "the server stopped anyway");
}

/// A session has one server: another one started for it while it runs refuses to, even when
/// the first one's socket is gone (a cleaner of /tmp may remove it), so that two servers never
/// write the same session.
#[test]
fn a_session_never_has_two_servers() {
    let env = Env::new("oneserver");
    let _evidence = Evidence(&env);
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    env.run(&["open", "w"]);
    let first = env.server_pid().expect("the server runs");
    let socket = env.root.join("run/kraal/oneserver.sock");
    let aside = env.root.join("run/kraal/aside.sock");
    std::fs::rename(&socket, &aside).unwrap();
    let second = env.cmd(&["server"]).stderr(std::process::Stdio::piped()).spawn().unwrap();
    let out = finish(second, Duration::from_secs(10));
    std::fs::rename(&aside, &socket).unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("holds session \"oneserver\""), "{err}");
    assert_eq!(env.server_pid(), Some(first));
    env.run(&["panes"]);
}

/// `kraal forget` deletes nothing of a session whose server runs, even one whose socket is gone:
/// a server holds its session, and `forget` must hold it too.
#[test]
fn forget_leaves_a_running_session_alone() {
    let env = Env::new("forgetheld");
    let _evidence = Evidence(&env);
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    env.run(&["open", "w"]);
    wait_until(Duration::from_secs(10), "the layout was never saved", || env.state_file().exists());
    let socket = env.root.join("run/kraal/forgetheld.sock");
    let aside = env.root.join("run/kraal/aside.sock");
    std::fs::rename(&socket, &aside).unwrap();
    let out = env.try_run(&["forget"]);
    std::fs::rename(&aside, &socket).unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("is running"), "{err}");
    assert!(env.state_file().exists(), "the layout was deleted");
}

/// The lock file of `env`'s session in one of its places: where the session saves (`state`), or
/// one of its two runtime directories (`run`, and `run2` of [`environment`]).
fn lock_file(env: &Env, place: &str) -> PathBuf {
    env.root.join(place).join(format!("kraal/{}.lock", env.session))
}

/// How long a start that finds its session held looks for that one's server before it gives up:
/// the time a start may take, and not much more.
fn gave_up_in_time(took: Duration) -> bool {
    took >= Duration::from_secs(5) && took < Duration::from_secs(20)
}

/// The same user, state and configuration as `env`, with another runtime directory when
/// `other`: a cron job without `$XDG_RUNTIME_DIR`, say, beside a desktop terminal that has it.
fn environment(env: &Env, other: bool, args: &[&str]) -> Command {
    let mut c = env.cmd(args);
    if other {
        let run = env.root.join("run2");
        std::fs::create_dir_all(&run).unwrap();
        c.env("XDG_RUNTIME_DIR", run);
    }
    c
}

/// Stops the server that `env`'s session has in its other runtime directory when the test
/// ends, however it ends: `Env` stops the one of its own.
struct StopsOther<'a>(&'a Env);

impl Drop for StopsOther<'_> {
    fn drop(&mut self) {
        use std::process::Stdio;
        let _ = environment(self.0, true, &["kill"]).stdout(Stdio::null()).stderr(Stdio::null()).status();
    }
}

/// What `kraal args` prints in one of the two environments of `env`, which must succeed, and
/// within the time any command may take ([`bounded`]).
fn in_environment(env: &Env, other: bool, args: &[&str]) -> String {
    let out = bounded(env, other, args);
    let (said, err) = (String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let failed = format!("kraal {args:?} (other: {other}): {}\n{said}{err}", out.status);
    assert!(out.status.success(), "{failed}\n{}", evidence(env));
    said.into_owned()
}

/// What `kraal args` did in one of the two environments of `env`, within the time any command
/// may take: one that does not end is stopped, and fails the test with what is known of the
/// session ([`evidence`]). What it says goes to files, not to pipes: a pipe is read to its end,
/// which comes once every process that has it is gone, and on macOS a program another thread
/// starts at that moment is given it.
fn bounded(env: &Env, other: bool, args: &[&str]) -> Output {
    let mut command = environment(env, other, args);
    within(env, &mut command, &format!("kraal {args:?}"))
}

/// What `command`, a program of `env`'s test, did within the time any command may take, as
/// [`bounded`] has it.
fn within(env: &Env, command: &mut Command, what: &str) -> Output {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let said = env.root.join(format!("said-{}", NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    let file = |end: &str| std::fs::File::create(said.with_extension(end)).unwrap();
    let mut child = command.stdout(file("out")).stderr(file("err")).spawn().unwrap();
    let start = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break Some(status);
        }
        if start.elapsed() > PATIENCE {
            let _ = child.kill();
            let _ = child.wait();
            break None;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let read = |end: &str| std::fs::read(said.with_extension(end)).unwrap_or_default();
    let (stdout, stderr) = (read("out"), read("err"));
    let Some(status) = status else {
        let (out, err) = (String::from_utf8_lossy(&stdout), String::from_utf8_lossy(&stderr));
        panic!("{what} still ran after {PATIENCE:?}. It said:\n{out}{err}\n{}", evidence(env));
    };
    Output { status, stdout, stderr }
}

/// Says what is known of `env`'s session ([`evidence`]) when the test that has it fails,
/// whatever fails in it: an assertion, a command that did not end, a thread of its own.
struct Evidence<'a>(&'a Env);

impl Drop for Evidence<'_> {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("{}", evidence(self.0));
        }
    }
}

/// What is known of `env`'s session when a command did not do what a test expects: what its
/// servers logged, and what is where it saves and in its two runtime directories (sockets, their
/// second names, lock files).
fn evidence(env: &Env) -> String {
    let mut all = String::new();
    for dir in ["state/kraal", "run/kraal", "run2/kraal"] {
        let mut names: Vec<String> = std::fs::read_dir(env.root.join(dir))
            .into_iter()
            .flatten()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        all.push_str(&format!("--- {dir}: {names:?}\n"));
        for log in names.iter().filter(|name| name.starts_with("server-") && name.ends_with(".log")) {
            let logged = std::fs::read_to_string(env.root.join(dir).join(log)).unwrap_or_default();
            all.push_str(&format!("--- {log}:\n{logged}\n"));
        }
    }
    all
}

/// Where the session of `env` has a socket, of its two runtime directories.
fn sockets(env: &Env) -> Vec<&'static str> {
    let socket = format!("kraal/{}.sock", env.session);
    ["run", "run2"].into_iter().filter(|run| env.root.join(run).join(&socket).exists()).collect()
}

/// Hold a lock of `env`'s session, as another kraal would: the one where the session saves
/// (`state`) or the one of its runtime directory (`run`), with what that kraal says of itself
/// there. Held until what is returned is dropped.
fn hold(env: &Env, place: &str, said: &str) -> std::fs::File {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::DirBuilderExt;
    let dir = env.root.join(place).join("kraal");
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir).unwrap();
    let path = dir.join(format!("{}.lock", env.session));
    let mut lock = std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(path).unwrap();
    // Once nobody has it: a lock this test let go a moment ago is still that of a program
    // another test is starting, which has this process's files until it runs.
    // SAFETY: plain syscall on a file this test holds open.
    wait_until(PATIENCE, "the session is held", || unsafe {
        libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0
    });
    lock.write_all(said.as_bytes()).unwrap();
    lock
}

/// A session has one server for what it saves, whichever runtime directory a kraal of the user
/// resolves: started from one environment, it is not started, served or forgotten from the
/// other; in both orders. A start from the other looks for a server of its own for the time a
/// start may take, then says where the session runs and how to reach it. Stopped where it runs,
/// the session starts from the other.
#[test]
fn a_session_running_under_another_runtime_directory_is_refused_and_said() {
    for first in [false, true] {
        let env = Env::new(if first { "onestate-b" } else { "onestate-a" });
        let _other = StopsOther(&env);
        let _evidence = Evidence(&env);
        env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
        let (here, there) = (first, !first);
        let run = |other: bool| if other { "run2" } else { "run" };
        in_environment(&env, here, &["open", "w"]);
        wait_until(PATIENCE, "the layout was never saved", || env.state_file().exists());
        let where_it_is = format!("{}/kraal\", not in", run(here));
        let reach_it = format!("{}/kraal\" where this kraal looks: reach it with XDG_RUNTIME_DIR=", run(there));
        let start = Instant::now();
        let refused = bounded(&env, there, &["open", "w"]);
        let said = String::from_utf8_lossy(&refused.stderr);
        assert!(gave_up_in_time(start.elapsed()), "refused after {:?}", start.elapsed());
        assert!(!refused.status.success() && said.contains("session onestate-"), "{said}");
        assert!(said.contains("is not started here: kraal ") && said.contains(&where_it_is), "{said}");
        assert!(said.contains(&reach_it), "{said}");
        let forget = bounded(&env, there, &["forget"]);
        let said = String::from_utf8_lossy(&forget.stderr);
        assert!(!forget.status.success() && said.contains("is running (kraal ") && said.contains(&reach_it), "{said}");
        let second = bounded(&env, there, &["server"]);
        assert_eq!(second.status.code(), Some(75), "{}", String::from_utf8_lossy(&second.stderr));
        // Nothing of it was touched: it answers where it runs, with what it saved.
        assert_eq!((sockets(&env), env.state_file().exists()), (vec![run(here)], true));
        assert!(in_environment(&env, here, &["panes"]).contains("one"));
        assert!(in_environment(&env, here, &["kill"]).contains("stopped session"));
        assert_eq!((sockets(&env), lock_file(&env, "state").exists()), (Vec::new(), false));
        in_environment(&env, there, &["open", "w"]);
        assert!(in_environment(&env, there, &["panes"]).contains("one"));
        in_environment(&env, there, &["kill"]);
        in_environment(&env, here, &["forget"]);
        let left = (sockets(&env), lock_file(&env, "state").exists(), env.state_file().exists());
        assert_eq!(left, (Vec::new(), false, false));
    }
}

/// Started at the same instant from two environments with different runtime directories, a
/// session ends with one socket, and one of the two starts is refused: a few rounds of what the
/// lock decides, which the unit tests of the lock show step by step.
#[test]
fn a_session_started_at_once_from_two_runtime_directories_has_one_server() {
    let env = Env::new("atonce");
    let _other = StopsOther(&env);
    let _evidence = Evidence(&env);
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    for round in 0..3 {
        let together = std::sync::Barrier::new(2);
        let started: Vec<Output> = std::thread::scope(|scope| {
            let starts: Vec<_> = [false, true]
                .into_iter()
                .map(|other| {
                    let (env, together) = (&env, &together);
                    scope.spawn(move || {
                        together.wait();
                        bounded(env, other, &["open", "w"])
                    })
                })
                .collect();
            starts.into_iter().map(|start| start.join().unwrap()).collect()
        });
        let at = sockets(&env);
        assert_eq!(at.len(), 1, "round {round}: {at:?}");
        let winner = at[0] == "run2";
        assert!(started[usize::from(winner)].status.success(), "round {round}");
        let loser = &started[usize::from(!winner)];
        let said = String::from_utf8_lossy(&loser.stderr);
        let told = said.contains("is not started here") || said.contains("no server of it listens");
        assert!(!loser.status.success() && told, "round {round}: {said}");
        assert!(in_environment(&env, winner, &["panes"]).contains("one"), "round {round}");
        in_environment(&env, winner, &["kill"]);
        assert!(!lock_file(&env, "state").exists(), "round {round}");
        in_environment(&env, !winner, &["forget"]);
    }
}

/// A server killed outright leaves its lock files, what it said of itself and its socket:
/// nothing of that keeps the session from starting again, from either environment.
#[test]
fn a_killed_server_leaves_nothing_that_blocks_the_next_start() {
    let env = Env::new("killedheld");
    let _other = StopsOther(&env);
    let _evidence = Evidence(&env);
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    for next in [false, true] {
        in_environment(&env, false, &["open", "w"]);
        let pid = env.server_pid().expect("the server runs");
        let socket = env.root.join("run/kraal/killedheld.sock");
        // SAFETY: plain syscall to the server this test started.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
        // Gone once nothing listens: a process being killed still does for a moment.
        wait_until(PATIENCE, "the killed server still listens", || {
            std::os::unix::net::UnixStream::connect(&socket).is_err()
        });
        assert!(lock_file(&env, "state").exists() && socket.exists(), "what a killed server leaves");
        let started = bounded(&env, next, &["open", "w"]);
        assert!(started.status.success(), "{}", String::from_utf8_lossy(&started.stderr));
        assert!(in_environment(&env, next, &["panes"]).contains("one"));
        in_environment(&env, next, &["kill"]);
        assert!(!lock_file(&env, "state").exists(), "the lock file a killed server left");
    }
}

/// Where the state directory cannot hold a session, none is started there and none forgotten:
/// the reason is said, with what to do, and what the session saved stays. Here the lock file
/// cannot be made; a file system that locks no file is what the unit tests put in its place.
#[test]
fn a_state_directory_that_cannot_hold_a_session_starts_none_and_forgets_none() {
    let env = Env::new("nostate");
    let _evidence = Evidence(&env);
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    env.run(&["open", "w"]);
    wait_until(PATIENCE, "the layout was never saved", || env.state_file().exists());
    env.run(&["kill"]);
    // No lock can be had there: a directory is where its file goes.
    let lock = env.root.join("state/kraal/nostate.lock");
    std::fs::create_dir(&lock).unwrap();
    for command in [&["open", "w"][..], &["forget"]] {
        let out = bounded(&env, false, command);
        let said = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && said.contains("no session can be held in"), "{command:?}: {said}");
        assert!(said.contains("XDG_STATE_HOME"), "{command:?}: {said}");
    }
    assert_eq!((env.server_pid(), sockets(&env)), (None, Vec::new()));
    assert!(env.state_file().exists(), "what it saved was deleted");
    std::fs::remove_dir(&lock).unwrap();
    env.run(&["open", "w"]);
    assert!(env.run(&["panes"]).contains("one"));
}

/// `$XDG_RUNTIME_DIR` set to a directory kraal cannot keep its sockets in is said with the
/// reason: by `kraal config check`, to who starts a server so, in that server's log, and to who
/// attaches to it. Not set, the sockets go where they go without a word.
#[test]
fn a_runtime_directory_kraal_cannot_use_is_said() {
    let env = Env::new("xdgno");
    let _evidence = Evidence(&env);
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    std::fs::create_dir_all(env.root.join("state/kraal")).unwrap();
    std::fs::write(env.root.join("state/kraal/welcomed"), "").unwrap();
    // Too long for a socket path. The sockets then go under the temporary directory: this
    // test's own.
    let (long, tmp) = (env.root.join("x".repeat(60)), env.root.join("t"));
    for dir in [&long, &tmp] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let tmp = tmp.canonicalize().unwrap();
    // SAFETY: getuid cannot fail.
    let sockets = tmp.join(format!("kraal-{}/kraal", unsafe { libc::getuid() }));
    let warning = format!(
        "XDG_RUNTIME_DIR ({}) is not used: it is {} bytes long, and a socket path leaves it 59. Sockets are in {}",
        long.display(),
        long.as_os_str().len(),
        sockets.display()
    );
    let kraal = |args: &[&str]| env.cmd(args).env("XDG_RUNTIME_DIR", &long).env("TMPDIR", &tmp).output().unwrap();
    let checked = kraal(&["config", "check"]);
    assert_eq!(String::from_utf8_lossy(&checked.stderr), format!("warning: {warning}\n"));
    let unset = env.cmd(&["config", "check"]).env_remove("XDG_RUNTIME_DIR").env("TMPDIR", &tmp).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&unset.stderr), "");
    let opened = kraal(&["open", "w"]);
    assert!(opened.status.success(), "{}", String::from_utf8_lossy(&opened.stderr));
    assert_eq!(String::from_utf8_lossy(&opened.stderr), format!("kraal: warning: {warning}\n"));
    let log = std::fs::read_to_string(env.root.join("state/kraal/server-xdgno.log")).unwrap();
    assert!(log.contains(&format!("kraal: warning: {warning}\n")), "{log}");
    // Said once, by who started the server.
    assert_eq!(String::from_utf8_lossy(&kraal(&["panes"]).stderr), "");
    let client = RawClient::attach_at(&sockets.join("xdgno.sock"), Some("w"), (120, 40), None);
    client.wait_for_screen("XDG_RUNTIME_DIR (");
    drop(client);
    assert!(kraal(&["kill"]).status.success());
}

/// A session is not forgotten while it is restarted: in the moment between its server and the
/// next, when nobody holds it, `kraal forget` is refused, and the session comes back with what
/// it saved. One restart at a time.
#[test]
fn a_session_is_not_forgotten_while_it_restarts() {
    use std::os::fd::AsRawFd;
    let env = Env::new("restartgap");
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    env.run(&["open", "w"]);
    env.run(&["run", "--name", "kept", "--", "sh"]);
    wait_until(PATIENCE, "the layout was never saved", || env.state_file().exists());
    // As a restart keeps it: `forget` deletes nothing, another restart stops nothing.
    let kept = std::fs::File::create(env.root.join("state/kraal/restartgap.restart")).unwrap();
    // SAFETY: plain syscall on a file this test holds open.
    assert_eq!(unsafe { libc::flock(kept.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) }, 0);
    let pid = env.server_pid();
    let restart = bounded(&env, false, &["restart", "--yes"]);
    let said = String::from_utf8_lossy(&restart.stderr);
    assert!(!restart.status.success() && said.contains("another kraal is restarting session restartgap"), "{said}");
    assert_eq!(env.server_pid(), pid, "the session was stopped");
    env.run(&["kill"]);
    let forget = bounded(&env, false, &["forget"]);
    let said = String::from_utf8_lossy(&forget.stderr);
    assert!(!forget.status.success() && said.contains("another kraal is restarting session restartgap"), "{said}");
    assert!(env.state_file().exists(), "forgotten all the same");
    drop(kept);
    env.run(&["open", "w"]);
    // Restarted again and again while `forget` keeps coming: it comes back whole each time.
    /// Says so when it goes, however this test ends: `forget` then stops coming.
    struct Done<'a>(&'a std::sync::atomic::AtomicBool);
    impl Drop for Done<'_> {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let done = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            while !done.load(std::sync::atomic::Ordering::SeqCst) {
                let forget = bounded(&env, false, &["forget"]);
                assert!(!forget.status.success(), "a running session was forgotten");
            }
        });
        let _done = Done(&done);
        for round in 0..8 {
            let restarted = bounded(&env, false, &["restart", "--yes"]);
            let panes = String::from_utf8_lossy(&bounded(&env, false, &["panes"]).stdout).into_owned();
            let whole = restarted.status.success() && panes.contains("one") && panes.contains("kept");
            assert!(whole, "round {round}: {}\n{panes}", String::from_utf8_lossy(&restarted.stderr));
        }
    });
    assert!(lock_file(&env, "state").exists(), "its server holds it");
    assert!(!env.root.join("state/kraal/restartgap.restart").exists(), "what kept it for its restart was left");
}

/// A restart keeps the session from `kraal forget` until the server it started has it, whatever
/// becomes of the kraal that restarts: killed, or done waiting, while that server is still on
/// its way. That server alone has what kept it: once it has the session, nothing else does.
#[test]
fn a_restart_keeps_the_session_until_its_server_has_it() {
    use std::process::Stdio;
    let env = Env::new("restartkept");
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    env.run(&["open", "w"]);
    env.run(&["run", "--name", "kept", "--", "sh"]);
    wait_until(PATIENCE, "the layout was never saved", || env.state_file().exists());
    // While this file is there, a server started for the session waits before it takes it.
    let hold = env.root.join("hold");
    /// Lets that server go and stops it when the test ends, however it ends: one still held
    /// back would start once the test's directory, and this file with it, is gone.
    struct LetGo<'a>(&'a Env, PathBuf);
    impl Drop for LetGo<'_> {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.1);
            let start = Instant::now();
            while self.0.server_pid().is_some() && start.elapsed() < Duration::from_secs(10) {
                let _ = self.0.try_run(&["kill"]);
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    let _let_go = LetGo(&env, hold.clone());
    let reservation = env.root.join("state/kraal/restartkept.restart");
    for killed in [true, false] {
        std::fs::write(&hold, "").unwrap();
        let old = env.server_pid().expect("the server runs");
        let mut restart = env
            .cmd(&["restart", "--yes"])
            .env("KRAAL_TEST_HOLD_SERVER", &hold)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        // SAFETY: signal 0 only asks whether the server this test started is still there.
        wait_until(PATIENCE, "the next server was never started", || unsafe {
            libc::kill(old, 0) != 0 && env.server_pid().is_some()
        });
        if killed {
            restart.kill().unwrap();
            restart.wait().unwrap();
        } else {
            let out = finish(restart, PATIENCE);
            let said = String::from_utf8_lossy(&out.stderr);
            assert!(!out.status.success() && said.contains("did not start in time"), "{said}");
        }
        // The server on its way has no session yet, and still keeps it.
        let forget = bounded(&env, false, &["forget"]);
        let said = String::from_utf8_lossy(&forget.stderr);
        assert!(!forget.status.success() && said.contains("another kraal is restarting session"), "{killed}: {said}");
        assert!(env.state_file().exists() && reservation.exists(), "killed: {killed}");
        // Let go, it takes the session and restores it.
        std::fs::remove_file(&hold).unwrap();
        wait_until(PATIENCE, "the session never came back", || {
            String::from_utf8_lossy(&bounded(&env, false, &["panes"]).stdout).contains("kept")
        });
        assert!(!reservation.exists(), "what kept it for its restart was left");
        // What kept it is nobody's any more, a pane's least of all: `forget` gets as far as the
        // session, which its server has.
        let forget = bounded(&env, false, &["forget"]);
        let said = String::from_utf8_lossy(&forget.stderr);
        assert!(!forget.status.success() && said.contains("is running"), "killed: {killed}: {said}");
    }
}

/// A start that finds the session held by a kraal that does not listen yet (the one that won
/// when two starts came at the same moment, a kraal 0.3.0 that holds only its runtime
/// directory's lock) waits for it, and has a server once that one listens or is gone: it never
/// says that its own server exited. What is written of the holder decides nothing of that: a
/// kraal that has just taken the session and not yet said so, under what one killed in another
/// runtime directory left written, is waited for like any other. While the session stays held
/// and nothing listens, the start gives up after the time a start may take, and what is written
/// then tells why.
#[test]
fn a_start_waits_for_the_kraal_that_has_the_session() {
    let env = Env::new("heldstart");
    let _evidence = Evidence(&env);
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    let elsewhere = "{\"pid\":4242,\"host\":\"another-machine.invalid\",\"runtime\":\"/run/user/1/kraal\"}\n";
    let held_for = Duration::from_secs(1);
    for (place, said) in [("run", ""), ("state", ""), ("state", elsewhere)] {
        let lock = hold(&env, place, said);
        let start = Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(held_for);
                drop(lock);
            });
            let out = bounded(&env, false, &["open", "w"]);
            assert!(out.status.success(), "{place} {said:?}: {}", String::from_utf8_lossy(&out.stderr));
        });
        assert!(start.elapsed() >= held_for, "{place}: started while the session was held");
        assert!(env.run(&["panes"]).contains("one"));
        env.run(&["kill"]);
    }
    // Held for good, with no server: said, once a start has had its time.
    let by_another = "session heldstart is held (by another kraal) and no server of it listens";
    let on_another = "session heldstart is not started here: it runs on \"another-machine.invalid\" (kraal 4242)";
    // What is written may name anything where the holder's sockets are: a FIFO, which would
    // never open, keeps nobody waiting.
    let fifo = env.root.join("fifo");
    let name = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: a path that ends in a zero byte.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let at_a_fifo =
        format!("{{\"pid\":4242,\"host\":\"another-machine.invalid\",\"runtime\":{:?}}}\n", fifo.display().to_string());
    for (said, told) in [("", by_another), (elsewhere, on_another), (at_a_fifo.as_str(), on_another)] {
        let lock = hold(&env, "state", said);
        let start = Instant::now();
        let out = bounded(&env, false, &["open", "w"]);
        let answer = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && answer.contains(told) && !answer.contains("exited"), "{answer}");
        assert!(gave_up_in_time(start.elapsed()), "gave up after {:?}", start.elapsed());
        assert_eq!((env.server_pid(), sockets(&env)), (None, Vec::new()));
        drop(lock);
    }
    env.run(&["open", "w"]);
    assert!(env.run(&["panes"]).contains("one"));
}

/// A state directory and a runtime directory that are one directory, as given or through a
/// link, hold a session as two do: it starts at once, is not forgotten while it runs, and is
/// forgotten once stopped.
#[test]
fn a_state_and_a_runtime_directory_that_are_one_hold_a_session() {
    let env = Env::new("onedir");
    let _evidence = Evidence(&env);
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    let (both, alias) = (env.root.join("both"), env.root.join("alias"));
    std::fs::create_dir_all(&both).unwrap();
    std::os::unix::fs::symlink(&both, &alias).unwrap();
    for runtime in [&both, &alias] {
        let kraal = |args: &[&str]| {
            use std::process::Stdio;
            let mut c = env.cmd(args);
            c.env("XDG_STATE_HOME", &both)
                .env("XDG_RUNTIME_DIR", runtime)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            finish(c.spawn().unwrap(), PATIENCE)
        };
        let start = Instant::now();
        let opened = kraal(&["open", "w"]);
        assert!(opened.status.success(), "{}", String::from_utf8_lossy(&opened.stderr));
        assert!(start.elapsed() < Duration::from_secs(5), "started after {:?}", start.elapsed());
        assert!(String::from_utf8_lossy(&kraal(&["panes"]).stdout).contains("one"));
        let forget = kraal(&["forget"]);
        assert!(String::from_utf8_lossy(&forget.stderr).contains("is running"), "{forget:?}");
        assert!(kraal(&["kill"]).status.success());
        let forget = kraal(&["forget"]);
        assert!(forget.status.success(), "{}", String::from_utf8_lossy(&forget.stderr));
    }
}

/// Started many times at the same moment, a session has one server, and every start ends with
/// it: none fails because another one's server was first.
#[test]
fn a_session_started_many_times_at_once_starts_once_and_for_all() {
    let env = Env::new("startrace");
    let _evidence = Evidence(&env);
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    for round in 0..10 {
        let together = std::sync::Barrier::new(8);
        let starts: Vec<Output> = std::thread::scope(|scope| {
            let starts: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        together.wait();
                        bounded(&env, false, &["open", "w"])
                    })
                })
                .collect();
            starts.into_iter().map(|start| start.join().unwrap()).collect()
        });
        let all: Vec<String> = starts
            .iter()
            .map(|out| format!("{}: {}", out.status, String::from_utf8_lossy(&out.stderr).trim()))
            .collect();
        assert!(starts.iter().all(|out| out.status.success()), "round {round}: {all:#?}\n{}", evidence(&env));
        assert!(in_environment(&env, false, &["panes"]).contains("one"), "round {round}\n{}", evidence(&env));
        // One server: stopped, none is left.
        in_environment(&env, false, &["kill"]);
        let start = Instant::now();
        while env.server_pid().is_some() {
            assert!(start.elapsed() < PATIENCE, "round {round}: a second server was running\n{}", evidence(&env));
            std::thread::sleep(Duration::from_millis(50));
        }
        in_environment(&env, false, &["forget"]);
    }
}

/// A server keeps open nothing of what the kraal that started it was started with. A pipe that
/// kraal was given (by a script that reads what its commands say, a build tool, a test) ends
/// for whoever reads it when that kraal is gone, not when the session is.
#[test]
fn a_server_keeps_nothing_its_starter_was_given() {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    let env = Env::new("nofds");
    let _evidence = Evidence(&env);
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    let (mut read, given) = std::io::pipe().unwrap();
    let fd = given.as_raw_fd();
    let mut open = env.cmd(&["open", "w"]);
    // SAFETY: fcntl alone, which may run between fork and exec. The kraal started has the
    // writing end of the pipe as a descriptor nobody marked to be closed.
    unsafe {
        open.pre_exec(move || match libc::fcntl(fd, libc::F_SETFD, 0) {
            0 => Ok(()),
            _ => Err(std::io::Error::last_os_error()),
        })
    };
    let out = within(&env, &mut open, "kraal open w");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    drop(given);
    assert!(env.server_pid().is_some(), "no server runs");
    let (ended, end) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut rest = Vec::new();
        let _ = ended.send(read.read_to_end(&mut rest));
    });
    let ended = end.recv_timeout(Duration::from_secs(10));
    assert!(ended.is_ok(), "the server holds a pipe of whoever started the kraal that started it");
}

/// The lock file where a session saves goes with its holder, a server or `kraal forget`: a
/// session name that was used leaves nothing there, and the file a killed server left is the
/// next one's to remove. The one of the runtime directory stays, for kraal 0.3.0: it takes that
/// file without looking at it again, and would hold one that is gone.
#[test]
fn a_session_leaves_no_lock_file_where_it_saves() {
    let env = Env::new("nolock");
    let _evidence = Evidence(&env);
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    env.run(&["open", "w"]);
    assert!(lock_file(&env, "state").exists() && lock_file(&env, "run").exists(), "a server holds its session");
    // Open here until the end: a file made once it is gone could have its number.
    let runtime_lock = std::fs::File::open(lock_file(&env, "run")).unwrap();
    env.run(&["kill"]);
    assert!(!lock_file(&env, "state").exists(), "left by a server that stopped");
    env.run(&["forget"]);
    assert!(!lock_file(&env, "state").exists(), "left by kraal forget");
    // A server killed outright cannot remove its file: the next server takes it, and removes it.
    env.run(&["open", "w"]);
    let pid = env.server_pid().expect("the server runs");
    // SAFETY: plain syscall to the server this test started.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    // Gone once nothing listens: a process being killed still does for a moment.
    let socket = env.root.join("run/kraal/nolock.sock");
    wait_until(Duration::from_secs(10), "the killed server still listens", || {
        std::os::unix::net::UnixStream::connect(&socket).is_err()
    });
    assert!(lock_file(&env, "state").exists());
    env.run(&["open", "w"]);
    env.run(&["kill"]);
    assert!(!lock_file(&env, "state").exists(), "left by the server after a killed one");
    let number = |file: std::fs::Metadata| std::os::unix::fs::MetadataExt::ino(&file);
    let (then, now) = (runtime_lock.metadata().unwrap(), std::fs::metadata(lock_file(&env, "run")).unwrap());
    assert_eq!(number(now), number(then), "the runtime directory's lock file was replaced");
}

/// Servers started and stopped and the session forgotten, all at once and from many processes:
/// the session is never held by two of them, every command works or is refused for what it says,
/// and nothing is left of the lock once all is stopped.
#[test]
fn a_session_started_stopped_and_forgotten_at_once_is_held_by_one_at_a_time() {
    use std::process::Stdio;
    let env = Env::new("lockrace");
    let _evidence = Evidence(&env);
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    // Each server with the file it says in: no pipe, which a server started at the same moment
    // would keep open for as long as it runs.
    let servers = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for worker in 0..8u64 {
            let (env, servers) = (&env, &servers);
            scope.spawn(move || {
                // The same commands in the same order at each run, different for each worker.
                let mut random = worker + 1;
                for n in 0..30 {
                    random = random.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                    match (random >> 33) % 4 {
                        0 | 1 => {
                            let said = env.root.join(format!("server-{worker}-{n}.err"));
                            let err = std::fs::File::create(&said).unwrap();
                            let server = env.cmd(&["server"]).stdout(Stdio::null()).stderr(err).spawn();
                            servers.lock().unwrap().push((server.unwrap(), said));
                        }
                        2 => {
                            let out = bounded(env, false, &["kill"]);
                            let said = String::from_utf8_lossy(&out.stderr);
                            assert!(out.status.success() || said.contains("no kraal server"), "kill: {said}");
                        }
                        _ => {
                            let out = bounded(env, false, &["forget"]);
                            let said = String::from_utf8_lossy(&out.stderr);
                            assert!(out.status.success() || said.contains("is running"), "forget: {said}");
                        }
                    }
                }
            });
        }
    });
    // Each server ran until it was stopped, or was refused the session. One that had found
    // another one listening, or its socket taken, would have held the session with that one.
    let servers = servers.into_inner().unwrap();
    assert!(servers.len() > 100, "{} servers started", servers.len());
    let (mut ran, mut refused) = (0, 0);
    for (mut server, said) in servers {
        let start = Instant::now();
        let status = loop {
            if let Some(status) = server.try_wait().unwrap() {
                break status;
            }
            let _ = bounded(&env, false, &["kill"]);
            assert!(start.elapsed() < Duration::from_secs(30), "a server would not stop\n{}", evidence(&env));
        };
        let said = std::fs::read_to_string(said).unwrap();
        match status.success() {
            true => ran += 1,
            false => refused += 1,
        }
        assert_eq!(status.success(), said.is_empty(), "{said}");
        assert!(said.is_empty() || said.contains("holds session \"lockrace\""), "{said}");
    }
    assert!(ran > 0 && refused > 0, "{ran} servers ran, {refused} were refused");
    assert!(!lock_file(&env, "state").exists());
    // And the session is as any other.
    env.run(&["open", "w"]);
    assert!(env.run(&["panes"]).contains("one"));
    env.run(&["kill"]);
    env.run(&["forget"]);
    assert!(!lock_file(&env, "state").exists());
}

/// A workspace that cannot be restored (its shell is gone) stays saved as it was, scrollback
/// included, while the server saves the others; it comes back once it can.
#[test]
fn a_workspace_that_cannot_be_restored_stays_saved() {
    let env = Env::new("unrestored");
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(&bin.join("claude"), "#!/bin/sh\necho original-agent\nexec sleep 1000\n");
    let home = env.root.join("work-home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        env.root.join("config/kraal/accounts.toml"),
        format!("[[account]]\nname = 'work'\nagent = 'claude'\nhome = {:?}\n", home.to_str().unwrap()),
    )
    .unwrap();
    env.workspace("shells", "layout {\n    pane name=\"shell\"\n    pane name=\"agent\" agent=\"claude:work\"\n}\n");
    env.workspace("jobs", "layout {\n    pane name=\"job\" command=\"sh\" { args \"-c\" \"exec sleep 4246\"; }\n}\n");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let opened = env.cmd(&["open", "shells"]).env("PATH", &path).output().unwrap();
    assert!(opened.status.success(), "{}", String::from_utf8_lossy(&opened.stderr));
    env.wait_for_text("agent", "original-agent");
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let original_id = panes.as_array().unwrap().iter().find(|p| p["name"] == "agent").unwrap()["account_id"]
        .as_str()
        .unwrap()
        .to_string();
    env.run(&["open", "jobs"]);
    env.run(&["send", "-p", "shell", "--enter", "echo kept-$((4*4))"]);
    env.wait_for_text("shell", "kept-16");
    env.run(&["kill"]);
    // A released server saved only the name and launch home. The failing shell precedes
    // the agent, so binding only panes that successfully spawn would miss it.
    let mut state: serde_json::Value = serde_json::from_slice(&std::fs::read(env.state_file()).unwrap()).unwrap();
    let workspace = state["workspaces"].as_array_mut().unwrap().iter_mut().find(|ws| ws["name"] == "shells").unwrap();
    let spec = &mut workspace["tabs"][0]["root"]["Split"]["children"][1][0]["Pane"]["spec"];
    assert_eq!(spec["account_id"], original_id);
    spec.as_object_mut().unwrap().remove("account_id");
    spec["env"].as_array_mut().unwrap().retain(|pair| pair[0] != "KRAAL_ACCOUNT_ID");
    std::fs::write(env.state_file(), serde_json::to_vec(&state).unwrap()).unwrap();
    let names = || -> Vec<String> {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        panes.as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap_or_default().to_string()).collect()
    };
    // Without its shell, that workspace cannot come back; the other one does, and is saved.
    let shell = env.root.join("sh");
    let away = env.root.join("sh.away");
    std::fs::rename(&shell, &away).unwrap();
    env.run(&["open", "jobs"]);
    assert_eq!(names(), ["job"]);
    env.run(&["accounts", "rename", "claude:work", "renamed"]);
    env.run(&["config", "reload"]);
    env.run(&["accounts", "rm", "claude:renamed"]);
    env.run(&["accounts", "add", "claude", "renamed", "--home", home.to_str().unwrap(), "--no-login"]);
    env.run(&["config", "reload"]);
    env.run(&["kill"]);
    let state = std::fs::read_to_string(env.state_file()).unwrap();
    assert!(state.contains("\"shells\""), "the workspace was dropped: {state}");
    let saved: serde_json::Value = serde_json::from_str(&state).unwrap();
    let workspace = saved["workspaces"].as_array().unwrap().iter().find(|ws| ws["name"] == "shells").unwrap();
    let spec = &workspace["tabs"][0]["root"]["Split"]["children"][1][0]["Pane"]["spec"];
    assert_eq!(spec["account_id"], original_id);
    assert_eq!(spec["account"], "claude:renamed");
    std::fs::rename(&away, &shell).unwrap();
    env.run(&["open", "jobs"]);
    let mut back = names();
    back.sort();
    assert_eq!(back, ["agent", "job", "shell"]);
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let agent = panes.as_array().unwrap().iter().find(|p| p["name"] == "agent").unwrap();
    assert_eq!(agent["account_id"], original_id, "the replacement cannot capture the restored pane");
    assert_eq!(agent["account"], "claude:renamed");
    env.wait_for_text("shell", "kept-16");
}

/// A restart stops the session's programs only once the layout it brings back is saved: when it
/// cannot be, the session keeps running as it is, and the restart says why.
#[test]
fn a_restart_that_cannot_save_the_session_leaves_it_running() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new("unsavedrestart");
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    env.run(&["open", "w"]);
    wait_until(Duration::from_secs(10), "the layout was never saved", || env.state_file().exists());
    let pid = env.server_pid().expect("the server runs");
    let sessions = env.root.join("state/kraal/sessions-v2");
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o500)).unwrap();
    // A pane opened since the last save: the layout on disk is not the session's any more.
    env.run(&["run", "--name", "two", "--target", "one", "--", "sh"]);
    let out = env.try_run(&["restart", "--yes"]);
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o700)).unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("not restarting: the layout could not be saved"), "{err}");
    assert_eq!(env.server_pid(), Some(pid), "the session stopped");
    // Once it can be saved, the restart brings back the pane opened since.
    let said = env.run(&["restart", "--yes"]);
    assert!(said.contains("restarted session unsavedrestart"), "{said}");
    assert_ne!(env.server_pid(), Some(pid));
    assert!(env.run(&["panes"]).contains("two"));
}

/// `kraal kill` and `kraal restart` give up on a server that does not answer (stopped, here) in
/// a bounded time, instead of waiting for it for good.
#[test]
fn stopping_a_server_that_does_not_answer_gives_up_in_time() {
    let env = Env::new("frozen");
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    env.run(&["open", "w"]);
    let stopped = Stopped::stop(env.server_pid().expect("the server runs"));
    let quiet = |args: &[&str]| {
        let mut c = env.cmd(args);
        c.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped());
        c.spawn().unwrap()
    };
    let (kill, restart) = (quiet(&["kill"]), quiet(&["restart", "--yes"]));
    let outs = [finish(kill, Duration::from_secs(60)), finish(restart, Duration::from_secs(60))];
    drop(stopped);
    for out in outs {
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && err.contains("is still running after"), "{err}");
    }
}

#[test]
fn a_failed_start_leaves_no_empty_workspace() {
    let env = Env::new("emptyws");
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    // A server with no workspace yet, asked for an agent on an account that does not exist.
    let mut server = env.cmd(&["server"]).spawn().unwrap();
    let socket = env.root.join("run/kraal/emptyws.sock");
    let start = Instant::now();
    while std::os::unix::net::UnixStream::connect(&socket).is_err() {
        assert!(start.elapsed() < Duration::from_secs(5), "the server did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = env.cmd(&["agent", "claude", "--account", "nope"]).env("KRAAL_SOCKET", &socket).output().unwrap();
    assert!(!out.status.success());
    let list = env.run(&["workspaces", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&list).unwrap();
    assert!(v.as_array().unwrap().iter().all(|w| w["name"] != "main"), "{list}");
    env.run(&["open", "w"]);
    env.run(&["close", "one"]);
    let before = env.run(&["workspaces", "--json"]);
    let out = env.cmd(&["agent", "claude", "--account", "nope"]).env("KRAAL_SOCKET", &socket).output().unwrap();
    assert!(!out.status.success());
    assert_eq!(env.run(&["workspaces", "--json"]), before, "an existing empty workspace stays open");
    env.run(&["kill"]);
    let _ = server.wait();
}

#[test]
fn scrollback_survives_a_server_restart() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new("scrollback");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    // More lines than the screen holds, then a coloured line with wide characters.
    env.run(&["send", "-p", "shell", "--enter", "seq 1 300; printf '\\033[31m%s\\033[0m 日本\\n' red-$((6*7))"]);
    env.wait_for_text("shell", "red-42 日本");
    env.run(&["kill"]);
    // Saved with its colours, for the user's eyes only.
    let dir = env.root.join("state/kraal/scrollback-v2/scrollback");
    let files: Vec<PathBuf> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
    assert_eq!(files.len(), 1, "{files:?}");
    let saved = std::fs::read_to_string(&files[0]).unwrap();
    assert!(saved.contains("\x1b[0;31mred-42\x1b[0m 日本"), "{saved:?}");
    assert_eq!(std::fs::metadata(&files[0]).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    // Back after the restart, history included, with what the new shell writes below a marker.
    env.run(&["open", "w"]);
    let screen = env.run(&["read", "-p", "shell"]);
    assert!(screen.contains("red-42 日本") && screen.contains("kraal: restored from"), "{screen}");
    let all = env.run(&["read", "-p", "shell", "--lines", "1000"]);
    assert!(all.lines().any(|l| l == "1"), "{all}");
    env.run(&["send", "-p", "shell", "--enter", "echo after-$((1+1))"]);
    env.wait_for_text_since_restore("shell", "after-2");
    // Restarted again: both restores show, in order.
    env.run(&["kill"]);
    env.run(&["open", "w"]);
    let all = env.run(&["read", "-p", "shell", "--lines", "1000"]);
    let at = |text: &str, from: usize| match all[from..].find(text) {
        Some(i) => from + i,
        None => panic!("no {text:?} after byte {from} of:\n{all}"),
    };
    let first = at("kraal: restored from", at("red-42 日本", at("300", 0)));
    at("kraal: restored from", at("after-2", first));
    // `kraal forget` deletes it with the layout.
    env.run(&["kill"]);
    env.run(&["forget"]);
    assert!(!dir.exists() && !env.state_file().exists());
    // Turned off, what was saved goes, and nothing is saved any more.
    env.run(&["open", "w"]);
    env.run(&["send", "-p", "shell", "--enter", "echo secret-$((2+2))"]);
    env.wait_for_text("shell", "secret-4");
    env.run(&["kill"]);
    assert!(dir.exists());
    let config = env.root.join("config/kraal/config.toml");
    let base = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{base}save_scrollback = false\n")).unwrap();
    env.run(&["open", "w"]);
    let screen = env.run(&["read", "-p", "shell"]);
    assert!(!screen.contains("secret-4") && !screen.contains("restored from"), "{screen}");
    env.run(&["kill"]);
    assert!(!dir.exists());
}

/// A pane whose scrollback could not be written is saved again once it can be, though it
/// printed nothing since; `kraal kill` says when it still cannot be.
#[test]
fn scrollback_that_could_not_be_written_is_saved_later() {
    let env = Env::new("scrollfail");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    // A file where the session's scrollback directory goes: nothing can be written there.
    let dir = env.root.join("state/kraal/scrollback-v2/scrollfail");
    std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
    std::fs::write(&dir, "in the way").unwrap();
    env.run(&["open", "w"]);
    env.run(&["send", "-p", "shell", "--enter", "echo once-$((5*5))"]);
    env.wait_for_text("shell", "once-25");
    // Long enough for a save of everything it printed to be tried, and to fail.
    std::thread::sleep(Duration::from_secs(11));
    assert!(std::fs::read_to_string(env.state_file()).unwrap().contains(".ans\""), "the state names its file");
    std::fs::remove_file(&dir).unwrap();
    env.run(&["kill"]);
    env.run(&["open", "w"]);
    let screen = env.run(&["read", "-p", "shell"]);
    assert!(screen.contains("once-25") && screen.contains("kraal: restored from"), "{screen}");
    // Still impossible when the server stops: said.
    std::fs::rename(&dir, env.root.join("aside")).unwrap();
    std::fs::write(&dir, "in the way").unwrap();
    env.run(&["send", "-p", "shell", "--enter", "echo twice-$((6*6))"]);
    env.wait_for_text_since_restore("shell", "twice-36");
    let out = env.try_run(&["kill"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("the scrollback of a pane could not be saved"), "{err}");
}

/// A server that dies without stopping (a crash) saved what its panes showed shortly before.
#[cfg(target_os = "linux")]
#[test]
fn scrollback_survives_a_crash() {
    let env = Env::new("crash");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    env.run(&["send", "-p", "shell", "--enter", "echo before-$((3*3))"]);
    env.wait_for_text("shell", "before-9");
    // Written while the pane prints, then named by the state.
    let dir = env.root.join("state/kraal/scrollback-v2/crash");
    let saved = || {
        let files = std::fs::read_dir(&dir).into_iter().flatten().flatten();
        files.filter(|e| std::fs::read_to_string(e.path()).unwrap_or_default().contains("before-9")).count() == 1
    };
    let named = || std::fs::read_to_string(env.state_file()).unwrap_or_default().contains(".ans\"");
    let start = Instant::now();
    while !(saved() && named()) {
        assert!(start.elapsed() < Duration::from_secs(10), "the scrollback was not saved while the server ran");
        std::thread::sleep(Duration::from_millis(50));
    }
    let pid = env.server_pid().expect("the server's pid");
    // SAFETY: plain syscall.
    unsafe { libc::kill(pid, libc::SIGKILL) };
    wait_gone(&PathBuf::from(format!("/proc/{pid}")));
    env.run(&["open", "w"]);
    let screen = env.run(&["read", "-p", "shell"]);
    assert!(screen.contains("before-9") && screen.contains("kraal: restored from"), "{screen}");
}

#[cfg(unix)]
#[test]
fn refuses_a_runtime_dir_that_is_a_symlink() {
    let env = Env::new("symlinkrun");
    let elsewhere = env.root.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let run = env.root.join("run");
    std::fs::remove_dir_all(&run).unwrap();
    std::fs::create_dir_all(&run).unwrap();
    std::os::unix::fs::symlink(&elsewhere, run.join("kraal")).unwrap();
    let out = env.try_run(&["open", "w"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("refusing"), "{}", String::from_utf8_lossy(&out.stderr));
}

/// An idle server sleeps: no thread of it wakes up again and again while nothing happens, not
/// even to wait for the reset of a usage window.
#[cfg(target_os = "linux")]
#[test]
fn an_idle_server_sleeps() {
    let env = Env::new("sleeps");
    std::fs::write(
        env.root.join("config/kraal/accounts.toml"),
        "[[account]]\nid = 'work'\nname = 'work'\nagent = 'claude'\nhome = '~/work'\n",
    )
    .unwrap();
    // In a directory where nothing happens (the home directory may be busy: its changes are
    // looked at).
    let quiet = env.root.join("quiet");
    std::fs::create_dir_all(&quiet).unwrap();
    env.workspace(
        "w",
        &format!(
            "layout {{\n    pane name=\"a\" cwd={0:?}\n    pane name=\"b\" cwd={0:?}\n}}\n",
            quiet.display().to_string()
        ),
    );
    // An account's usage window that resets in an hour: the server wakes for it then, once.
    let usage = env.root.join("state/kraal/usage/by-id");
    std::fs::create_dir_all(&usage).unwrap();
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let seen = serde_json::json!({
        "five_hour": { "used_percent": 60, "resets_at": now + 3600 },
        "observed_at": now,
        "source": "statusline",
    });
    std::fs::write(usage.join("claude-work.json"), seen.to_string()).unwrap();
    env.run(&["open", "w"]);
    let client = RawClient::attach(&env, "w");
    env.wait_for_text("a", "$");
    env.wait_for_text("b", "$");
    client.wait_for_rows("the account's usage", |rows| {
        rows.windows(2).any(|r| {
            r[0].contains("Claude · work") && r[1].split('│').nth(1).is_some_and(|detail| detail.trim() == "60% used")
        })
    });
    let pid = env.server_pid().expect("the server's pid");
    // The times each of its threads went to sleep (not those another program took the processor
    // from them while they ran: that depends on the machine's load).
    let sleeps = || -> std::collections::HashMap<std::ffi::OsString, u64> {
        let tasks = std::fs::read_dir(format!("/proc/{pid}/task")).unwrap();
        tasks
            .flatten()
            .filter_map(|t| {
                let status = std::fs::read_to_string(t.path().join("status")).ok()?;
                let line = status.lines().find(|l| l.starts_with("voluntary_ctxt_switches"))?;
                Some((t.file_name(), line.split_whitespace().last()?.parse().ok()?))
            })
            .collect()
    };
    // Past the requests above, and what they started.
    std::thread::sleep(Duration::from_secs(4));
    let start = Instant::now();
    let woke: u64 = loop {
        let before = sleeps();
        std::thread::sleep(Duration::from_secs(6));
        let after = sleeps();
        // A thread that ended meanwhile was still finishing what those requests started, as on
        // a loaded machine: the server was not idle yet, and is looked at again.
        if before.keys().all(|thread| after.contains_key(thread)) {
            break after.iter().map(|(thread, now)| now - before.get(thread).copied().unwrap_or(0)).sum();
        }
        assert!(start.elapsed() < Duration::from_secs(60), "the server's threads keep ending: {after:?}");
    };
    // At most its periodic work, and what that starts: not the readers of the panes, nor
    // anything else.
    assert!(woke <= 6, "{woke} wake-ups in 6 s");
    // Still there when something happens.
    env.run(&["send", "-p", "a", "-e", "echo still-awake"]);
    env.wait_for_text("a", "still-awake");
    drop(client);
}

/// `ctrl+b q` asks first, Cancel selected, saying what stops (a short question when nothing
/// would); a yes saves the layout and stops the session, every attached client is told how it
/// comes back, and the next start brings the layout back.
#[test]
fn the_menu_quits_after_asking_and_keeps_the_layout() {
    let env = Env::new("menuquit");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    let pid = env.server_pid().expect("the server runs");
    let mut client = RawClient::attach(&env, "w");
    let other = RawClient::attach(&env, "w");
    client.wait_for_message("\"attached\"");
    other.wait_for_message("\"attached\"");
    client.send(b"\x02");
    client.wait_for_row("d detach · q quit");
    client.send(b"q");
    let rows = client.wait_for_row("Quit kraal? The layout is saved for next time.");
    assert!(!rows.iter().any(|r| r.contains("These programs stop")), "a shell at its prompt:\n{}", rows.join("\n"));
    client.send(b"\r");
    client.wait_for_rows("the question gone", |rows| !rows.iter().any(|r| r.contains("Quit kraal?")));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(env.server_pid(), Some(pid), "Enter takes Cancel, selected first");

    env.run(&["run", "--name", "job", "--", "sh", "-c", "echo started; exec sleep 4249"]);
    env.wait_for_text("job", "started");
    client.send(b"\x02q");
    client.wait_for_row("Quit kraal? These programs stop:");
    let rows = client.wait_for_row("The layout is saved for next time.");
    // The line as `kraal restart` prints it, spacing included.
    let listed = rows.iter().any(|r| r.contains("  sleep 4249  (pane ") && r.contains("\"job\" in w)"));
    assert!(listed, "{}", rows.join("\n"));
    client.send(b"y");
    let said = "stopped; kraal -s menuquit starts it again with its layout";
    client.wait_for_message(said);
    other.wait_for_message(said);
    wait_until(Duration::from_secs(10), "the server still runs", || env.server_pid().is_none());
    env.run(&["open", "w"]);
    let panes = env.run(&["panes"]);
    assert!(panes.contains("shell") && panes.contains("job"), "{panes}");
}

/// A quit stops the session's programs only once its layout is saved: when it cannot be, the
/// session keeps running, and the quit says why.
#[test]
fn a_quit_that_cannot_save_the_session_leaves_it_running() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new("unsavedquit");
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    env.run(&["open", "w"]);
    wait_until(Duration::from_secs(10), "the layout was never saved", || env.state_file().exists());
    let pid = env.server_pid().expect("the server runs");
    let sessions = env.root.join("state/kraal/sessions-v2");
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o500)).unwrap();
    // A pane opened since the last save: the layout on disk is not the session's any more.
    env.run(&["run", "--name", "two", "--target", "one", "--", "sh"]);
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_message("\"attached\"");
    client.send(b"\x02q");
    client.wait_for_row("Quit kraal?");
    client.send(b"y");
    client.wait_for_rows("the question gone", |rows| !rows.iter().any(|r| r.contains("Quit kraal?")));
    client.wait_for_rows("the Quit failure on the notice line", |rows| {
        rows[0].contains("not quitting: the layout could not be saved")
    });
    client.send(b"\x02Vj");
    client.wait_for_row("not quitting: the layout could not be saved");
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(env.server_pid(), Some(pid), "the session stopped");
    assert!(!client.messages.lock().unwrap().iter().any(|m| m.contains(r#""type":"exit""#)));
}

/// A command pane stays open when its program ends, to show how it ended: it has not ended, and
/// closing it leaves the workspace empty and the session running.
#[test]
fn a_command_pane_left_open_keeps_its_session() {
    let env = Env::new("keptopen");
    env.workspace("w", "layout {\n    pane name=\"job\" command=\"sh\" { args \"-c\" \"sleep 0.3; exit 3\"; }\n}\n");
    env.run(&["open", "w"]);
    let pid = env.server_pid().expect("the server runs");
    let client = RawClient::attach(&env, "w");
    client.wait_for_message("\"attached\"");
    env.run(&["wait", "-p", "job", "--until", "exit", "--timeout", "10"]);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(env.server_pid(), Some(pid), "the session ended");
    assert!(!client.messages.lock().unwrap().iter().any(|m| m.contains(r#""type":"exit""#)));
    env.run(&["close", "job"]);
    client.wait_for_row("+ Add pane");
    assert_eq!(env.server_pid(), Some(pid));
}

/// `kraal restart` says what would stop before asking; a client attached in a terminal waits for
/// the next server and comes back to its workspace; an idle shell comes back; and a restart run
/// from a pane of the session itself outlives that pane.
#[cfg(target_os = "linux")]
#[test]
fn restart_keeps_the_session_and_brings_its_clients_back() {
    let env = Env::new("restart");
    env.workspace(
        "w",
        "layout {\n    pane split_direction=\"vertical\" {\n        pane name=\"idle\"\n        pane name=\"busy\"\n    }\n}\n",
    );
    env.run(&["open", "w"]);
    env.run(&["send", "-p", "idle", "--enter", "echo before-restart"]);
    env.wait_for_text("idle", "before-restart");
    env.run(&["send", "-p", "busy", "--enter", "sleep 4245"]);
    let sleeping = || {
        std::fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .any(|e| std::fs::read(e.path().join("cmdline")).is_ok_and(|c| c == b"sleep\x004245\x00"))
    };
    let start = Instant::now();
    while !sleeping() {
        assert!(start.elapsed() < Duration::from_secs(10), "sleep never started");
        std::thread::sleep(Duration::from_millis(50));
    }
    // A client in a 30-row terminal: once it is attached, the panes are shorter than that.
    let mut client = env.client();
    let attached = || {
        let start = Instant::now();
        loop {
            let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
            if panes.as_array().unwrap().iter().all(|p| p["rows"].as_u64().unwrap() < 30) {
                return;
            }
            assert!(start.elapsed() < Duration::from_secs(20), "no client attached: {panes}");
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    attached();
    let pid = env.server_pid().expect("the server runs");

    // Without a terminal to ask on: what stops is said, and nothing happens.
    let out = env.try_run(&["restart"]);
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "{said}");
    let listed = said.split_once("These programs stop:\n").map(|(_, rest)| rest).unwrap_or_default();
    assert!(listed.starts_with("  sleep 4245  (pane ") && listed.contains(" \"busy\" in w)"), "{said}");
    assert!(!said.contains("idle"), "a shell at its prompt runs nothing: {said}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("add --yes"));
    assert_eq!(env.server_pid(), Some(pid));
    // On a terminal: the list, then the question, whose Enter is no.
    let screen = in_terminal(env.cmd(&["restart"]), b"\n");
    let (listed, asked) = (screen.find("sleep 4245"), screen.find("Restart session restart? [y/N]"));
    assert!(listed.is_some() && asked.is_some() && listed < asked, "{screen}");
    assert!(screen.contains("cancelled"), "{screen}");
    assert_eq!(env.server_pid(), Some(pid));

    let out = env.run(&["restart", "--yes"]);
    assert!(out.contains("restarted session restart"), "{out}");
    let second = env.server_pid().expect("a server runs again");
    assert_ne!(second, pid);
    // The idle shell is back, with what it showed, and takes commands.
    env.wait_for_text("idle", "before-restart");
    env.run(&["send", "-p", "idle", "--enter", "echo after-restart"]);
    env.wait_for_text_since_restore("idle", "after-restart");
    // The client did not leave: it attached to the next server.
    assert!(client.try_wait().unwrap().is_none(), "the client left");
    attached();

    // From a pane of the session: the restart closes that pane, and happens all the same.
    let line = format!("'{}' restart --yes", env!("CARGO_BIN_EXE_kraal"));
    env.run(&["send", "-p", "idle", "--enter", &line]);
    let start = Instant::now();
    while env.server_pid().is_none_or(|p| p == second) {
        assert!(start.elapsed() < Duration::from_secs(20), "the session did not restart");
        std::thread::sleep(Duration::from_millis(50));
    }
    let start = Instant::now();
    while !env.try_run(&["read", "-p", "idle", "--lines", "200"]).stdout.windows(13).any(|w| w == b"after-restart") {
        assert!(start.elapsed() < Duration::from_secs(10), "the idle pane did not come back");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(client.try_wait().unwrap().is_none(), "the client left");
    attached();
    let _ = client.kill();
    let _ = client.wait();
}

/// A restart run in place of the shell of the session's last pane (`exec`): the command stays
/// until the restart has the session, which would otherwise end with that pane, its layout
/// deleted, and the session comes back.
#[cfg(target_os = "linux")]
#[test]
fn a_restart_from_the_last_pane_outlives_it() {
    let env = Env::new("lastpane");
    env.workspace("w", "layout {\n    pane name=\"only\"\n}\n");
    env.run(&["open", "w"]);
    let pidfile = env.root.join("pane.pid");
    env.run(&["send", "-p", "only", "--enter", &format!("echo $$ > '{}'", pidfile.display())]);
    let mut pane = PathBuf::new();
    wait_until(Duration::from_secs(10), "the shell never said its pid", || {
        let pid = std::fs::read_to_string(&pidfile).unwrap_or_default();
        pane = PathBuf::from(format!("/proc/{}", pid.trim()));
        pid.ends_with('\n')
    });
    let cmdline = |process: &Path| std::fs::read(process.join("cmdline")).unwrap_or_default();
    let kraal = env!("CARGO_BIN_EXE_kraal");
    let go = env.root.join("go");
    let line = format!("while [ ! -e '{}' ]; do sleep 0.1; done; exec '{kraal}' restart --yes", go.display());
    env.run(&["send", "-p", "only", "--enter", &line]);
    // With the server stopped, the restart waits for it, and nothing can end the command in the
    // pane meanwhile but the command itself.
    let server = env.server_pid().expect("the server runs");
    let stopped = Stopped::stop(server);
    std::fs::write(&go, "").unwrap();
    let helper = [kraal.as_bytes(), b"\0-s\0lastpane\0restart\0--yes\0"].concat();
    let started = || std::fs::read_dir("/proc").unwrap().flatten().any(|e| cmdline(&e.path()) == helper);
    wait_until(Duration::from_secs(20), "the restart never started", started);
    let command = [kraal.as_bytes(), b"\0restart\0--yes\0"].concat();
    for _ in 0..30 {
        assert_eq!(cmdline(&pane), command, "the command left the pane before the restart had the session");
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(stopped);
    wait_until(Duration::from_secs(30), "the session did not come back", || {
        env.server_pid().is_some_and(|p| p != server)
    });
    assert!(env.run(&["panes"]).contains("only"));
}

/// `kraal restart` run in a pane of the session says what stops as it does from anywhere else:
/// the jobs in the background of its own pane too. Only the command itself and what runs it (a
/// script) are not programs that stop, in the foreground or in the background.
#[cfg(target_os = "linux")]
#[test]
fn a_restart_from_a_pane_says_what_stops_in_that_pane_too() {
    let env = Env::new("ownjobs");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    let pid = env.server_pid().expect("the server runs");
    let running = |argv: &[u8]| {
        std::fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .any(|e| std::fs::read(e.path().join("cmdline")).is_ok_and(|c| c == argv))
    };
    env.run(&["send", "-p", "shell", "--enter", "sleep 4251 &"]);
    wait_until(Duration::from_secs(10), "the job never started", || running(b"sleep\x004251\x00"));
    // Without a terminal to ask on, the command says what stops and goes no further.
    let said = env.root.join("said");
    let restart = format!("'{}' restart </dev/null >'{}' 2>&1", env!("CARGO_BIN_EXE_kraal"), said.display());
    let script = env.root.join("restart.sh");
    write_script(&script, &format!("#!/bin/sh\n{restart}\n"));
    let stops = |typed: &str| -> Vec<String> {
        let _ = std::fs::remove_file(&said);
        env.run(&["send", "-p", "shell", "--enter", typed]);
        let mut text = String::new();
        wait_until(Duration::from_secs(10), "the restart did not stop at what it would stop", || {
            text = std::fs::read_to_string(&said).unwrap_or_default();
            text.contains("add --yes")
        });
        text.lines().filter_map(|line| line.strip_prefix("  ")).map(String::from).collect()
    };
    // The pane's shell may have its own standard input elsewhere while the command runs: dash
    // does that for a command given `< file`, and this line does it in any shell. The jobs are
    // those of the shell's terminal all the same.
    let elsewhere = |command: &str| format!("exec 3<&0 </dev/null; {command}; exec <&3 3<&-");
    let typed = [
        restart.clone(),
        format!("sh '{}'", script.display()),
        format!("{restart} &"),
        elsewhere(&format!("'{}' restart >'{}' 2>&1", env!("CARGO_BIN_EXE_kraal"), said.display())),
    ];
    for typed in typed {
        let stops = stops(&typed);
        assert_eq!(stops.len(), 1, "{typed}: {stops:?}");
        assert!(stops[0].starts_with("sleep 4251 (in the background)  (pane "), "{typed}: {stops:?}");
    }
    // The same list as from outside the session.
    let outside = || {
        let out = env.try_run(&["restart"]);
        assert!(!out.status.success());
        let said = String::from_utf8_lossy(&out.stdout).into_owned();
        said.lines().filter_map(|line| line.strip_prefix("  ")).map(String::from).collect::<Vec<_>>()
    };
    let stops = outside();
    assert_eq!(stops.len(), 1, "{stops:?}");
    assert!(stops[0].starts_with("sleep 4251 (in the background)  (pane "), "{stops:?}");
    // And from outside while that shell runs a program with its standard input elsewhere.
    env.run(&["send", "-p", "shell", "--enter", &elsewhere("sleep 4253")]);
    wait_until(Duration::from_secs(10), "the program never started", || running(b"sleep\x004253\x00"));
    let stops = outside();
    assert_eq!(stops.len(), 2, "{stops:?}");
    assert!(stops[0].starts_with("sleep 4253  (pane "), "{stops:?}");
    assert!(stops[1].starts_with("sleep 4251 (in the background)  (pane "), "{stops:?}");
    assert_eq!(env.server_pid(), Some(pid));
    assert!(running(b"sleep\x004251\x00"));
}

/// A job left in the background of a shell stops with a restart (the terminal hangs up): it is
/// said with what stops, and the restart needs a yes for it. What the shell runs off its
/// terminal (a prompt's daemon) is not the user's job.
#[cfg(target_os = "linux")]
#[test]
fn a_restart_says_it_stops_the_jobs_in_the_background() {
    let env = Env::new("bgjobs");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    let pid = env.server_pid().expect("the server runs");
    let jobs = "sleep 4247 & (exec sleep 4248 </dev/null >/dev/null 2>&1) &";
    env.run(&["send", "-p", "shell", "--enter", jobs]);
    let running = |argv: &[u8]| {
        std::fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .any(|e| std::fs::read(e.path().join("cmdline")).is_ok_and(|c| c == argv))
    };
    wait_until(Duration::from_secs(10), "the jobs never started", || {
        running(b"sleep\x004247\x00") && running(b"sleep\x004248\x00")
    });
    let out = env.try_run(&["restart"]);
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "{said}");
    assert!(said.contains("These programs stop:\n  sleep 4247 (in the background)  (pane "), "{said}");
    assert!(!said.contains("4248"), "{said}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("add --yes"));
    assert_eq!(env.server_pid(), Some(pid));
}
