use super::support::{RawClient, buffer_list, set_buffer, wait_gone, wait_production};
use crate::common::Env;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

#[test]
fn paste_buffers_from_the_command_line() {
    let env = Env::new("buffers");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    let keys = env.run(&["config", "keys"]);
    assert!(keys.contains("ctrl+b V p, ctrl+b =") && keys.contains("Paste buffers"), "{keys}");
    env.run(&["open", "w"]);
    let out = env.try_run(&["buffer", "show"]);
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("no paste buffers"));
    // Text from the standard input, and from a file under a name.
    assert!(set_buffer(&env, &[], "echo first\n").status.success());
    let file = env.root.join("notes.txt");
    std::fs::write(&file, "line one\nline two\n").unwrap();
    env.run(&["buffer", "set", "notes", "--from-file", &file.display().to_string()]);
    // What a program copies (OSC 52) is kept too, as the newest.
    let copy = "printf '\\033]52;c;%s\\007' \"$(printf copied-by-a-program | base64)\"; exec sleep 60";
    env.run(&["run", "--name", "copier", "--", "sh", "-c", copy]);
    let start = Instant::now();
    while buffer_list(&env).len() < 3 {
        assert!(start.elapsed() < Duration::from_secs(10), "the program's copy was not kept");
        std::thread::sleep(Duration::from_millis(50));
    }
    let list = buffer_list(&env);
    let keys: Vec<&str> = list.iter().map(|b| b["buffer"].as_str().unwrap()).collect();
    assert_eq!(keys, ["0", "notes", "1"], "newest first; numbers count the numbered buffers");
    let notes = (list[1]["named"].as_bool(), list[1]["lines"].as_u64(), list[1]["bytes"].as_u64());
    assert_eq!(notes, (Some(true), Some(2), Some(18)));
    assert_eq!(env.run(&["buffer", "show"]), "copied-by-a-program");
    assert_eq!(env.run(&["buffer", "show", "notes"]), "line one\nline two\n");
    assert_eq!(env.run(&["buffer", "show", "1"]), "echo first\n");
    let shown = env.run(&["buffer", "list"]);
    assert!(shown.contains("notes") && shown.contains("line one | line two"), "{shown}");
    // The same text again is not kept twice, and a number is not a name.
    assert!(set_buffer(&env, &[], "copied-by-a-program").status.success());
    assert_eq!(buffer_list(&env).len(), 3);
    let out = set_buffer(&env, &["7"], "x");
    assert!(!out.status.success() && String::from_utf8_lossy(&out.stderr).contains("is a number"));
    assert!(!env.try_run(&["buffer", "show", "5"]).status.success());

    // A program that asked for bracketed paste gets the markers around it; one that did not
    // gets its line ends as Enter.
    assert!(set_buffer(&env, &["word"], "pasted-word").status.success());
    let show = |mode: &str| format!("stty raw -echo; printf '{mode}READY'; exec cat -v");
    env.run(&["run", "--name", "bracketed", "--", "sh", "-c", &show("\\033[?2004h")]);
    env.run(&["run", "--name", "plain", "--", "sh", "-c", &show("")]);
    env.wait_for_text("bracketed", "READY");
    env.wait_for_text("plain", "READY");
    env.run(&["buffer", "paste", "word", "-p", "bracketed"]);
    env.wait_for_text("bracketed", "^[[200~pasted-word^[[201~");
    env.run(&["buffer", "paste", "1", "-p", "plain"]);
    let screen = env.wait_for_text("plain", "echo first^M");
    assert!(!screen.contains("200~"), "{screen}");

    env.run(&["buffer", "delete", "notes"]);
    assert!(!buffer_list(&env).iter().any(|b| b["buffer"] == "notes"));
    env.run(&["buffer", "delete", "--all"]);
    assert!(buffer_list(&env).is_empty());
}

#[test]
fn paste_buffers_stay_in_memory_unless_kept_on_purpose() {
    let env = Env::new("bufkeep");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let file = env.root.join("state/kraal/buffers").join(format!("{}.json", env.session));
    assert!(set_buffer(&env, &["secret"], "hunter2").status.success());
    env.run(&["kill"]);
    assert!(!file.exists(), "written to disk unasked");
    env.run(&["open", "w"]);
    assert!(buffer_list(&env).is_empty(), "came back after a restart");

    // Kept on purpose: in a file only its owner reads, and back after a restart.
    let path = env.root.join("config/kraal/config.toml");
    let base = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("{base}[buffers]\npersist = true\n")).unwrap();
    env.run(&["config", "reload"]);
    assert!(set_buffer(&env, &["secret"], "hunter2").status.success());
    let start = Instant::now();
    while !file.exists() {
        assert!(start.elapsed() < Duration::from_secs(5), "never written");
        std::thread::sleep(Duration::from_millis(50));
    }
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    env.run(&["kill"]);
    env.run(&["open", "w"]);
    assert_eq!(env.run(&["buffer", "show", "secret"]), "hunter2");
    // Turned off again: the file goes.
    std::fs::write(&path, &base).unwrap();
    env.run(&["config", "reload"]);
    wait_gone(&file);
    assert!(!file.exists());
    assert_eq!(env.run(&["buffer", "show", "secret"]), "hunter2", "still there until the server stops");
}

