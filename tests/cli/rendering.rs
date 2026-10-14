use super::support::{PATIENCE, RawClient, emulate, raw_frame, says_its_size, terminal_after, text_after};
use crate::common::{Env, claude_hook, wait_until, write_script};
use std::time::{Duration, Instant};

/// `name.ext:123` reads as a file and its line, or as a host and its port. In a local pane the
/// file is looked for: a host and its port is offered to open, never as a file to edit, and a
/// file that is there opens in the editor at its line.
#[test]
fn hints_tell_a_host_and_its_port_from_a_file_and_its_line() {
    let env = Env::new("hostport");
    let dir = env.root.join("proj");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("notes.md"), "a file\n").unwrap();
    std::fs::write(dir.join("seen"), "redis.lan:6379\nnotes.md:12\n").unwrap();
    // An editor that says what it was given.
    let (editor, edited) = (env.root.join("editor"), env.root.join("edited"));
    write_script(&editor, &format!("#!/bin/sh\necho \"$@\" > {}\n", edited.display()));
    let config = env.root.join("config/kraal/config.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    // Labels in the order of the screen, from the bottom: x for the last line, y above it.
    std::fs::write(&config, format!("{text}[hints]\nalphabet = \"xy\"\n")).unwrap();
    env.workspace("w", &format!("layout {{\n    pane name=\"shell\" cwd={:?}\n}}\n", dir.display().to_string()));
    let out = env.cmd(&["open", "w"]).env("VISUAL", &editor).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    env.run(&["send", "-p", "shell", "--enter", "cat seen"]);
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_row("notes.md:12");
    let act = |client: &mut RawClient, label: &[u8]| {
        client.send(b"\x02o");
        client.wait_for_row("HINTS: COPY");
        client.send(b"\t");
        client.wait_for_row("HINTS: ACT");
        client.send(label);
    };

    act(&mut client, b"y");
    client.wait_for_message("http://redis.lan:6379");
    std::thread::sleep(Duration::from_millis(300));
    assert!(!edited.exists(), "an editor opened on {:?}", std::fs::read_to_string(&edited));
    act(&mut client, b"x");
    wait_until(Duration::from_secs(10), "the editor never opened", || {
        std::fs::read_to_string(&edited).is_ok_and(|args| args.trim() == "+12 notes.md")
    });
}

