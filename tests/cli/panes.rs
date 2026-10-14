use super::support::{PATIENCE, RawClient};
use crate::common::{Env, wait_until, write_script};
use std::path::Path;
use std::time::{Duration, Instant};

#[test]
fn run_send_read_and_close() {
    let env = Env::new("basic");
    env.workspace("w", "layout {\n    pane name=\"first\"\n}\n");
    env.run(&["open", "w"]);
    let id =
        env.run(&["run", "--name", "worker", "--target", "first", "--close-on-exit", "--", "sh"]).trim().to_string();
    env.run(&["send", "-p", &id, "--enter", "echo", "hello-$((40+2))"]);
    env.wait_for_text(&id, "hello-42");
    let panes = env.run(&["panes"]);
    assert!(panes.contains("worker") && panes.contains("first"), "{panes}");
    env.run(&["keys", "-p", &id, "ctrl+d"]);
    let start = Instant::now();
    while env.run(&["panes"]).contains("worker") {
        assert!(start.elapsed() < Duration::from_secs(5), "shell pane did not close on exit");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn command_panes_stay_after_exit_and_wait_reports_it() {
    let env = Env::new("exit");
    env.workspace("w", "layout {\n    pane name=\"job\" command=\"sh\" {\n        args \"-c\" \"echo working; sleep 0.3; exit 7\"\n    }\n}\n");
    env.run(&["open", "w"]);
    let out = env.run(&["wait", "-p", "job", "--until", "exit", "--timeout", "10"]);
    assert!(!out.is_empty());
    let json = env.run(&["panes", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v[0]["exit_code"], 7, "{json}");
    assert!(env.run(&["read", "-p", "job"]).contains("working"));
}

#[test]
fn a_silent_panes_final_burst_is_drawn_and_its_empty_end_is_seen() {
    let env = Env::new("finalburst");
    env.workspace("w", "layout { pane name=\"main\"; }\n");
    env.run(&["open", "w"]);
    let mut client = RawClient::attach(&env, "w");
    env.run(&["run", "--name", "burst", "--", "sh", "-c", "stty -echo; printf 'FINAL-BURST\\n'; read line"]);
    client.wait_for_row("FINAL-BURST");
    client.send(b"\r");
    env.run(&["wait", "-p", "burst", "--until", "exit", "--timeout", "10"]);
    let text = env.run(&["read", "-p", "burst"]);
    assert_eq!(text.matches("FINAL-BURST").count(), 1, "{text}");
}

#[test]
fn a_wait_for_exit_includes_every_line_of_the_final_output_once() {
    let env = Env::new("finaloutput");
    env.workspace("w", "layout { pane name=\"main\"; }\n");
    env.run(&["open", "w"]);
    let script =
        r#"awk 'BEGIN { for (i = 1; i <= 9000; i++) printf "%06d %023d\n", i, 0 }'; printf 'LAST-LINE\n'; exit 7"#;
    env.run(&["run", "--name", "burst", "--", "sh", "-c", script]);
    env.run(&["wait", "-p", "burst", "--until", "exit", "--timeout", "15"]);
    let text = env.run(&["read", "-p", "burst", "--lines", "9500"]);
    let numbers: Vec<u32> = text.lines().filter_map(|l| l.split_whitespace().next()?.parse().ok()).collect();
    assert_eq!(numbers, (1..=9000).collect::<Vec<_>>());
    assert_eq!(text.matches("LAST-LINE").count(), 1);
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let burst = panes.as_array().unwrap().iter().find(|p| p["name"] == "burst").unwrap();
    assert_eq!(burst["exit_code"], 7);
}

#[test]
fn pane_edit_confirms_a_restart_and_keeps_the_same_pane_and_source_file() {
    for size in [(80, 24), (124, 36)] {
        let env = Env::new("pane-edit-live");
        let starts = env.root.join("starts");
        let quoted = format!("'{}'", starts.display().to_string().replace('\'', "'\\''"));
        let record = format!("printf '%s\\n' \"$$\" >> {quoted}");
        let script = format!("{record}; printf 'original output\\n'; exec sleep 60");
        let layout = format!(
            "layout {{\n    pane name=\"tests\" command=\"sh\" {{\n        args \"-c\" {}\n    }}\n}}\n",
            serde_json::to_string(&script).unwrap()
        );
        env.workspace("edit", &layout);
        env.run(&["open", "edit"]);
        env.wait_for_text("tests", "original output");
        let pane = || {
            let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
            panes.as_array().unwrap().iter().find(|p| p["workspace"] == "edit").unwrap().clone()
        };
        let before = pane();
        let started = std::fs::read_to_string(&starts).unwrap();
        assert_eq!(started.lines().count(), 1);
        let mut client = RawClient::attach_sized(&env, Some("edit"), size).traced();
        client.wait_for_message("\"attached\"");
        client.send(b"\x02Pe");
        client.wait_for_row("Edit pane");
        client.send(b"\x15renamed\r");
        client.wait_for_row("Pane updated");
        assert_eq!(pane()["id"], before["id"]);
        assert_eq!(std::fs::read_to_string(&starts).unwrap(), started);
        assert_eq!(pane()["command"], before["command"]);
        client.send(b"\x02Pe");
        client.wait_for_row("Edit pane");
        client.send(b"\t\t\x15");
        client.paste(format!("{record}; printf 'replacement output\\n'; exec sleep 60").as_bytes());
        client.send(b"\r");
        client.wait_for_row("Apply command?");
        client.send(b"\r");
        client.wait_for_row("Edit pane");
        assert_eq!(std::fs::read_to_string(&starts).unwrap(), started);
        client.send(b"\r");
        client.wait_for_row("Apply command?");
        client.send(b"\t\r");
        env.wait_for_text("renamed", "replacement output");
        assert_eq!(pane()["id"], before["id"]);
        let replaced = std::fs::read_to_string(&starts).unwrap();
        assert_eq!(replaced.lines().count(), 2);
        assert_ne!(replaced.lines().last(), started.lines().next());
        assert!(env.run(&["read", "-p", "renamed"]).contains("original output"));
        assert_eq!(std::fs::read_to_string(env.root.join("config/kraal/workspaces/edit.kdl")).unwrap(), layout);
    }
}

#[test]
fn pane_edit_keeps_errors_and_restores_a_stopped_command_until_enter() {
    let env = Env::new("pane-edit-stopped");
    env.workspace("edit", "layout {\n    pane name=\"tests\" command=\"true\"\n}\n");
    env.run(&["open", "edit"]);
    wait_until(PATIENCE, "command did not end", || {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        panes.as_array().unwrap().iter().any(|p| p["name"] == "tests" && !p["exit_code"].is_null())
    });
    let folder = env.root.join("project folder");
    std::fs::create_dir(&folder).unwrap();
    let marker = folder.join("ran");
    let mut client = RawClient::attach_sized(&env, Some("edit"), (80, 24)).traced();
    client.wait_for_message("\"attached\"");
    client.send(b"\x02Pe");
    client.wait_for_row("Edit pane");
    client.send(b"\t\x15/nonexistent/kraal-pane-edit\r");
    client.wait_for_row("is not available.");
    client.send(b"\x15");
    client.paste(folder.to_str().unwrap().as_bytes());
    client.send(b"\t\x15");
    client.paste(b"pwd > ran; printf 'edited command ran\\n'");
    client.send(b"\r");
    client.wait_for_row("Ready to run");
    assert!(!marker.exists());
    drop(client);
    env.run(&["kill"]);
    env.run(&["open", "edit"]);
    let mut client = RawClient::attach_sized(&env, Some("edit"), (124, 36)).traced();
    client.wait_for_row("enter run");
    assert!(!marker.exists());
    client.send(b"\r");
    wait_until(PATIENCE, "edited command did not run", || {
        env.run(&["read", "-p", "tests"]).lines().any(|line| line.trim() == "edited command ran")
    });
    assert_eq!(std::fs::read_to_string(marker).unwrap().trim(), folder.canonicalize().unwrap().to_str().unwrap());
}

/// Panes stacked from the keys share one place and keep one size; the one the focus reaches
/// opens (a key, a click on its row); a pane comes out again; one key goes through the layouts;
/// stacks and the layout chosen survive a restart, and a saved workspace keeps its stacks.
#[test]
fn stacks_and_layouts_from_the_keys() {
    let env = Env::new("stacks");
    let config = env.root.join("config/kraal/config.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{text}[sidebar]\nvisible = false\n")).unwrap();
    env.workspace(
        "w",
        "layout {\n    pane split_direction=\"vertical\" {\n        pane name=\"alpha\"\n        pane name=\"bravo\"\n        pane name=\"charlie\"\n    }\n}\n",
    );
    env.run(&["open", "w"]);
    let panes = || -> Vec<serde_json::Value> {
        let v: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        v.as_array().unwrap().iter().filter(|p| p["workspace"] == "w").cloned().collect()
    };
    let size = |name: &str| -> (u64, u64) {
        let p = panes().into_iter().find(|p| p["name"] == name).unwrap();
        (p["cols"].as_u64().unwrap(), p["rows"].as_u64().unwrap())
    };
    let focused = || -> String {
        let p = panes().into_iter().find(|p| p["focused"] == true);
        p.and_then(|p| p["name"].as_str().map(String::from)).unwrap_or_default()
    };
    let until = |what: &str, done: &dyn Fn() -> bool| {
        let start = Instant::now();
        while !done() {
            assert!(start.elapsed() < Duration::from_secs(10), "{what}: {}", env.run(&["panes", "--json"]));
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let mut client = RawClient::attach_sized(&env, Some("w"), (124, 40));
    client.wait_for_message("\"attached\"");
    // 124×40 with a four-column strip: the panes take rows 1 to 38, between the bars, a third of the width each.
    until("three columns", &|| size("alpha") == (37, 36));
    env.run(&["focus", "bravo"]);

    // Bravo goes right, onto charlie: bravo open in two thirds of the width, charlie folded to
    // the row under it, of the same size.
    client.send(b"\x02Ps\x1b[C");
    until("stacked", &|| size("bravo") == (78, 35) && size("charlie") == (78, 35));
    assert_eq!(focused(), "bravo");
    // Down goes to the next pane of the stack, which opens.
    client.send(b"\x1b[1;3B");
    until("charlie focused", &|| focused() == "charlie");
    // Bravo is folded to the top row of the stack now: a click on it opens it.
    client.send(b"\x1b[<0;61;2M\x1b[<0;61;2m");
    until("bravo clicked", &|| focused() == "bravo");
    assert_eq!(size("charlie"), (78, 35), "opening a pane resizes nothing");
    // Out of the stack again: above charlie, each half of the height, frame on frame.
    client.send(b"\x02Pu");
    until("unstacked", &|| size("bravo") == (78, 17) && size("charlie") == (78, 17));
    assert_eq!(focused(), "bravo");

    // The layouts, in turn: even columns, even rows, the main pane left, the others stacked.
    client.send(b"\x02Pl");
    until("columns", &|| size("alpha") == (37, 36) && size("charlie") == (38, 36));
    client.send(b"\x02Pl");
    until("rows", &|| size("alpha").0 == 118 && size("charlie").0 == 118);
    client.send(b"\x02Pl");
    until("main left", &|| size("alpha") == (69, 36) && size("bravo").0 == 46);
    client.send(b"\x02Pl");
    until("main left, stacked", &|| size("bravo") == (46, 35) && size("charlie") == (46, 35));
    assert_eq!(focused(), "bravo");

    // A restart brings the stack and the layout back.
    drop(client);
    env.run(&["kill"]);
    let state = std::fs::read_to_string(env.state_file()).unwrap();
    assert!(state.contains("\"stacked\": true") && state.contains("\"preset\": \"main-left-stacked\""), "{state}");
    env.run(&["open", "w"]);
    until("restored", &|| panes().len() == 3);
    until("stacked again", &|| size("bravo") == (46, 35) && size("charlie") == (46, 35) && size("alpha") == (69, 36));
    // The layout is still the tab's: a pane that comes joins the stack.
    env.run(&["run", "-t", "alpha", "--name", "delta", "--", "sh"]);
    until("delta stacked", &|| panes().len() == 4 && size("delta") == (46, 34) && size("bravo") == (46, 34));

    // Saved, the stack is `stacked=true`, with the pane open `expanded=true`, and opens as one.
    env.run(&["export", env.root.join("config/kraal/workspaces/copy.kdl").to_str().unwrap(), "--workspace", "w"]);
    let saved = std::fs::read_to_string(env.root.join("config/kraal/workspaces/copy.kdl")).unwrap();
    assert!(saved.contains("pane stacked=true") && saved.contains("expanded=true"), "{saved}");
    env.run(&[
        "import",
        env.root.join("config/kraal/workspaces/copy.kdl").to_str().unwrap(),
        "--name",
        "copy",
        "--yes",
    ]);
    let copies = || -> Vec<(u64, u64)> {
        let v: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let mut sizes: Vec<(u64, u64)> = v
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["workspace"] == "copy")
            .map(|p| (p["cols"].as_u64().unwrap(), p["rows"].as_u64().unwrap()))
            .collect();
        sizes.sort();
        sizes
    };
    until("the copy opens stacked", &|| copies() == [(46, 34), (46, 34), (46, 34), (69, 36)]);
}

#[test]
fn a_stack_opens_on_its_expanded_pane_without_a_focus_given() {
    let env = Env::new("expanded");
    let stack = "pane stacked=true {\n        pane name=\"a\"\n        pane name=\"b\" expanded=true\n    }";
    env.workspace("w", &format!("layout {{\n    {stack}\n}}\n"));
    env.run(&["open", "w"]);
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let focused: Vec<&str> =
        panes.as_array().unwrap().iter().filter(|p| p["focused"] == true).filter_map(|p| p["name"].as_str()).collect();
    assert_eq!(focused, ["b"]);
}

#[test]
fn unknown_panes_and_workspaces_are_errors() {
    let env = Env::new("errors");
    env.workspace("w", "layout {\n    pane\n}\n");
    env.run(&["open", "w"]);
    let out = env.try_run(&["read", "-p", "nope"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no pane named"));
    let out = env.try_run(&["open", "missing"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("no workspace named"));
}

/// Outside a pane, a command that acts on a pane and is given none says how it takes one: the
/// option for those that take an option, the argument for the one that takes an argument.
#[test]
fn a_command_given_no_pane_says_how_it_takes_one() {
    let env = Env::new("whichpane");
    let said = |args: &[&str]| {
        let out = env.try_run(args);
        assert!(!out.status.success(), "{args:?}");
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    let with_option: [&[&str]; 6] =
        [&["send", "hi"], &["keys", "enter"], &["read"], &["wait"], &["rename", "x"], &["buffer", "paste"]];
    for args in with_option {
        assert!(said(args).contains("which pane? pass --pane"), "{args:?}: {}", said(args));
        // What the message names is taken: the command gets as far as looking for the session.
        let named = said(&[args, &["--pane", "1"][..]].concat());
        assert!(named.contains("no kraal server"), "{args:?}: {named}");
    }
    let close = said(&["close"]);
    assert!(close.contains("which pane? name it after `kraal close`") && !close.contains("--pane"), "{close}");
    assert!(said(&["close", "1"]).contains("no kraal server"));
}

#[test]
fn moves_never_lose_the_source() {
    let env = Env::new("move");
    let data = env.root.join("data");
    let out = env.root.join("out");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::create_dir_all(&out).unwrap();
    let report = data.join("report");
    std::fs::write(&report, "only copy").unwrap();
    std::os::unix::fs::symlink(&data, env.root.join("alias")).unwrap();
    let path = |p: &Path| p.to_str().unwrap().to_string();
    // Into its own directory, seen through a link: refused, nothing deleted.
    let same = env.try_run(&["cp", "--move", &path(&report), &path(&env.root.join("alias"))]);
    assert!(!same.status.success() && String::from_utf8_lossy(&same.stderr).contains("already in"));
    assert_eq!(std::fs::read_to_string(&report).unwrap(), "only copy");
    env.run(&["cp", "--move", &path(&report), &path(&out)]);
    assert_eq!(std::fs::read_to_string(out.join("report")).unwrap(), "only copy");
    assert!(!report.exists());
    // What must not be replaced stays, and so does the source.
    std::fs::write(data.join("b"), "mine").unwrap();
    std::fs::write(out.join("b"), "theirs").unwrap();
    let kept = env.try_run(&["cp", "--move", "--no-replace", &path(&data.join("b")), &path(&out)]);
    assert!(!kept.status.success());
    assert_eq!(std::fs::read_to_string(out.join("b")).unwrap(), "theirs");
    assert_eq!(std::fs::read_to_string(data.join("b")).unwrap(), "mine");
}

#[test]
fn missing_command_does_not_break_the_workspace() {
    let env = Env::new("missing");
    env.workspace(
        "w",
        "layout {\n    pane split_direction=\"vertical\" {\n        pane name=\"ok\"\n        pane name=\"bad\" command=\"kraal-test-no-such-program\"\n    }\n}\n",
    );
    env.run(&["open", "w"]);
    let json = env.run(&["panes", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let bad = v.as_array().unwrap().iter().find(|p| p["name"] == "bad").expect("the failing pane is kept");
    assert_eq!(bad["suspended"], true, "{json}");
    env.wait_for_text("bad", "cannot start");
    // The server is still alive and the workspace still usable.
    assert!(env.run(&["workspaces"]).contains("open"));
}

/// A program found but that cannot run (its interpreter is missing) is known only once the pane
/// started it: the pane then waits for Enter all the same, saying why, and the others run.
#[test]
fn a_program_that_cannot_run_after_all_waits_for_enter() {
    let env = Env::new("cantrun");
    let broken = env.root.join("broken");
    write_script(&broken, "#!/no/such/interpreter\n");
    env.workspace(
        "w",
        &format!(
            "layout {{\n    pane split_direction=\"vertical\" {{\n        pane name=\"ok\"\n        pane name=\"bad\" command={:?}\n    }}\n}}\n",
            broken.display().to_string()
        ),
    );
    env.run(&["open", "w"]);
    env.wait_for_text("bad", "cannot start");
    env.wait_for_text("bad", "No such file or directory");
    let start = Instant::now();
    loop {
        let v: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let bad = v.as_array().unwrap().iter().find(|p| p["name"] == "bad").cloned().expect("the pane is kept");
        if bad["suspended"] == true {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(5), "{bad}");
        std::thread::sleep(Duration::from_millis(50));
    }
    env.run(&["send", "-p", "ok", "-e", "echo still-here"]);
    env.wait_for_text("ok", "still-here");
    write_script(&broken, "#!/bin/sh\nprintf 'repaired-start\\n'\nexec sleep 1000\n");
    env.run(&["wait", "-p", "bad", "--until", "exit", "--timeout", "10"]);
    env.run(&["focus", "bad"]);
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_row("cannot start");
    client.send(b"\r");
    env.wait_for_text("bad", "repaired-start");
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let bad = panes.as_array().unwrap().iter().find(|p| p["name"] == "bad").unwrap();
    assert_eq!(bad["suspended"], false);
}

#[test]
fn restore_suspends_commands_but_reconnects_ssh() {
    let env = Env::new("restoresafe");
    // A fake `ssh` that just prints, to stand for a reconnecting SSH pane.
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let ssh = bin.join("ssh");
    write_script(&ssh, "#!/bin/sh\necho \"connected $*\"\nexec sleep 1000\n");
    env.workspace(
        "w",
        &format!(
            "layout {{\n    pane split_direction=\"vertical\" {{\n        pane name=\"srv\" command=\"{0}\" {{ args \"admin@db\"; }}\n        pane name=\"deploy\" command=\"sh\" {{ args \"-c\" \"echo DEPLOYED; exec sleep 1000\"; }}\n        pane name=\"remote\" command=\"{0}\" {{ args \"-t\" \"admin@db\" \"systemctl restart app\"; }}\n    }}\n}}\n",
            ssh.display()
        ),
    );
    env.run(&["open", "w"]);
    env.wait_for_text("deploy", "DEPLOYED");
    env.wait_for_text("srv", "connected admin@db");
    env.wait_for_text("remote", "connected");
    env.run(&["kill"]);
    env.run(&["open", "w"]);
    // What each pane printed before shows above the restore marker; what runs now writes below.
    env.wait_for_text_since_restore("srv", "connected admin@db");
    assert!(env.run(&["read", "-p", "deploy"]).contains("DEPLOYED"), "what it printed before shows");
    let screen = env.wait_for_text_since_restore("deploy", "restored session");
    assert!(!screen.lines().any(|l| l.trim() == "DEPLOYED"), "the command must not run again by itself:\n{screen}");
    // An ssh that runs a command on the server is a command too, not a shell to reconnect.
    let screen = env.wait_for_text_since_restore("remote", "restored session");
    assert!(!screen.contains("connected"), "the remote command must not run again by itself:\n{screen}");
}

/// A timed-out target is stopped to its last process, by its process group: a command started
/// again in its pane meanwhile is not touched, and closing its pane does not let what it left
/// behind escape.
#[cfg(target_os = "linux")]
#[test]
fn a_timed_out_target_is_stopped_but_not_what_replaced_it() {
    let env = Env::new("tostop");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    let alive = |pid: libc::pid_t| {
        // A zombie has no command line: it is gone for what matters here.
        std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| !c.is_empty())
    };
    let read_pid =
        |path: &Path| crate::common::read_complete_lines(path).and_then(|t| t.trim().parse::<libc::pid_t>().ok());
    let wait_until = |what: &str, mut ok: Box<dyn FnMut() -> bool + '_>| {
        let start = Instant::now();
        while !ok() {
            assert!(start.elapsed() < Duration::from_secs(20), "{what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let pane_of = |tab: &str| -> String {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let p = panes.as_array().unwrap().iter().find(|p| p["tab"] == tab).expect("the run's pane");
        p["id"].to_string()
    };
    let log_of = |name: &str| -> String {
        std::fs::read_dir(env.root.join("state/kraal/runs"))
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| p.to_string_lossy().contains(&format!("-{name}-")) && p.extension().is_some_and(|x| x == "log"))
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default()
    };
    // Each start leaves a process that ignores SIGTERM and SIGHUP in the group, and says its pid.
    let step = |pidfile: &Path| {
        format!(
            "timeout = \"1\"\nsteps = [\"(trap '' TERM HUP; exec sleep 60) & echo $! > {}; wait\"]\n",
            pidfile.display()
        )
    };

    std::fs::create_dir_all(env.root.join("config/kraal/runbooks")).unwrap();
    // Started again by hand while it was being stopped: the new command is not stopped. Enter
    // in an ended pane starts it again; it takes a client, in a terminal `script` provides.
    let mut client = env.client();
    let first = env.root.join("first.pid");
    std::fs::write(env.root.join("config/kraal/runbooks/again.toml"), step(&first)).unwrap();
    env.run(&["runbook", "run", "again", "--yes"]);
    wait_until("the timeout", Box::new(|| log_of("again").contains("over its timeout")));
    let old = read_pid(&first).expect("the first start's pid");
    let pane = pane_of("▶ again");
    wait_until(
        "the command ends on SIGTERM",
        Box::new(|| {
            let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
            let id: u64 = pane.parse().unwrap();
            panes.as_array().unwrap().iter().any(|p| p["id"].as_u64() == Some(id) && !p["exit_code"].is_null())
        }),
    );
    std::fs::remove_file(&first).unwrap();
    std::io::Write::write_all(client.stdin.as_mut().unwrap(), b"\r").unwrap();
    wait_until("the second start", Box::new(|| read_pid(&first).is_some()));
    let new = read_pid(&first).unwrap();
    assert_ne!(old, new);
    wait_until("the run ends", Box::new(|| log_of("again").contains("timed out, stopped")));
    assert!(!alive(old), "what the timed-out start left ({old}) still runs");
    assert!(alive(new), "the start made by hand ({new}) was stopped");
    // SAFETY: plain syscall on our own test process.
    unsafe { libc::kill(new, libc::SIGKILL) };
    let _ = client.kill();
    let _ = client.wait();

    // Its pane closed while it is being stopped: what it left is stopped all the same.
    let second = env.root.join("second.pid");
    std::fs::write(env.root.join("config/kraal/runbooks/closed.toml"), step(&second)).unwrap();
    env.run(&["runbook", "run", "closed", "--yes"]);
    wait_until("the timeout", Box::new(|| log_of("closed").contains("over its timeout")));
    let left = read_pid(&second).expect("the start's pid");
    env.run(&["close", &pane_of("▶ closed")]);
    wait_until("what it left is stopped", Box::new(|| !alive(left)));
}

/// What a timed-out target left behind is stopped even when kraal stops meanwhile (its last
/// pane closed during the grace period), and even when the server was held up past that period.
#[cfg(target_os = "linux")]
#[test]
fn a_timed_out_target_is_stopped_whatever_happens_to_kraal_meanwhile() {
    let alive = |pid: libc::pid_t| std::fs::read(format!("/proc/{pid}/cmdline")).is_ok_and(|c| !c.is_empty());
    let read_pid =
        |path: &Path| crate::common::read_complete_lines(path).and_then(|t| t.trim().parse::<libc::pid_t>().ok());
    let wait_until = |what: &str, limit: u64, mut ok: Box<dyn FnMut() -> bool + '_>| {
        let start = Instant::now();
        while !ok() {
            assert!(start.elapsed() < Duration::from_secs(limit), "{what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let log_of = |env: &Env, name: &str| -> String {
        std::fs::read_dir(env.root.join("state/kraal/runs"))
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| p.to_string_lossy().contains(&format!("-{name}-")) && p.extension().is_some_and(|x| x == "log"))
            .and_then(|p| std::fs::read_to_string(p).ok())
            .unwrap_or_default()
    };
    let start_run = |env: &Env, name: &str| -> libc::pid_t {
        env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
        env.run(&["open", "w"]);
        std::fs::create_dir_all(env.root.join("config/kraal/runbooks")).unwrap();
        let pidfile = env.root.join(format!("{name}.pid"));
        std::fs::write(
            env.root.join(format!("config/kraal/runbooks/{name}.toml")),
            format!(
                "timeout = \"1\"\nsteps = [\"(trap '' TERM HUP; exec sleep 60) & echo $! > {}; wait\"]\n",
                pidfile.display()
            ),
        )
        .unwrap();
        env.run(&["runbook", "run", name, "--yes"]);
        wait_until("the timeout", 20, Box::new(|| log_of(env, name).contains("over its timeout")));
        read_pid(&pidfile).expect("the pid of what the target left")
    };

    // Closing its last pane keeps the session while the timeout's grace period finishes.
    let env = Env::new("tolast");
    let left = start_run(&env, "last");
    env.run(&["close", "shell"]);
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let id = panes.as_array().unwrap()[0]["id"].to_string();
    let _ = env.try_run(&["close", &id]);
    assert!(env.server_pid().is_some());
    wait_until("what the target left is stopped", 20, Box::new(|| !alive(left)));

    // The server held up past the grace period: SIGKILL still goes before it gives up.
    let env = Env::new("toheld");
    let left = start_run(&env, "held");
    let server = env.server_pid().expect("the server");
    // SAFETY: plain syscalls on the test's own server.
    unsafe { libc::kill(server, libc::SIGSTOP) };
    std::thread::sleep(Duration::from_secs(16));
    unsafe { libc::kill(server, libc::SIGCONT) };
    wait_until("what the target left is stopped", 5, Box::new(|| !alive(left)));
    wait_until("the run ends", 20, Box::new(|| log_of(&env, "held").contains("timed out")));
    assert!(!log_of(&env, "held").contains("do not end"), "{}", log_of(&env, "held"));
    // SAFETY: as above.
    unsafe { libc::kill(left, libc::SIGKILL) };
}

#[test]
fn new_tab_puts_the_pane_in_a_new_tab() {
    let env = Env::new("newtab");
    env.workspace("w", "layout {\n    pane name=\"first\"\n}\n");
    env.run(&["open", "w"]);
    env.run(&["run", "--tab", "--target", "first", "--name", "second", "--", "sh"]);
    let json = env.run(&["panes", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let tabs: Vec<&str> = v.as_array().unwrap().iter().map(|p| p["tab"].as_str().unwrap()).collect();
    assert_eq!(tabs.len(), 2);
    assert_ne!(tabs[0], tabs[1], "the new pane must be alone in its new tab: {json}");
}

#[test]
fn closing_or_adding_tabs_keeps_the_one_shown() {
    let env = Env::new("tabs");
    env.workspace(
        "w",
        "layout {\n    tab name=\"a\" {\n        pane name=\"a\"\n    }\n    tab name=\"b\" {\n        pane name=\"b\"\n    }\n    \
         tab name=\"c\" {\n        pane name=\"c\"\n    }\n}\n",
    );
    env.run(&["open", "w"]);
    env.run(&["focus", "b"]);
    let focused = || -> String {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let shown = panes.as_array().unwrap().iter().find(|p| p["focused"] == true).cloned();
        shown.and_then(|p| p["name"].as_str().map(String::from)).unwrap_or_default()
    };
    assert_eq!(focused(), "b");
    env.run(&["close", "a"]);
    assert_eq!(focused(), "b", "closing an earlier tab keeps the one shown");
    env.run(&["run", "--tab", "--no-focus", "--", "sleep", "60"]);
    assert_eq!(focused(), "b", "a new tab that must not take the focus does not");
}

#[test]
fn an_exit_is_seen_even_when_something_left_behind_keeps_printing() {
    let env = Env::new("chatty");
    env.workspace("w", "layout {\n    pane name=\"first\"\n}\n");
    env.run(&["open", "w"]);
    // The command ends at once; what it left in the background, deaf to the hangup, prints on
    // for as long as the terminal lets it.
    env.run(&["run", "-n", "chatty", "--", "sh", "-c", "(trap '' HUP; exec yes kraal) & exit 3"]);
    let start = Instant::now();
    env.run(&["wait", "-p", "chatty", "--until", "exit", "--timeout", "15"]);
    assert!(start.elapsed() < Duration::from_secs(10), "the exit took {:?}", start.elapsed());
    let json = env.run(&["panes", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let chatty = v.as_array().unwrap().iter().find(|p| p["name"] == "chatty").cloned().unwrap();
    assert_eq!(chatty["exit_code"], 3, "{json}");
    // The server still answers, and closing the pane hangs up what was left.
    env.run(&["close", "chatty"]);
    assert!(env.run(&["panes"]).contains("first"));
}