#[test]
fn paste_buffers_that_could_not_be_kept_are_written_later() {
    let env = Env::new("buffail");
    let config = env.root.join("config/kraal/config.toml");
    let base = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{base}[buffers]\npersist = true\n")).unwrap();
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    // A file where the buffers' directory goes: nothing can be written there.
    let dir = env.root.join("state/kraal/buffers");
    std::fs::write(&dir, "in the way").unwrap();
    assert!(set_buffer(&env, &["notes"], "kept-later").status.success());
    // Long enough for the write to be tried, and to fail.
    std::thread::sleep(Duration::from_millis(1500));
    std::fs::remove_file(&dir).unwrap();
    env.run(&["kill"]);
    env.run(&["open", "w"]);
    assert_eq!(env.run(&["buffer", "show", "notes"]), "kept-later");
    // Still impossible when the server stops: said.
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::write(&dir, "in the way").unwrap();
    assert!(set_buffer(&env, &["notes"], "changed").status.success());
    let out = env.try_run(&["kill"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("the paste buffers could not be saved"), "{err}");
}

#[test]
fn copies_of_the_paste_buffers_a_killed_server_was_writing_go() {
    let env = Env::new("buftemp");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    // What a server killed while it wrote left: temporary copies, named after its pid.
    let mut gone = Command::new("true").spawn().unwrap();
    let dead = gone.id();
    gone.wait().unwrap();
    let (buffers, sessions) = (env.root.join("state/kraal/buffers"), env.root.join("state/kraal/sessions-v2"));
    let temp = |dir: &Path, session: &str| dir.join(format!(".{session}.json.{dead}.0.tmp"));
    for dir in [&buffers, &sessions] {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(temp(dir, "buftemp"), "hunter2").unwrap();
    }
    std::fs::write(temp(&buffers, "other"), "another session's").unwrap();
    // `kraal forget` deletes them with the rest of the session, and nothing of another.
    env.run(&["forget"]);
    assert!(!temp(&buffers, "buftemp").exists() && !temp(&sessions, "buftemp").exists());
    assert!(temp(&buffers, "other").exists());
    // A server that starts deletes all of them, buffers kept or not.
    env.run(&["open", "w"]);
    wait_gone(&temp(&buffers, "other"));
    assert!(!temp(&buffers, "other").exists());
}

#[test]
fn a_paste_buffer_goes_whole_or_not_at_all() {
    let env = Env::new("bufbig");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let got = env.root.join("pasted");
    let script = format!("stty raw -echo; printf READY; exec cat > {}", got.display());
    env.run(&["run", "--name", "sink", "--", "sh", "-c", &script]);
    env.wait_for_text("sink", "READY");
    // Within the buffers' 32 MB, more than a program can be sent at once (16 MiB).
    assert!(set_buffer(&env, &["big"], &"y".repeat(17 << 20)).status.success());
    let out = env.try_run(&["buffer", "paste", "big", "-p", "sink"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("at most"), "{err}");
    // What fits goes.
    assert!(set_buffer(&env, &["small"], "fits").status.success());
    env.run(&["buffer", "paste", "small", "-p", "sink"]);
    let size = || std::fs::metadata(&got).map(|m| m.len()).unwrap_or(0);
    let start = Instant::now();
    while size() < 4 {
        assert!(start.elapsed() < Duration::from_secs(10), "the small paste never arrived");
        std::thread::sleep(Duration::from_millis(50));
    }
    // Chosen in the list: said too.
    env.run(&["focus", "sink"]);
    let mut client = RawClient::attach(&env, "w");
    client.send(b"\x02=");
    client.wait_for_screen("Paste buffers");
    client.send(b"\x1b[B");
    std::thread::sleep(Duration::from_millis(150));
    client.send(b"\r");
    client.wait_for_screen("at most now");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(size(), 4, "nothing of the large paste went");
}

#[test]
fn paste_buffers_reach_production_only_on_purpose() {
    let env = Env::new("bufprod");
    let tf = env.root.join("infra");
    std::fs::create_dir_all(tf.join(".terraform")).unwrap();
    std::fs::write(tf.join(".terraform/environment"), "prod\n").unwrap();
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let script = "stty raw -echo; printf READY; exec cat -v";
    env.run(&["run", "--cwd", &tf.display().to_string(), "--name", "ops", "--", "sh", "-c", script]);
    wait_production(&env, "ops");
    env.wait_for_text("ops", "READY");
    assert!(set_buffer(&env, &[], "rm -rf /tmp/x\nreboot\n").status.success());
    // From the command line: refused unless said.
    let out = env.try_run(&["buffer", "paste", "-p", "ops"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("is production") && err.contains("--production"), "{err}");
    std::thread::sleep(Duration::from_millis(200));
    assert!(!env.run(&["read", "-p", "ops"]).contains("reboot"));
    // From the list: the question a paste of several lines into production gets.
    env.run(&["focus", "ops"]);
    let mut client = RawClient::attach(&env, "w");
    client.send(b"\x02=");
    client.wait_for_screen("Paste buffers");
    client.send(b"\r");
    client.wait_for_screen("Paste 2 lines into production");
    std::thread::sleep(Duration::from_millis(200));
    assert!(!env.run(&["read", "-p", "ops"]).contains("reboot"), "pasted before the answer");
    client.send(b"y");
    env.wait_for_text("ops", "rm -rf /tmp/x^Mreboot^M");
    // On purpose from the command line.
    assert!(set_buffer(&env, &[], "said-so").status.success());
    env.run(&["buffer", "paste", "-p", "ops", "--production"]);
    env.wait_for_text("ops", "said-so");
}

#[test]
fn the_paste_buffers_list_pastes_shows_and_deletes() {
    let env = Env::new("bufpick");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    env.run(&["run", "--name", "src", "--", "sh", "-c", "echo COPY-ME-LINE; exec sleep 60"]);
    env.wait_for_text("src", "COPY-ME-LINE");
    env.run(&["run", "--name", "sink", "--", "sh", "-c", "stty raw -echo; printf READY; exec cat -v"]);
    env.wait_for_text("sink", "READY");
    env.run(&["focus", "src"]);
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_screen("COPY-ME-LINE");
    // Copy mode sends one structured copy to the client and keeps the text in buffers.
    for key in [&b"\x02v"[..], b"k", b"V", b"y"] {
        client.send(key);
        std::thread::sleep(Duration::from_millis(150));
    }
    client.wait_for_message("\"type\":\"clipboard\"");
    assert!(env.run(&["buffer", "show"]).contains("COPY-ME-LINE"));
    assert!(set_buffer(&env, &[], "older-text").status.success());
    assert!(set_buffer(&env, &[], "newest-text").status.success());

    // Enter pastes the newest into the focused pane; typing filters.
    env.run(&["focus", "sink"]);
    client.send(b"\x02=");
    client.wait_for_screen("Paste buffers");
    client.send(b"\r");
    env.wait_for_text("sink", "READYnewest-text");
    client.send(b"\x02V");
    std::thread::sleep(Duration::from_millis(150));
    client.send(b"p");
    client.wait_for_screen("older-text");
    client.send(b"older");
    std::thread::sleep(Duration::from_millis(150));
    client.send(b"\r");
    env.wait_for_text("sink", "newest-textolder-text");

    // Tab lists what else can be done; ctrl+o shows the buffer in a window.
    client.send(b"\x02=");
    std::thread::sleep(Duration::from_millis(150));
    client.send(b"\t");
    client.wait_for_screen("Show in a preview pane");
    // Escape is the key once the server has waited for what may follow it: a key that reached
    // it sooner, on a machine that took its time to read the Escape, would be Alt with that key.
    client.send(b"\x1b");
    client.wait_for_rows("back at the list", |rows| {
        rows.iter().any(|r| r.contains("Paste buffers")) && !rows.iter().any(|r| r.contains("Show in a preview pane"))
    });
    client.send(b"\x0f");
    env.wait_for_text("buffer 0", "newest-text");
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let shown = panes.as_array().unwrap().iter().find(|p| p["name"] == "buffer 0").cloned().unwrap();
    assert_eq!((shown["kind"].as_str(), shown["floating"].as_bool()), (Some("preview:text"), Some(true)));
    // Nothing of it is saved with the session.
    env.run(&["kill"]);
    let state = std::fs::read_to_string(env.state_file()).unwrap();
    assert!(state.contains("\"sink\"") && !state.contains("buffer 0") && !state.contains("newest-text"), "{state}");
}

#[test]
fn the_paste_buffers_list_deletes_and_stays_open() {
    let env = Env::new("bufdel");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    for text in ["first-text", "second-text", "third-text"] {
        assert!(set_buffer(&env, &[], text).status.success());
    }
    let mut client = RawClient::attach(&env, "w");
    client.send(b"\x02=");
    client.wait_for_screen("Paste buffers");
    // ctrl+x deletes the selected buffer (the newest), and the list stays to go on.
    client.send(b"\x18");
    let start = Instant::now();
    while buffer_list(&env).len() != 2 {
        assert!(start.elapsed() < Duration::from_secs(5), "not deleted");
        std::thread::sleep(Duration::from_millis(50));
    }
    client.send(b"\x1b[B");
    std::thread::sleep(Duration::from_millis(150));
    client.send(b"\x18");
    let start = Instant::now();
    while buffer_list(&env).len() != 1 {
        assert!(start.elapsed() < Duration::from_secs(5), "the list did not stay");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(env.run(&["buffer", "show"]), "second-text");
    client.send(b"\x1b");
}