/// Frames are drawn over the last one, with the rows that did not change left out: whatever
/// happens, a client ends up showing what a client attaching then is shown whole.
#[test]
fn frames_drawn_over_the_last_one_end_as_a_whole_frame() {
    let env = Env::new("incremental");
    std::fs::create_dir_all(env.root.join("state/kraal")).unwrap();
    std::fs::write(env.root.join("state/kraal/welcomed"), "").unwrap();
    // In a directory that is no repository: what git says of one comes when git has answered,
    // which would change the frames by itself between two looks at them.
    let here = env.root.display().to_string();
    for (workspace, pane) in [("w", "main"), ("w2", "other"), ("w3", "third")] {
        env.workspace(workspace, &format!("layout {{\n    cwd {here:?}\n    pane name={pane:?}\n}}\n"));
    }
    let run = |name: &str, place: &str, script: &str| {
        env.run(&["run", "--name", name, place, "--cwd", &here, "--", "sh", "-c", script]);
    };
    let frames = env.open_noting_frames("w");
    let colors = "printf '\\033[1;31mred \\033[42;4mgreen\\033[0m \\346\\227\\245\\346\\234\\254 \
                  \\033]8;;https://example.org\\033\\\\link\\033]8;;\\033\\\\ e\\314\\201 done\\n'; exec cat";
    run("colors", "--right", colors);
    run("lines", "--down", "seq 1 200; exec cat");
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_screen("done");
    let same = |client: &RawClient, step: &str| {
        client.settle();
        // What it shows before another client attaches: that draws every client again, whole.
        let shown = client.screen.lock().unwrap().clone();
        let fresh = RawClient::attach(&env, "w");
        fresh.settle();
        let whole = fresh.screen.lock().unwrap().clone();
        let (a, b) = (terminal_after(&shown, 120, 40), terminal_after(&whole, 120, 40));
        for (y, (a, b)) in a.iter().zip(&b).enumerate() {
            assert_eq!(a, b, "{step}: row {y} differs");
        }
        drop(fresh);
        client.settle();
    };
    same(&client, "start");
    // Typing into a program: its echo, one cell at a time.
    env.run(&["focus", "colors"]);
    client.settle();
    let before = client.settle();
    client.send(b"x");
    // The frame with the echo is that character and the cursor, whatever else is drawn then.
    let start = Instant::now();
    let echo = loop {
        let after = String::from_utf8_lossy(&client.screen.lock().unwrap()[before..]).into_owned();
        if let Some(frame) = after.split_inclusive("\x1b[?2026l").find(|f| f.contains('x')) {
            break frame.to_string();
        }
        assert!(start.elapsed() < Duration::from_secs(10), "the echo never came: {after:?}");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(echo.len() < 100, "a frame for one character sent {} bytes: {echo:?}", echo.len());
    // Keys typed one after the other are echoed in frames made from what the pane printed alone
    // (but the first after the server's periodic work, made whole).
    let drawn = frames().len();
    let from_output = || frames()[drawn..].iter().filter(|f| f.ends_with("output")).count();
    let start = Instant::now();
    for b in b"yz 123".iter().cycle() {
        if from_output() >= 3 {
            break;
        }
        assert!(start.elapsed() < PATIENCE, "{:?}", &frames()[drawn..]);
        client.send(&[*b]);
        std::thread::sleep(Duration::from_millis(30));
    }
    same(&client, "typing");
    // Typing while another pane prints: frames made from what one pane printed, from what both
    // did, and one after the other.
    for i in 0..8 {
        client.send(b"k");
        env.run(&["send", "-p", "lines", &format!("between {i}\r")]);
    }
    client.send(b"last");
    same(&client, "typing while another pane prints");
    // What a pane prints changes what its frame says of it (a command that failed), with
    // nothing else happening: the keys only went to its shell.
    env.run(&["focus", "main"]);
    client.settle();
    client.send(b"printf '\\033]133;A\\007$ \\033]133;B\\007x\\n\\033]133;C\\007\\033]133;D;7\\007'\r");
    client.wait_for_row("\u{2717} 7");
    same(&client, "a command failed");
    env.run(&["focus", "colors"]);
    // Output scrolling one pane.
    env.run(&["send", "-p", "lines", "more\r"]);
    same(&client, "scrolling");
    // Image placeholders without an image draw as blanks.
    run("img", "--right", "printf 'img:\\364\\216\\273\\256 end\\n'; exec cat");
    same(&client, "placeholders");
    // A floating pane over the others, then gone.
    run("float", "--floating", "echo FLOAT; exec cat");
    client.wait_for_screen("FLOAT");
    same(&client, "floating");
    env.run(&["close", "float"]);
    same(&client, "floating closed");
    // Another tab, and back.
    run("tab2", "--tab", "echo TAB2; exec cat");
    client.wait_for_screen("TAB2");
    same(&client, "new tab");
    env.run(&["focus", "colors"]);
    same(&client, "tab back");
    // A pane that is not on the screen prints, then one that is.
    env.run(&["send", "-p", "tab2", "unseen\r"]);
    client.send(b"seen");
    same(&client, "a pane of another tab printed");
    // Colors stacked with the pane below it: lines folded to a row, renamed there, then open.
    client.send(b"\x02Ps\x1b[B");
    same(&client, "stacked");
    env.run(&["rename", "-p", "lines", "folded"]);
    same(&client, "folded pane renamed");
    client.send(b"\x1b[1;3B");
    same(&client, "other pane of the stack open");
    // The menu over the panes, then closed.
    client.send(b"\x02");
    client.settle();
    client.send(b"\x1b");
    same(&client, "menu closed");
    // A second workspace, then a third: the sidebar lists them as open.
    env.run(&["open", "w2"]);
    same(&client, "sidebar");
    env.run(&["open", "w3"]);
    same(&client, "sidebar changed");
    // A pane of another workspace prints, then one of this one: the sidebar shows both.
    env.run(&["send", "-p", "other", "elsewhere\r"]);
    client.send(b"here");
    same(&client, "a pane of another workspace printed");
    // A pane closed: the others take its place.
    env.run(&["close", "img"]);
    same(&client, "pane closed");
}

/// What a pane types by itself before the thing a test looks at: a character at a time, each in
/// a frame of its own, most of them made from what the pane printed alone.
const PRINTS_A_LITTLE: &str = "i=0; while [ $i -lt 20 ]; do printf t; sleep 0.02; i=$((i+1)); done";

/// The notes of the frames of a test with one client (see [`Env::open_noting_frames`]), once
/// the pane that printed a little did the thing looked at and nothing more: some were made from
/// what it printed alone, and the last one, which drew that thing, whole.
fn drawn_whole_after_output(frames: &[String]) {
    assert!(frames.iter().any(|f| f.ends_with("output")), "{frames:?}");
    assert!(frames.last().is_some_and(|f| f.ends_with("whole")), "{frames:?}");
}

/// A bell in a tab that is not shown marks that tab in the top bar. Nothing a frame keeps of the
/// pane says so (its name, what it works on): only that something other than output happened,
/// which is what makes the frame whole.
#[test]
fn a_bell_in_another_tab_is_drawn_by_a_whole_frame() {
    let env = Env::new("bell");
    // No welcome note: the top bar has room for both tabs.
    std::fs::create_dir_all(env.root.join("state/kraal")).unwrap();
    std::fs::write(env.root.join("state/kraal/welcomed"), "").unwrap();
    let tabs = "tab name=\"one\" focus=true {\n        pane name=\"front\"\n    }\n    \
                tab name=\"two\" {\n        pane name=\"back\"\n    }";
    env.workspace("w", &format!("layout {{\n    {tabs}\n}}\n"));
    let frames = env.open_noting_frames("w");
    let client = RawClient::attach(&env, "w");
    client.wait_for_row(" 2 two ");
    // Then nothing comes after the bell: its shell is busy.
    env.run(&["send", "-p", "back", "--enter", &format!("{PRINTS_A_LITTLE}; printf '\\a'; sleep 600")]);
    client.wait_for_row("two  !");
    client.settle();
    drawn_whole_after_output(&frames());
}

/// A command that starts takes the failure of the one before off the pane's frame. Nothing is
/// signalled then, as when a command ends: only what the frame keeps of the pane says it
/// changed, which is what makes the frame whole. (Without a sidebar: with one, the frame is
/// whole already for what the sidebar may say of the pane.)
#[test]
fn a_command_that_starts_is_drawn_by_a_whole_frame() {
    let env = Env::new("starts");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    let frames = env.open_noting_frames("w");
    let client = RawClient::attach_sized(&env, Some("w"), (39, 40));
    // What a shell that marks its commands prints around them: one that failed, then one that
    // starts and runs on.
    let failed = "printf '\\033]133;A\\007$ \\033]133;B\\007x\\n\\033]133;C\\007\\033]133;D;7\\007'";
    let starts = "printf '\\033]133;A\\007$ \\033]133;B\\007y\\n\\033]133;C\\007'";
    env.run(&["send", "-p", "shell", "--enter", failed]);
    client.wait_for_row("\u{2717} 7");
    env.run(&["send", "-p", "shell", "--enter", &format!("{PRINTS_A_LITTLE}; {starts}; sleep 600")]);
    client.wait_for_rows("the failure off the frame", |rows| !rows.iter().any(|r| r.contains("\u{2717} 7")));
    client.settle();
    drawn_whole_after_output(&frames());
}

/// The columns a pane's frame starts and ends at on the row under the top bar, where the sidebar
/// has no corner: those of the first pane there. A frame wider than what is left of the screen
/// is closed at the last column.
fn frame_columns(rows: &[String]) -> (Option<usize>, Option<usize>) {
    let at = |corner: char| rows[1].chars().position(|c| c == corner);
    (at('╭'), at('╮'))
}

#[test]
fn sidebar_strip_cells_clicks_and_pty_sizes() {
    for (cols, height, pane_cols, pane_rows) in [(124, 36, 118, 32), (80, 24, 74, 20), (60, 20, 54, 16)] {
        let env = Env::new(&format!("strip-{cols}"));
        let config = env.root.join("config/kraal/config.toml");
        let defaults = std::fs::read_to_string(&config).unwrap();
        std::fs::write(&config, format!("{defaults}\n[theme]\npreset = \"ascii\"\n")).unwrap();
        for (ws, pane) in [("api", "review"), ("docs", "writer")] {
            env.workspace(ws, &format!("layout {{\n    pane name=\"{pane}\"\n}}\n"));
            env.run(&["open", ws]);
            claude_hook(&env, pane, r#"{"hook_event_name":"SessionStart"}"#);
            claude_hook(&env, pane, r#"{"hook_event_name":"UserPromptSubmit"}"#);
        }
        claude_hook(&env, "review", r#"{"hook_event_name":"Notification","message":"Claude needs your permission"}"#);
        let mut client = RawClient::attach_sized(&env, Some("api"), (cols, height));
        if cols >= 84 {
            client.wait_for_row("WORKSPACES");
            client.send(b"\x02Vl");
        }
        let screen = client.wait_for_rows("both workspaces and agents in the strip", |rows| {
            ["WS | api ", " 1*|", " 2*|", "AG |", " 1!|", " 2*|"]
                .iter()
                .enumerate()
                .all(|(y, prefix)| rows[y].starts_with(prefix))
                && rows[height - 2].starts_with(" > |")
        });
        assert_eq!(screen[1].chars().nth(4), Some('+'));
        assert_eq!(screen[1].chars().last(), Some('+'));
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        for pane in panes.as_array().unwrap() {
            assert_eq!((pane["cols"].as_u64(), pane["rows"].as_u64()), (Some(pane_cols), Some(pane_rows)));
        }
        env.run(&["send", "-p", "review", "--enter", "printf 'strip-pty '; stty size"]);
        env.wait_for_text("review", &format!("strip-pty {pane_rows} {pane_cols}"));

        // Selecting a workspace or an agent keeps the strip, including its entire row's hit area.
        client.send(b"\x1b[<0;2;3M\x1b[<0;2;3m");
        client.wait_for_rows("docs selected from the strip", |rows| rows[0].starts_with("WS | docs "));
        client.send(b"\x1b[<0;3;5M\x1b[<0;3;5m");
        client.wait_for_rows("review revealed from the strip", |rows| rows[0].starts_with("WS | api "));

        // State changes of an agent off screen keep their exact symbols without acknowledging done.
        for (hook, icon) in [("Notification", "!"), ("UserPromptSubmit", "*"), ("Stop", "+")] {
            claude_hook(
                &env,
                "writer",
                &format!(r#"{{"hook_event_name":"{hook}","message":"Claude needs your permission"}}"#),
            );
            client.wait_for_rows("the writer's state in the strip", |rows| rows[5].starts_with(&format!(" 2{icon}|")));
        }
        client.settle();
        assert!(client.rows()[5].starts_with(" 2+|"));
        client.send(b"\x1b[<0;2;6M\x1b[<0;2;6m");
        client.wait_for_rows("visiting writer acknowledges done", |rows| {
            rows[0].starts_with("WS | docs ") && rows[5].starts_with(" 2o|")
        });

        client.send(b"\x1b[<2;2;2M");
        client.wait_for_row("Rename");
        client.close_overlay("Rename");
        client.close_overlay("+ Add workspace");
        client.send(format!("\x1b[<0;2;{}M\x1b[<0;2;{}m", height - 1, height - 1).as_bytes());
        if cols >= 84 {
            client.wait_for_row("WORKSPACES");
        } else {
            client.wait_for_row("+ Add workspace");
            client.close_overlay("+ Add workspace");
            client.resize((124, 36));
            client.wait_for_row("WORKSPACES");
        }
    }
}

/// The sidebar is there from the first screen of a new session, with one workspace and nothing
/// else to list. The key folds it and expands it again in the terminal it is typed in, `[sidebar]
/// visible` says for the terminals that did not choose, and a terminal less than three times as
/// wide as the sidebar has a four-column strip, down to 40 columns.
#[test]
fn the_sidebar_is_there_from_the_first_screen() {
    let env = Env::new("sidebar");
    let shown = |rows: &[String]| rows[0].starts_with("╭─ WORKSPACES");
    // With the strip, the top bar names the workspace.
    let folded = |rows: &[String]| rows[0].starts_with("WS │ main ");
    let shows = |client: &RawClient, what: &str| client.wait_for_rows(what, shown);
    let folds = |client: &RawClient, what: &str| client.wait_for_rows(what, folded);
    let size = || {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        (panes[0]["cols"].as_u64().unwrap(), panes[0]["rows"].as_u64().unwrap())
    };
    let config = env.root.join("config/kraal/config.toml");
    let defaults = std::fs::read_to_string(&config).unwrap();
    let configure = |sidebar: &str| {
        std::fs::write(&config, format!("{defaults}{sidebar}")).unwrap();
        env.run(&["config", "reload"]);
    };

    // A session started by a command that opens nothing: the first terminal gets the workspace
    // `main` and a shell, and its first frame has the sidebar, the pane beside it.
    env.run(&["tunnel", "list"]);
    let mut client = RawClient::attach_sized(&env, None, (120, 40));
    let rows = client.first_frame();
    assert!(shown(&rows) && rows[1].starts_with("│ ● main"), "{}", rows.join("\n"));
    assert_eq!(frame_columns(&rows), (Some(29), Some(119)), "{}", rows.join("\n"));
    assert_eq!(size(), (89, 36));
    // What has nothing to list has no section.
    client.settle();
    let rows = client.rows();
    assert!(rows[2].starts_with("│   Open · 1 tab · 1 pane"), "{}", rows.join("\n"));
    let sections = rows.iter().filter(|r| r.starts_with("├─ ")).collect::<Vec<_>>();
    assert!(sections.iter().all(|s| s.starts_with("├─ ACCOUNTS")), "{sections:?}");

    // The key folds it in the terminal it is typed in, which gives room to the panes, and
    // shows it again. Another terminal keeps its own.
    let other = RawClient::attach(&env, "main");
    shows(&other, "the sidebar of a second terminal");
    client.send(b"\x02Vl");
    let rows = folds(&client, "the sidebar folded with the key");
    assert_eq!(frame_columns(&rows), (Some(4), Some(119)), "{}", rows.join("\n"));
    assert_eq!(size(), (114, 36));
    other.settle();
    assert!(shown(&other.rows()), "{}", other.rows().join("\n"));
    client.send(b"\x02Vl");
    shows(&client, "the sidebar shown again with the key");
    // A zoomed pane takes the room of the panes, beside the sidebar.
    env.run(&["run", "--name", "second", "--right", "--", "sh"]);
    client.wait_for_rows("two panes side by side", |rows| frame_columns(rows).1.is_some_and(|end| end < 119));
    client.send(b"\x02z");
    let rows = client.wait_for_rows("the zoomed pane", |rows| frame_columns(rows) == (Some(29), Some(119)));
    assert!(shown(&rows), "{}", rows.join("\n"));
    env.run(&["close", "second"]);

    // `visible = false` folds it where no one chose. A terminal that requested it keeps it;
    // a terminal attaching starts with the strip, and the key expands it there too.
    configure("[sidebar]\nvisible = false\n");
    folds(&other, "the sidebar folded by the configuration");
    client.settle();
    assert!(shown(&client.rows()), "{}", client.rows().join("\n"));
    let mut late = RawClient::attach(&env, "main");
    folds(&late, "the strip for a terminal attaching");
    late.send(b"\x02Vl");
    shows(&late, "the sidebar asked for with the key");
    // So wide that no terminal has room for it, the strip takes its place.
    configure("[sidebar]\nwidth = 30000\n");
    folds(&client, "no room for the sidebar");

    // A terminal less than three times the sidebar width gets the strip; below 40 it disappears.
    configure("");
    let mut narrow = RawClient::attach_sized(&env, Some("main"), (80, 24));
    let rows = folds(&narrow, "the strip in 80 columns");
    assert_eq!(frame_columns(&rows), (Some(4), Some(79)), "{}", rows.join("\n"));
    assert_eq!(size(), (74, 20));
    for (cols, frame) in [(39, (0, 38)), (40, (4, 39)), (60, (4, 59)), (83, (4, 82)), (84, (29, 83)), (80, (4, 79))] {
        narrow.resize((cols, 24));
        let what = format!("the pane of a terminal of {cols} columns");
        let rows = narrow.wait_for_rows(&what, |rows| frame_columns(rows) == (Some(frame.0), Some(frame.1)));
        assert_eq!(shown(&rows), cols == 84, "{}", rows.join("\n"));
    }

    // What a terminal chose is its own, and is not saved: after a restart, the configuration
    // says again.
    client.send(b"\x02Vl");
    folds(&client, "the sidebar folded before a restart");
    env.run(&["restart", "--yes"]);
    let back = RawClient::attach_sized(&env, None, (120, 40));
    shows(&back, "the sidebar after a restart");
}

/// The panes have one size, that of the terminal typed in last, and every attached terminal has
/// its own sidebar: full, folded with the key, or automatically compact in a narrow terminal.
/// Each terminal draws panes beside its sidebar, without overlapping it or leaving its place empty.
#[test]
fn each_terminal_draws_the_panes_beside_its_own_sidebar() {
    let env = Env::new("beside");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    // The columns of the sidebar and the space after it, above the place of the accounts (which
    // come with their usage, and that may be seen while the test runs).
    let side = |rows: &[String]| -> Vec<String> { rows[..32].iter().map(|r| r.chars().take(29).collect()).collect() };
    let shows = |client: &RawClient, columns: (usize, usize), when: &str| {
        let columns = (Some(columns.0), Some(columns.1));
        let rows = client.wait_for_rows(when, |rows| frame_columns(rows) == columns);
        // And it stays so.
        client.settle();
        assert_eq!(frame_columns(&client.rows()), columns, "{when}:\n{}", client.rows().join("\n"));
        rows
    };
    let mut wide = RawClient::attach(&env, "w");
    let sidebar = side(&shows(&wide, (29, 119), "the pane beside the sidebar"));
    assert!(sidebar[0].starts_with("╭─ WORKSPACES"), "{sidebar:?}");

    // A second terminal folds its sidebar: panes start after the strip.
    // The first one keeps its sidebar, and shows of the panes what fits beside it.
    let mut bare = RawClient::attach(&env, "w");
    bare.send(b"\x02Vl");
    shows(&bare, (4, 119), "the pane beside the strip");
    assert_eq!(side(&shows(&wide, (29, 119), "the pane beside the sidebar still")), sidebar);
    // Typed in, the first terminal has the panes laid out beside its sidebar: the other one
    // shows them beside its strip, with room left at its right.
    wide.send(b"echo typed-in-wide\r");
    shows(&bare, (4, 94), "the narrower pane beside the strip");
    assert_eq!(side(&shows(&wide, (29, 119), "the pane laid out beside the sidebar")), sidebar);

    // A narrow terminal gets the strip, and the panes are laid out for it.
    let mut narrow = RawClient::attach_sized(&env, Some("w"), (80, 24));
    let rows = shows(&narrow, (4, 79), "the pane beside the strip");
    assert!(rows[0].starts_with("WS │ w "), "the workspace is named in the top bar: {:?}", rows[0]);
    shows(&bare, (4, 79), "the narrow terminal's pane beside the strip");
    assert_eq!(side(&shows(&wide, (29, 104), "the narrow terminal's pane beside the sidebar")), sidebar);
    // It shows the panes of a wider one beside the strip, clipped to its last column.
    wide.send(b"echo typed-again\r");
    narrow.wait_for_row("typed-again");
    shows(&narrow, (4, 79), "the wide terminal's pane beside the strip");
    shows(&bare, (4, 94), "the wide terminal's pane");
    assert_eq!(side(&shows(&wide, (29, 119), "the pane beside the sidebar again")), sidebar);
    // Once wide enough, it has the sidebar too.
    narrow.resize((120, 40));
    assert_eq!(side(&shows(&narrow, (29, 119), "the sidebar once there is room")), sidebar);
}

/// An account's usage window resets with the clock, whatever else happens: the sidebar shows the
/// account without it from then on, while a pane prints (frames made from what that pane printed
/// alone, each checked against a whole one in this build) as on a screen where nothing moves.
#[test]
fn the_sidebar_follows_a_usage_window_that_resets() {
    let env = Env::new("resets");
    std::fs::write(
        env.root.join("config/kraal/accounts.toml"),
        "[[account]]\nid = 'work'\nname = 'work'\nagent = 'claude'\nhome = '~/work'\n",
    )
    .unwrap();
    let usage = env.root.join("state/kraal/usage/by-id");
    std::fs::create_dir_all(&usage).unwrap();
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_row("ACCOUNTS");
    client.wait_for_row("Claude · work");
    let now = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    // The named account's usage: its five-hour window, the fuller, and its weekly one.
    let seen = |five_hour: u64, weekly: u64| {
        let said = serde_json::json!({
            "five_hour": { "used_percent": 93, "resets_at": five_hour },
            "weekly": { "used_percent": 41, "resets_at": weekly },
            "observed_at": now(),
            "source": "statusline",
        });
        std::fs::write(usage.join("new"), said.to_string()).unwrap();
        std::fs::rename(usage.join("new"), usage.join("claude-work.json")).unwrap();
    };
    let shows = |rows: &[String], detail: &str| {
        rows.windows(2)
            .any(|r| r[0].contains("Claude · work") && r[1].split('│').nth(1).is_some_and(|text| text.trim() == detail))
    };
    // Keys typed one after the other, each echoed in a frame of its own, until the sidebar says
    // `figure` of the account.
    let typing_until = |client: &mut RawClient, figure: &str| {
        let row = format!("{figure} used");
        let start = Instant::now();
        while !shows(&client.rows(), &row) {
            assert!(start.elapsed() < Duration::from_secs(15), "never {row}:\n{}", client.rows().join("\n"));
            client.send(b"x");
            std::thread::sleep(Duration::from_millis(15));
        }
    };
    // Resets as far away as a file can say are none to wait for: the usage shows all the same.
    seen(i64::MAX as u64, i64::MAX as u64);
    typing_until(&mut client, "93%");
    // The five-hour window resets while keys are echoed, the weekly one once nothing happens.
    seen(now() + 2, now() + 5);
    typing_until(&mut client, "41%");
    client.wait_for_rows("both usage windows expired", |rows| shows(rows, "Last checked just now"));
}

/// The sidebar names panes of every workspace: what a pane prints that changes its name (a
/// title) shows at once for a client that looks at another workspace, where nothing happens.
#[test]
fn the_sidebar_follows_what_a_pane_of_another_workspace_prints() {
    let env = Env::new("elsewhere");
    env.workspace("here", "layout {\n    pane name=\"quiet\"\n}\n");
    env.workspace("there", "layout {\n    pane\n}\n");
    env.run(&["open", "there"]);
    let client = RawClient::attach(&env, "here");
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let unnamed = panes.as_array().unwrap().iter().find(|p| p["workspace"] == "there").unwrap()["id"].to_string();
    // A port the pane serves, as far as kraal can tell: the sidebar lists it with the pane's name.
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    env.run(&["send", "-p", &unnamed, "--enter", &format!("echo http://localhost:{port}/")]);
    client.wait_for_row(&format!(":{port}"));
    // The title comes when the request that typed its command is long over, and all is quiet.
    env.run(&["send", "-p", &unnamed, "--enter", "sleep 1; printf '\\033]2;renamed\\007'"]);
    client.settle();
    assert!(!client.rows().iter().any(|r| r.contains("renamed")), "{}", client.rows().join("\n"));
    client.wait_for_row("   renamed");
}

/// The rows a pane shows far back come from its compact history: a client drawn incrementally
/// while the view goes there and back shows what a client attaching then is shown whole.
#[test]
fn rows_brought_back_from_the_history_are_drawn() {
    let env = Env::new("farback");
    std::fs::create_dir_all(env.root.join("state/kraal")).unwrap();
    std::fs::write(env.root.join("state/kraal/welcomed"), "").unwrap();
    env.workspace("w", "layout {\n    pane name=\"log\"\n}\n");
    env.run(&["open", "w"]);
    env.wait_for_text("log", "$");
    env.run(&["send", "-p", "log", "seq 1 4000; echo ENDS\r"]);
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_screen("ENDS");
    // The pane's rows, between the bars (they show the client's mode), compared.
    let same = |client: &RawClient, step: &str| {
        client.settle();
        let fresh = RawClient::attach(&env, "w");
        fresh.settle();
        client.settle();
        let (a, b) = (client.screen.lock().unwrap().clone(), fresh.screen.lock().unwrap().clone());
        let (a, b) = (terminal_after(&a, 120, 40), terminal_after(&b, 120, 40));
        for (y, (a, b)) in a.iter().zip(&b).enumerate().take(39).skip(1) {
            assert_eq!(a, b, "{step}: row {y} differs");
        }
        drop(fresh);
        client.settle();
    };
    same(&client, "at the bottom");
    // Many screens back with the wheel, beyond what the emulator keeps.
    for _ in 0..80 {
        client.send(b"\x1b[<64;60;20M");
    }
    same(&client, "far back");
    // A search goes further back still, then down again.
    client.send(b"\x02/");
    client.settle();
    client.send(b"1234");
    client.send(b"\r");
    same(&client, "search far back");
    // The view is there, far beyond the rows the emulator keeps.
    let rows = terminal_after(&client.screen.lock().unwrap(), 120, 40);
    let text = |row: &String| -> String {
        row.split(" | ").map(|c| c.split(' ').next().unwrap_or("").trim_matches('"').to_string()).collect()
    };
    assert!(rows.iter().any(|r| text(r).contains("│1234│")), "{:?}", rows.iter().map(text).collect::<Vec<_>>());
    for _ in 0..30 {
        client.send(b"\x1b[<65;60;20M");
    }
    same(&client, "down again");
    client.send(b"\x1b");
    std::thread::sleep(Duration::from_millis(100));
    same(&client, "back at the bottom");
}

/// The last row shows what can be done in the focused pane, with its keys, and the menu key at
/// the right end; `ctrl+b V k` gives the row to the panes, and the menu key goes to the top bar.
#[test]
fn the_bottom_bar_shows_the_focused_panes_actions() {
    let env = Env::new("bottombar");
    std::fs::create_dir_all(env.root.join("state/kraal")).unwrap();
    std::fs::write(env.root.join("state/kraal/welcomed"), "").unwrap();
    // A fake ssh: a shell "on the server".
    let ssh = env.root.join("bin/ssh");
    std::fs::create_dir_all(ssh.parent().unwrap()).unwrap();
    write_script(&ssh, "#!/bin/sh\nexec sh\n");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let height = |name: &str| -> u64 {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        panes.as_array().unwrap().iter().find(|p| p["name"] == name).unwrap()["rows"].as_u64().unwrap()
    };
    let mut client = RawClient::attach(&env, "w");
    let shown = client.wait_for_row("alt+n new pane");
    let last = shown[39].trim();
    assert!(last.starts_with("alt+n new pane  ctrl+b P → split  ctrl+b s connect to a server"), "{last}");
    assert!(last.ends_with("ctrl+b menu") && !shown[0].contains("menu"), "{shown:?}");
    assert_eq!(height("main"), 36, "the panes end above the bar");

    env.run(&["run", "--name", "web", "--", &ssh.display().to_string(), "web-prod-1"]);
    env.run(&["focus", "web"]);
    let shown = client.wait_for_row("web-prod-1 ▸ ctrl+b f its files  ctrl+b r tools there  ctrl+b S d download");
    assert!(shown[39].trim_end().ends_with("ctrl+b menu"), "{shown:?}");

    // A click on the menu key opens the menu (mouse columns count from 1).
    let col = shown[39].chars().count() - "menu ".len() + 1;
    client.send(format!("\x1b[<0;{col};40M\x1b[<0;{col};40m").as_bytes());
    client.wait_for_row("Connect to a server");
    client.send(b"\x1b");

    // Off for the session: the panes get the row, the menu key goes to the top bar.
    client.send(b"\x02Vk");
    let shown = client.wait_for_rows("the menu key on top", |rows| rows[0].contains("ctrl+b menu"));
    assert!(!shown[39].contains("its files"), "{shown:?}");
    let start = Instant::now();
    while height("main") != 37 {
        assert!(start.elapsed() < Duration::from_secs(10), "the panes never got the row");
        std::thread::sleep(Duration::from_millis(50));
    }
    client.send(b"\x02Vk");
    client.wait_for_row("web-prod-1 ▸ ctrl+b f its files");
}

/// The menu, pickers and viewers go above the bottom bar, the menu right on it; without the bar
/// they go down to the last row.
#[test]
fn overlays_stay_above_the_bottom_bar() {
    let env = Env::new("overbar");
    std::fs::create_dir_all(env.root.join("state/kraal")).unwrap();
    std::fs::write(env.root.join("state/kraal/welcomed"), "").unwrap();
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let mut client = RawClient::attach(&env, "w");
    let bar = client.wait_for_row("alt+n new pane")[39].clone();
    let framed = |row: &str| row.contains(['│', '╭', '╮', '╰', '╯', '─']);
    for (keys, title) in [(&b"\x02"[..], "Connect to a server"), (b"\x02 ", "Commands"), (b"\x02Vj", "Messages")] {
        client.send(keys);
        let rows = client.wait_for_row(title);
        assert!(!framed(&rows[39]), "{title} over the bar:\n{}", rows.join("\n"));
        client.send(b"\x1b");
        client.wait_for_rows("the bar back", |rows| rows[39] == bar);
    }
    // The menu sits right on the bar, which keeps its keys while the menu is open.
    client.send(b"\x02");
    let rows = client.wait_for_row("Connect to a server");
    assert!(rows[38].trim_end().ends_with('╯') && rows[39] == bar, "{}", rows.join("\n"));
    client.send(b"\x1b");

    // Without the bar, down to the last row.
    client.send(b"\x02Vk");
    client.wait_for_rows("the menu key on top", |rows| rows[0].contains("ctrl+b menu"));
    client.send(b"\x02");
    let rows = client.wait_for_row("Connect to a server");
    assert!(rows[39].trim_end().ends_with('╯'), "{}", rows.join("\n"));
}

/// What the program of the pane `name` says once told of the size the pane has now.
fn says(env: &Env, name: &str) -> String {
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let pane = panes.as_array().unwrap().iter().find(|p| p["name"] == name).cloned().unwrap();
    format!("{name} is {} {}", pane["rows"], pane["cols"])
}

/// Tell the server the client's terminal is now `cols × rows`; how much it had drawn until then.
fn resize(client: &mut RawClient, cols: usize, rows: usize) -> usize {
    let drawn = client.screen.lock().unwrap().len();
    raw_frame(&mut client.stream, 0, format!(r#"{{"type":"resize","cols":{cols},"rows":{rows}}}"#).as_bytes());
    drawn
}

/// The frames a client was sent.
fn frames(client: &RawClient) -> usize {
    String::from_utf8_lossy(&client.screen.lock().unwrap()).matches("\x1b[?2026l").count()
}

/// What its program draws when a pane is resized is the picture the user waits for: every pane
/// shows it, at the size it was given last, however fast the resizes came, and the client ends
/// with what a client attaching then is shown whole.
#[test]
fn panes_resized_show_what_their_programs_draw_again() {
    let env = Env::new("redraws");
    std::fs::create_dir_all(env.root.join("state/kraal")).unwrap();
    std::fs::write(env.root.join("state/kraal/welcomed"), "").unwrap();
    let names = ["one", "two", "three", "four"];
    let pane = |name: &str| format!("pane name={name:?} command=\"sh\" {{ args \"-c\" {:?}; }}", says_its_size(name));
    let column = |a: &str, b: &str| format!("pane split_direction=\"horizontal\" {{\n{}\n{}\n}}", pane(a), pane(b));
    env.workspace(
        "w",
        &format!(
            "layout {{\npane split_direction=\"vertical\" {{\n{}\n{}\n}}\n}}\n",
            column("one", "two"),
            column("three", "four")
        ),
    );
    env.run(&["open", "w"]);
    let mut client = RawClient::attach(&env, "w");
    for name in names {
        client.wait_for_row(&format!("{name} ready"));
    }
    client.settle();
    // One resize, then many faster than the programs answer, then back: each ends at a size no
    // pane had before.
    let bursts: [&[(usize, usize)]; 3] = [
        &[(100, 30)],
        &[(90, 28), (140, 44), (96, 32), (132, 42), (88, 26), (150, 46), (104, 34), (126, 38)],
        &[(120, 40)],
    ];
    let mut drawn = 0;
    for sizes in bursts {
        for (cols, rows) in sizes {
            drawn = resize(&mut client, *cols, *rows);
            std::thread::sleep(Duration::from_millis(1));
        }
        let (cols, rows) = sizes[sizes.len() - 1];
        for name in names {
            wait_until(Duration::from_secs(10), &format!("the size of {name} never shown"), || {
                // The first frame after a resize draws the whole screen: what came since the
                // last one is what a terminal of that size shows.
                let shown = text_after(&client.screen.lock().unwrap()[drawn..], cols, rows);
                let said = says(&env, name);
                shown.iter().any(|row| row.contains(&said))
            });
        }
        client.settle();
    }
    let fresh = RawClient::attach(&env, "w");
    fresh.settle();
    client.settle();
    let (a, b) = (client.screen.lock().unwrap()[drawn..].to_vec(), fresh.screen.lock().unwrap().clone());
    // The cells and the cursor: the modes were set when the client attached, before the frames
    // read here.
    let shown = |bytes: &[u8]| terminal_after(bytes, 120, 40)[..41].to_vec();
    for (y, (a, b)) in shown(&a).iter().zip(&shown(&b)).enumerate() {
        assert_eq!(a, b, "row {y} differs");
    }
}

/// A pane printing without a pause is drawn at its own pace whatever is resized meanwhile: a
/// resize adds its frame and those of the panes that draw themselves again, not a frame for
/// each piece of what the other one prints.
#[test]
fn resizes_while_a_pane_floods_do_not_draw_a_storm_of_frames() {
    let env = Env::new("resizeflood");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    env.run(&["run", "--name", "quiet", "--right", "--", "sh", "-c", &says_its_size("quiet")]);
    env.run(&["run", "--name", "flood", "--down", "--", "yes"]);
    env.run(&["focus", "main"]);
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_row("quiet ready");
    std::thread::sleep(Duration::from_millis(300));
    let (before, start) = (frames(&client), Instant::now());
    for i in 0..20 {
        let (cols, rows) = if i % 2 == 0 { (100, 30) } else { (120, 40) };
        resize(&mut client, cols, rows);
        std::thread::sleep(Duration::from_millis(50));
    }
    let per_second = (frames(&client) - before) as f64 / start.elapsed().as_secs_f64();
    // The flood is drawn every 8 ms or so, and each resize adds a few frames: far from the
    // thousands a frame for each read of the flood would make.
    assert!(per_second < 400.0, "{per_second:.0} frames a second");
    // The quiet pane's picture still comes through, at a size it did not have yet.
    let drawn = resize(&mut client, 110, 36);
    wait_until(Duration::from_secs(10), "the size of the quiet pane never shown", || {
        let shown = text_after(&client.screen.lock().unwrap()[drawn..], 110, 36);
        let said = says(&env, "quiet");
        shown.iter().any(|row| row.contains(&said))
    });
}

#[test]
fn a_pane_printing_without_a_pause_does_not_starve_the_others() {
    let env = Env::new("starve");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    env.run(&["run", "--name", "other", "--", "sh", "-c", "sleep 1; echo OTHER-SPOKE; sleep 60"]);
    env.run(&["run", "--name", "flood", "--", "yes"]);
    env.run(&["focus", "flood"]);
    let _client = RawClient::attach(&env, "w");
    env.wait_for_text("other", "OTHER-SPOKE");
}

/// What a program prints first is drawn at once, up to a screenful: one that prints a little at
/// a time without a pause, from its start, is then drawn at its pace like any other, not once
/// for each piece of what it prints.
#[test]
fn a_program_printing_without_a_pause_from_its_start_draws_no_storm_of_frames() {
    let env = Env::new("firstflood");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let client = RawClient::attach(&env, "w");
    client.wait_for_row("$");
    let frames = || String::from_utf8_lossy(&client.screen.lock().unwrap()).matches("\x1b[?2026l").count();
    let (before, start) = (frames(), Instant::now());
    // A line at a time, each written by itself, thousands of them a second.
    let ticks = "BEGIN { while (1) { print \"tick\"; fflush(); for (i = 0; i < 2000; i++) {} } }";
    env.run(&["run", "--name", "ticks", "--right", "--", "awk", ticks]);
    std::thread::sleep(Duration::from_secs(1));
    let per_second = (frames() - before) as f64 / start.elapsed().as_secs_f64();
    // A frame for each of its first rows, then one every 8 ms or so.
    assert!(per_second < 400.0, "{per_second:.0} frames a second");
    client.wait_for_row("tick");
}

#[test]
fn output_parsed_while_keys_come_is_whole_and_in_order() {
    let env = Env::new("slices");
    env.workspace("w", "layout {\n    pane name=\"main\" command=\"cat\"\n}\n");
    env.run(&["open", "w"]);
    // About 280 KB in lines narrower than the pane: chunks of it are parsed while keys come,
    // each of which stops the parse for a moment.
    let script = r#"sleep 0.5; awk 'BEGIN { for (i = 1; i <= 9000; i++) printf "%06d %023d\n", i, 0 }'; echo FLOOD-DONE; exec sleep 60"#;
    env.run(&["run", "--name", "flood", "--", "sh", "-c", script]);
    env.run(&["focus", "main"]);
    let mut client = RawClient::attach(&env, "w");
    let start = Instant::now();
    while !env.run(&["read", "-p", "flood"]).contains("FLOOD-DONE") {
        assert!(start.elapsed() < Duration::from_secs(10), "the output never ended");
        for _ in 0..20 {
            client.send(b"x");
            std::thread::sleep(Duration::from_micros(500));
        }
    }
    let text = env.run(&["read", "-p", "flood", "--lines", "9500"]);
    let numbers: Vec<u32> = text.lines().filter_map(|l| l.split_whitespace().next()?.parse().ok()).collect();
    assert!(numbers == (1..=9000).collect::<Vec<u32>>(), "{} lines, first gap after {:?}", numbers.len(), {
        numbers.windows(2).find(|w| w[1] != w[0] + 1)
    });
}

#[test]
fn picker_actions_and_footers_follow_the_line_at_both_sizes() {
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::cell::Flags;

    let env = Env::new("pickerlines");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.workspace("closed", "layout {\n    pane\n}\n");
    std::fs::write(
        env.root.join("config/kraal/servers.toml"),
        "[[server]]\nname = \"web\"\nhost = \"web.example.com\"\ngroups = [\"fleet\"]\n",
    )
    .unwrap();
    env.run(&["open", "w"]);
    let click = |client: &mut RawClient, text: &str| {
        let rows = client.wait_for_row(text);
        let y = rows.iter().position(|r| r.contains(text)).unwrap();
        let x = rows[y][..rows[y].find(text).unwrap()].chars().count();
        client.send(format!("\x1b[<0;{};{}M\x1b[<0;{};{}m", x + 1, y + 1, x + 1, y + 1).as_bytes());
    };
    for size in [(124, 36), (80, 24)] {
        let mut client = RawClient::attach_sized(&env, Some("w"), size);
        client.send(b"\x02Al");
        client.wait_for_row("Enter Add account");
        let label = "+ Add account";
        let rows = client.wait_for_row(label);
        let y = rows.iter().position(|r| r.contains(label)).unwrap();
        let x = rows[y][..rows[y].find(label).unwrap()].chars().count();
        let term = emulate(&client.screen.lock().unwrap(), size.0, size.1);
        assert!(term.grid()[Line(y as i32)][Column(x)].flags.contains(Flags::BOLD), "at {size:?}");
        click(&mut client, label);
        client.wait_for_row("Name    e.g. Work or Personal");
        client.wait_for_row("Agent   Claude");
        client.close_overlay("Agent   Claude");
        client.wait_for_row("+ Add account");
        client.close_overlay("Enter Add account");

        client.send(b"\x02wclosed");
        client.wait_for_row("Enter Open · Tab Actions · Esc Close");
        client.send(b"\t");
        let rows = client.wait_for_row("Delete");
        assert!(rows.iter().any(|r| r.contains("Enter") && r.contains("Open")));
        // Match action names as words: the sidebar can still show the status "Closed".
        for absent in ["Rename", "Close"] {
            assert!(
                !rows.iter().any(|r| r.split_whitespace().any(|word| word == absent) && !r.contains("Esc Close")),
                "{size:?}: {absent}\n{}",
                rows.join("\n")
            );
        }
        client.close_overlay("Delete");
        client.send(b"\x1b[B");
        let rows = client.wait_for_row("Enter Add");
        assert!(!rows.iter().any(|r| r.contains("Tab Actions")));
        client.send(b"\x1b");
        client.wait_for_rows("the workspace picker closed", |rows| !rows.iter().any(|r| r.contains("Search  closed")));

        client.send(b"\x02s");
        client.wait_for_row("╭ Servers ");
        client.wait_for_row("Enter Open group");
        client.send(b"\t");
        let rows = client.wait_for_row("Run a command on it");
        for absent in ["Connect in a new tab", "Browse its files", "Add tunnel", "Remove saved server"] {
            assert!(!rows.iter().any(|r| r.contains(absent)), "{size:?}: {absent}");
        }
        click(&mut client, "Run a command on it");
        client.wait_for_row("Run on @fleet");
        client.send(b"\x1b");
    }
}

/// While the keys after the prefix are awaited, the client's terminal is asked for every key
/// with its place on the keyboard (kitty keyboard protocol), and set back after: the AZERTY key
/// that types `&`, at the place of `1`, goes to tab 1. A terminal that cannot send the typed text
/// along is not asked again.
#[test]
fn the_leader_menu_reads_keys_by_their_place() {
    let env = Env::new("byplace");
    env.workspace(
        "w",
        "layout {\n    tab name=\"a\" {\n        pane name=\"a\"\n    }\n    tab name=\"b\" {\n        pane name=\"b\"\n    }\n}\n",
    );
    env.run(&["open", "w"]);
    env.run(&["focus", "b"]);
    let focused = || -> String {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let shown = panes.as_array().unwrap().iter().find(|p| p["focused"] == true).cloned();
        shown.and_then(|p| p["name"].as_str().map(String::from)).unwrap_or_default()
    };
    let count =
        |client: &RawClient, what: &str| String::from_utf8_lossy(&client.screen.lock().unwrap()).matches(what).count();
    let raised = "\x1b[=29;1u";
    let lowered = "\x1b[=5;1u";

    let mut client = RawClient::attach(&env, "w");
    client.send(b"\x02");
    client.wait_for_screen(&format!("{raised}\x1b[?u"));
    client.send(b"\x1b[?29u");
    client.send(b"\x1b[38::49;;38u");
    client.wait_for_screen(lowered);
    assert_eq!(focused(), "a", "the key at the place of 1 went to tab 1");
    // Asked once: the next time, only raised.
    client.send(b"\x02");
    let start = Instant::now();
    while count(&client, raised) < 2 {
        assert!(start.elapsed() < Duration::from_secs(10), "not raised the second time");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(count(&client, "\x1b[?u"), 1);
    client.send(b"\x1b");

    // A terminal that grants every key as a sequence but not its text is set back at once, and
    // not raised again.
    let mut other = RawClient::attach(&env, "w");
    other.send(b"\x02");
    other.wait_for_screen(&format!("{raised}\x1b[?u"));
    other.send(b"\x1b[?13u");
    other.wait_for_screen(lowered);
    other.send(b"\x1b");
    other.send(b"\x02");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(count(&other, raised), 1);
}

/// A program's kitty image reaches a terminal that shows images: sent to it once, then drawn as
/// placeholder cells where the program put it. A terminal that cannot draw them that way gets
/// neither.
#[test]
fn images_reach_a_terminal_that_shows_them() {
    let env = Env::new("images");
    env.workspace("w", "layout {\n    pane name=\"img\"\n}\n");
    env.run(&["open", "w"]);
    env.wait_for_text("img", "$");
    let script = env.root.join("img.sh");
    // A 2×2 RGB image shown over 4 columns and 2 rows.
    write_script(
        &script,
        "#!/bin/sh\nprintf '\\033_Ga=T,f=24,s=2,v=2,c=4,r=2,q=2;AAAAAAAAAAAAAAAA\\033\\\\'\necho shown\n",
    );

    let mut plain = RawClient::attach(&env, "w");
    let mut client = RawClient::attach(&env, "w");
    // The server asks each terminal; this one answers as Ghostty does, the other as a terminal
    // that speaks the protocol without drawing placeholders.
    client.wait_for_screen("a=q");
    client.send(b"\x1b_Gi=1263684657;OK\x1b\\\x1bP>|ghostty 1.1.3\x1b\\\x1b[6;20;10t");
    plain.wait_for_screen("a=q");
    plain.send(b"\x1b_Gi=1263684657;OK\x1b\\\x1bP>|WezTerm 20240203\x1b\\");
    std::thread::sleep(Duration::from_millis(300));
    env.run(&["send", "-p", "img", &format!("{}\r", script.display())]);
    env.wait_for_text("img", "shown");
    client.wait_for_screen("\x1b_Ga=t,f=100,i=");
    client.wait_for_screen("\x1b_Ga=p,U=1,i=");
    client.wait_for_screen("\u{10EEEE}");
    plain.send(b"\x1b[I");
    std::thread::sleep(Duration::from_millis(300));
    let seen = String::from_utf8_lossy(&plain.screen.lock().unwrap()).into_owned();
    assert!(seen.contains("shown"), "the other client draws the pane");
    assert!(!seen.contains("\x1b_Ga=t") && !seen.contains('\u{10EEEE}'), "no image for a terminal that cannot show it");
}

#[test]
fn images_reach_a_terminal_that_draws_sixel() {
    let env = Env::new("sixel");
    env.workspace("w", "layout {\n    pane name=\"img\"\n}\n");
    env.run(&["open", "w"]);
    env.wait_for_text("img", "$");
    let script = env.root.join("img.sh");
    write_script(
        &script,
        "#!/bin/sh\nprintf '\\033_Ga=T,f=24,s=2,v=2,c=4,r=2,q=2;AAAAAAAAAAAAAAAA\\033\\\\'\necho shown\n",
    );
    let mut client = RawClient::attach(&env, "w");
    // It answers as foot does: no kitty graphics, sixel among its device attributes.
    client.wait_for_screen("\x1b[c");
    client.send(b"\x1bP>|foot(1.21.0)\x1b\\\x1b[6;20;10t\x1b[?62;4;22c");
    std::thread::sleep(Duration::from_millis(300));
    env.run(&["send", "-p", "img", &format!("{}\r", script.display())]);
    env.wait_for_text("img", "shown");
    // 4×2 cells of 10×20 pixels, as many rows of them as whole sixel bands hold.
    client.wait_for_screen("\x1bP0;1;0q\"1;1;40;36#0;2;0;0;0#0");
    let seen = |c: &RawClient| String::from_utf8_lossy(&c.screen.lock().unwrap()).into_owned();
    assert!(!seen(&client).contains('\u{10EEEE}') && !seen(&client).contains("\x1b_Ga=t"), "no placeholders");
    // Text changing next to it does not draw it again.
    let drawn = seen(&client).matches("\x1bP0;1;0q").count();
    env.run(&["send", "-p", "img", "echo dk$((1+1))\r"]);
    env.wait_for_text("img", "dk2");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(seen(&client).matches("\x1bP0;1;0q").count(), drawn);
    // The font changed with the window: the image waits for the new cell size, then takes it.
    let asked = seen(&client).matches("\x1b[16t").count();
    client.resize((120, 40));
    let start = Instant::now();
    while seen(&client).matches("\x1b[16t").count() == asked {
        assert!(start.elapsed() < Duration::from_secs(10), "the cell size is asked again");
        std::thread::sleep(Duration::from_millis(50));
    }
    client.send(b"\x1b[6;18;9t");
    client.wait_for_screen("\x1bP0;1;0q\"1;1;36;36");
    // The screen cleared: the image's cells are erased.
    assert!(!seen(&client).contains("\x1b[4X"));
    env.run(&["send", "-p", "img", "printf '\\033[H\\033[2J'\r"]);
    client.wait_for_screen("\x1b[4X");
}

/// A large sixel image is painted off the server's loop: its cells are taken at once, where the
/// program put it, the text after it goes below it, and the terminal gets it once painted.
#[test]
fn a_large_sixel_image_reaches_the_terminal_with_the_text_after_it_below() {
    let env = Env::new("bigsixel");
    env.workspace("w", "layout {\n    pane name=\"img\"\n}\n");
    env.run(&["open", "w"]);
    env.wait_for_text("img", "$");
    // 400×240 pixels: 40 columns and 12 rows of 10×20 cells.
    let image = env.root.join("big.six");
    let mut sixel = b"\x1bPq\"1;1;400;240#1".to_vec();
    for _ in 0..40 {
        sixel.extend_from_slice(b"!400~-");
    }
    sixel.extend_from_slice(b"\x1b\\");
    std::fs::write(&image, sixel).unwrap();
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_screen("a=q");
    client.send(b"\x1b_Gi=1263684657;OK\x1b\\\x1bP>|ghostty 1.1.3\x1b\\\x1b[6;20;10t");
    std::thread::sleep(Duration::from_millis(300));
    env.run(&["send", "-p", "img", &format!("printf '\\033[H\\033[2J'; cat {}; echo below\r", image.display())]);
    let screen = env.wait_for_text("img", "below");
    assert_eq!(screen.lines().position(|l| l.starts_with("below")), Some(12), "{screen}");
    client.wait_for_screen("\x1b_Ga=t,f=100,i=");
    client.wait_for_screen("\x1b_Ga=p,U=1,i=");
}

#[test]
fn a_terminal_that_tells_its_cell_size_last_draws_sixel_too() {
    let env = Env::new("sixellate");
    env.workspace("w", "layout {\n    pane name=\"img\"\n}\n");
    env.run(&["open", "w"]);
    env.wait_for_text("img", "$");
    let script = env.root.join("img.sh");
    write_script(
        &script,
        "#!/bin/sh\nprintf '\\033_Ga=T,f=24,s=2,v=2,c=4,r=2,q=2;AAAAAAAAAAAAAAAA\\033\\\\'\necho shown\n",
    );
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_screen("\x1b[c");
    // Its device attributes before its cell size, though asked after it.
    client.send(b"\x1bP>|foot(1.21.0)\x1b\\\x1b[?62;4;22c\x1b[6;20;10t");
    std::thread::sleep(Duration::from_millis(300));
    env.run(&["send", "-p", "img", &format!("{}\r", script.display())]);
    env.wait_for_text("img", "shown");
    client.wait_for_screen("\x1bP0;1;0q");
}

#[test]
fn an_image_scrolling_with_the_output_is_not_sent_at_each_step() {
    let env = Env::new("sixelscroll");
    env.workspace("w", "layout {\n    pane name=\"img\"\n}\n");
    env.run(&["open", "w"]);
    env.wait_for_text("img", "$");
    let script = env.root.join("img.sh");
    // The pane is full first: each line after the image scrolls it up a row, 31 times, a few
    // milliseconds apart. The last word ends no line: the prompt follows it on the same row, and
    // nothing moves after.
    write_script(
        &script,
        "#!/bin/sh\nseq 60\nprintf '\\033_Ga=T,f=24,s=2,v=2,c=4,r=2,q=2;AAAAAAAAAAAAAAAA\\033\\\\'\necho\n\
         i=0\nwhile [ $i -lt 30 ]; do echo line $i; sleep 0.005; i=$((i+1)); done\nprintf stopped\n",
    );
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_screen("\x1b[c");
    client.send(b"\x1bP>|foot(1.21.0)\x1b\\\x1b[6;20;10t\x1b[?62;4;22c");
    std::thread::sleep(Duration::from_millis(300));
    env.run(&["send", "-p", "img", &format!("{}\r", script.display())]);
    // The frame that shows the last word has the image where it stopped: what erased it
    // before is in it or earlier. It is drawn there once it has stayed put, however long the
    // machine takes.
    client.wait_for_screen("stopped");
    let start = Instant::now();
    let seen = loop {
        let seen = String::from_utf8_lossy(&client.screen.lock().unwrap()).into_owned();
        let last = seen.rfind("\x1bP0;1;0q");
        if last.is_some() && last > seen.rfind("\x1b[4X") {
            break seen;
        }
        let drawn = seen.matches("\x1bP0;1;0q").count();
        assert!(start.elapsed() < Duration::from_secs(10), "not drawn where it stopped ({drawn} sixel images)");
        std::thread::sleep(Duration::from_millis(50));
    };
    // Drawn again at each step, it would be some thirty times. A slow machine may pause long
    // enough between steps for the image to be drawn where it waits, now and then.
    let drawn = seen.matches("\x1bP0;1;0q").count();
    assert!(drawn <= 10, "{drawn} sixel images for 31 steps");
}

#[test]
fn a_preview_shows_its_image_as_each_terminal_can() {
    let env = Env::new("pimg");
    let png = env.root.join("dot.png");
    image::RgbaImage::from_pixel(2, 2, image::Rgba([200, 10, 10, 255])).save(&png).unwrap();
    env.workspace("w", &format!("layout {{\n    pane name=\"pic\" preview=\"{}\"\n}}\n", png.display()));
    env.run(&["open", "w"]);
    let mut ghostty = RawClient::attach(&env, "w");
    let mut foot = RawClient::attach(&env, "w");
    let plain = RawClient::attach(&env, "w");
    ghostty.wait_for_screen("\x1b[c");
    ghostty.send(b"\x1b_Gi=1263684657;OK\x1b\\\x1bP>|ghostty 1.1.3\x1b\\\x1b[6;20;10t\x1b[?62;22c");
    foot.wait_for_screen("\x1b[c");
    foot.send(b"\x1bP>|foot(1.21.0)\x1b\\\x1b[6;20;10t\x1b[?62;4;22c");
    ghostty.wait_for_screen("\x1b_Ga=t,f=100,i=");
    ghostty.wait_for_screen("\x1b_Ga=p,U=1,i=");
    ghostty.wait_for_screen("\u{10EEEE}");
    foot.wait_for_screen("\x1bP0;1;0q\"1;1;");
    // A terminal that shows no images gets half blocks.
    plain.wait_for_screen("▀");
    let seen = String::from_utf8_lossy(&plain.screen.lock().unwrap()).into_owned();
    assert!(!seen.contains("\x1b_Ga=t") && !seen.contains("\x1bP0;1;0q") && !seen.contains('\u{10EEEE}'));
}

/// A preview follows its file, also when nothing else happens in the session.
#[test]
fn a_preview_follows_its_file_in_a_quiet_session() {
    let env = Env::new("pfollow");
    let notes = env.root.join("notes.txt");
    std::fs::write(&notes, "first-version\n").unwrap();
    env.workspace("w", &format!("layout {{\n    pane name=\"n\" preview=\"{}\"\n}}\n", notes.display()));
    env.run(&["open", "w"]);
    let client = RawClient::attach(&env, "w");
    client.wait_for_screen("first-version");
    // Long enough for the server to be idle.
    std::thread::sleep(Duration::from_secs(4));
    std::fs::write(&notes, "second-version\n").unwrap();
    let start = Instant::now();
    client.wait_for_screen("second-version");
    assert!(start.elapsed() < Duration::from_secs(3), "shown after {:?}", start.elapsed());
}
