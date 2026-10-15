use super::support::{PATIENCE, RawClient, raw_frame};
use crate::common::{Env, wait_until};

fn advertise(client: &mut RawClient, id: &str) {
    let message = serde_json::json!({
        "type": "attach", "version": 7, "cols": 120, "rows": 40,
        "workspace": "w", "remote": true,
        "laptop": { "id": id, "host": "travel", "os": "linux" }
    });
    raw_frame(&mut client.stream, 0, message.to_string().as_bytes());
}

#[test]
fn laptop_listing_is_private_and_disconnect_and_save_keep_a_usable_session() {
    let env = Env::new("laptop-list");
    env.workspace("w", "layout { pane name=\"shell\" }\n");
    env.run(&["open", "w"]);
    let mut owner = RawClient::attach(&env, "w");
    advertise(&mut owner, "abc123");
    let observer = RawClient::attach(&env, "w");
    env.run(&["files"]);
    owner.wait_for_row("files");
    owner.send(b"o");
    owner.wait_for_row("this laptop");
    owner.send(b"travel\r");
    owner.wait_for_message("laptop_list");
    let request: serde_json::Value = owner
        .messages
        .lock()
        .unwrap()
        .iter()
        .filter_map(|m| serde_json::from_str::<serde_json::Value>(m).ok())
        .find(|m| m["type"] == "laptop_list")
        .unwrap();
    let pane = request["pane"].to_string();
    assert_eq!(request["path"], "~");
    assert!(!observer.messages.lock().unwrap().iter().any(|m| m.contains("laptop_list")));
    let response = serde_json::json!({
        "type": "laptop_listed", "pane": request["pane"], "side": request["side"], "request": request["request"],
        "result": { "Ok": ["~/private-path", [{"name":"secret-entry", "is_dir":false, "is_link":false, "size":7, "mtime":0, "exact":true}]] }
    });
    raw_frame(&mut owner.stream, 0, response.to_string().as_bytes());
    owner.wait_for_row("secret-entry");
    let rows = observer.wait_for_row("shown on that machine only");
    assert!(!rows.iter().any(|r| r.contains("secret-entry") || r.contains("private-path")));
    let read = env.run(&["read", "-p", &pane]);
    assert!(read.contains("shown on that machine only"));
    assert!(!read.contains("secret-entry") && !read.contains("private-path"));
    drop(owner);
    observer.wait_for_row("travel is not attached");
    let mut returning = RawClient::attach(&env, "w");
    advertise(&mut returning, "abc123");
    returning.wait_for_message("laptop_list");
    drop(returning);
    let mut fresh = RawClient::attach(&env, "w");
    advertise(&mut fresh, "def456");
    fresh.wait_for_row("travel is not attached");
    env.run(&["kill"]);
    wait_until(PATIENCE, "session saved", || env.state_file().exists());
    let saved = std::fs::read_to_string(env.state_file()).unwrap();
    assert!(!saved.contains("private-path") && !saved.contains("secret-entry") && !saved.contains("abc123"));
    env.run(&["open", "w"]);
    let panes = env.run(&["panes", "--json"]);
    assert!(panes.contains("shell"), "{panes}");
}

fn laptop_copy(env: &Env, destination: &std::path::Path, replace: bool) -> (RawClient, String) {
    laptop_action(env, destination, replace, "Start")
}

fn laptop_action(env: &Env, destination: &std::path::Path, replace: bool, action: &str) -> (RawClient, String) {
    env.workspace("w", "layout { pane name=\"shell\" }\n");
    env.run(&["open", "w"]);
    let mut owner = RawClient::attach(env, "w");
    advertise(&mut owner, "abc123");
    env.run(&["files", "~", destination.to_str().unwrap()]);
    owner.wait_for_row("files");
    owner.send(b"o");
    owner.wait_for_row("this laptop");
    owner.send(b"travel\r");
    owner.wait_for_message("laptop_list");
    let request = owner
        .messages
        .lock()
        .unwrap()
        .iter()
        .filter_map(|m| serde_json::from_str::<serde_json::Value>(m).ok())
        .find(|m| m["type"] == "laptop_list")
        .unwrap();
    let response = serde_json::json!({
        "type": "laptop_listed", "pane": request["pane"], "side": request["side"], "request": request["request"],
        "result": { "Ok": ["~", [{"name":"report", "is_dir":false, "is_link":false, "size":3, "mtime":0, "exact":true}]] }
    });
    raw_frame(&mut owner.stream, 0, response.to_string().as_bytes());
    owner.wait_for_row("report");
    owner.send(match action {
        "Move" => b"m",
        "Delete" => b"d",
        _ => b"c",
    });
    if replace {
        owner.wait_for_row("It replaces");
        assert!(!owner.messages.lock().unwrap().iter().any(|m| m.contains("laptop_copy")));
        owner.send(b"\t\r");
    }
    wait_until(PATIENCE, "laptop copy job requests the selected paths", || {
        owner.messages.lock().unwrap().iter().any(|m| {
            let m: serde_json::Value = serde_json::from_str(m).unwrap();
            m["message"][action].is_object()
        })
    });
    let start = owner
        .messages
        .lock()
        .unwrap()
        .iter()
        .filter_map(|m| serde_json::from_str::<serde_json::Value>(m).ok())
        .find(|m| m["type"] == "laptop_copy" && m["message"][action].is_object())
        .unwrap();
    assert_eq!(start["message"][action]["sources"], serde_json::json!(["~/report"]));
    (owner, start["token"].as_str().unwrap().into())
}

fn copy_packet(owner: &mut RawClient, token: &str, sequence: u64, packet: serde_json::Value) {
    let message = serde_json::json!({ "type": "laptop_copy", "token": token, "message": { "Data": { "sequence": sequence, "packet": packet } } });
    raw_frame(&mut owner.stream, 0, message.to_string().as_bytes());
    wait_until(PATIENCE, "copy chunk acknowledged after writing", || {
        owner.messages.lock().unwrap().iter().any(|m| {
            let m: serde_json::Value = serde_json::from_str(m).unwrap();
            m["type"] == "laptop_copy" && m["token"] == token && m["message"]["Ack"] == sequence
        })
    });
}

fn send_report(owner: &mut RawClient, token: &str) {
    copy_packet(
        owner,
        token,
        1,
        serde_json::json!({ "Entry": {"path":"report","kind":"File","size":3,"mode":420,"seconds":1600000000,"nanos":0} }),
    );
    copy_packet(owner, token, 2, serde_json::json!({"Chunk":"YWJj"}));
    copy_packet(owner, token, 3, serde_json::json!("End"));
    copy_packet(owner, token, 4, serde_json::json!("Done"));
}

fn copy_finished(owner: &RawClient, token: &str) -> serde_json::Value {
    let mut result = None;
    wait_until(PATIENCE, "copy reports a final result", || {
        result = owner
            .messages
            .lock()
            .unwrap()
            .iter()
            .filter_map(|m| serde_json::from_str::<serde_json::Value>(m).ok())
            .find(|m| m["token"] == token && m["message"]["Finish"].is_object());
        result.is_some()
    });
    result.unwrap()["message"]["Finish"].clone()
}

#[test]
fn laptop_copy_installs_via_the_job_confirms_replacements_and_keeps_racing_destinations() {
    for outcome in ["new", "replace", "race"] {
        let env = Env::new(&format!("laptop-copy-{outcome}"));
        let destination = env.root.join("destination");
        std::fs::create_dir(&destination).unwrap();
        if outcome == "replace" {
            std::fs::write(destination.join("report"), b"old").unwrap();
        }
        let (mut owner, token) = laptop_copy(&env, &destination, outcome == "replace");
        copy_packet(&mut owner, &token, 0, serde_json::json!({"Begin":{"bytes":3,"entries":1}}));
        if outcome == "race" {
            std::fs::write(destination.join("report"), b"racing file").unwrap();
        }
        send_report(&mut owner, &token);
        let result = copy_finished(&owner, &token);
        if outcome == "race" {
            assert!(result["Err"].as_str().unwrap().contains("not fully installed"));
            assert_eq!(std::fs::read(destination.join("report")).unwrap(), b"racing file");
        } else {
            assert!(result.get("Ok").is_some(), "{result}");
            assert_eq!(std::fs::read(destination.join("report")).unwrap(), b"abc");
        }
        let spool = env.root.join("state/kraal/laptop-v1").join(&env.session).join(&token);
        wait_until(PATIENCE, "private spool removed", || !spool.exists());
    }
}

#[test]
fn laptop_move_installs_before_source_removal_and_refreshes_after_completion() {
    for changed in [false, true] {
        let env = Env::new(if changed { "laptop-move-changed" } else { "laptop-move" });
        let destination = env.root.join("destination");
        std::fs::create_dir(&destination).unwrap();
        let (mut owner, token) = laptop_action(&env, &destination, false, "Move");
        copy_packet(&mut owner, &token, 0, serde_json::json!({"Begin":{"bytes":3,"entries":1}}));
        send_report(&mut owner, &token);
        wait_until(PATIENCE, "source removal requested after installation", || {
            owner.messages.lock().unwrap().iter().any(|m| {
                let m: serde_json::Value = serde_json::from_str(m).unwrap();
                m["token"] == token && m["message"] == "Installed"
            })
        });
        assert_eq!(std::fs::read(destination.join("report")).unwrap(), b"abc");
        assert!(!owner.messages.lock().unwrap().iter().any(|m| {
            let m: serde_json::Value = serde_json::from_str(m).unwrap();
            m["token"] == token && m["message"]["Finish"].is_object()
        }));
        let result = if changed {
            serde_json::json!({"Err":"Source changed: report was kept. Check both locations before retrying"})
        } else {
            serde_json::json!({"Ok":null})
        };
        let message = serde_json::json!({"type":"laptop_copy", "token":token, "message":{"Finish":result}});
        raw_frame(&mut owner.stream, 0, message.to_string().as_bytes());
        assert_eq!(copy_finished(&owner, &token), result);
        wait_until(PATIENCE, "laptop source panel refreshes", || {
            owner.messages.lock().unwrap().iter().filter(|m| m.contains("laptop_list")).count() >= 2
        });
        assert_eq!(std::fs::read(destination.join("report")).unwrap(), b"abc");
    }
}

#[test]
fn laptop_delete_job_waits_for_local_result_and_refreshes_the_source_panel() {
    for cancelled in [false, true] {
        let env = Env::new(if cancelled { "laptop-delete-cancel" } else { "laptop-delete" });
        let destination = env.root.join("destination");
        std::fs::create_dir(&destination).unwrap();
        let (mut owner, token) = laptop_action(&env, &destination, false, "Delete");
        let result = if cancelled {
            serde_json::json!({"Err":"Operation cancelled: nothing was removed"})
        } else {
            serde_json::json!({"Ok":null})
        };
        let message = serde_json::json!({"type":"laptop_copy", "token":token, "message":{"Finish":result}});
        raw_frame(&mut owner.stream, 0, message.to_string().as_bytes());
        assert_eq!(copy_finished(&owner, &token), result);
        wait_until(PATIENCE, "laptop panel refreshes after deletion", || {
            owner.messages.lock().unwrap().iter().filter(|m| m.contains("laptop_list")).count() >= 2
        });
    }
}

#[test]
fn laptop_copy_window_close_and_disconnect_remove_partial_spools_without_installing() {
    for outcome in ["close", "disconnect"] {
        let env = Env::new(&format!("laptop-copy-{outcome}"));
        let destination = env.root.join("destination");
        std::fs::create_dir(&destination).unwrap();
        let (mut owner, token) = laptop_copy(&env, &destination, false);
        copy_packet(&mut owner, &token, 0, serde_json::json!({"Begin":{"bytes":3,"entries":1}}));
        copy_packet(
            &mut owner,
            &token,
            1,
            serde_json::json!({ "Entry": {"path":"report","kind":"File","size":3,"mode":420,"seconds":1600000000,"nanos":0} }),
        );
        let spool = env.root.join("state/kraal/laptop-v1").join(&env.session).join(&token);
        assert!(spool.join("payload/report").exists());
        if outcome == "close" {
            let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
            let job = panes.as_array().unwrap().iter().find(|p| p["name"].as_str() == Some("⇅ copy")).unwrap();
            env.run(&["close", &job["id"].to_string()]);
            assert!(copy_finished(&owner, &token)["Err"].as_str().unwrap().contains("cancelled"));
        } else {
            drop(owner);
        }
        wait_until(PATIENCE, "cancelled spool removed", || !spool.exists());
        assert!(!destination.join("report").exists());
    }
}

#[test]
fn session_copy_to_laptop_sends_only_after_client_readiness_and_refuses_source_link_races() {
    for outcome in ["copy", "race", "move", "changed", "failed"] {
        let race = outcome == "race";
        let moving = matches!(outcome, "move" | "changed" | "failed");
        let env = Env::new(&format!("laptop-write-{outcome}"));
        let source = env.root.join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("report"), b"abc").unwrap();
        // Like /tmp on macOS, the directory shown by the file manager can be a link.
        let linked = env.root.join("linked-source");
        std::os::unix::fs::symlink(&source, &linked).unwrap();
        let secret = env.root.join("secret");
        std::fs::write(&secret, b"not transferred").unwrap();
        env.workspace("w", "layout { pane name=\"shell\" }\n");
        env.run(&["open", "w"]);
        let mut owner = RawClient::attach(&env, "w");
        advertise(&mut owner, "abc123");
        env.run(&["files", linked.to_str().unwrap(), "~"]);
        owner.wait_for_row("report");
        owner.send(b"\to");
        owner.wait_for_row("this laptop");
        owner.send(b"travel\r");
        owner.wait_for_message("laptop_list");
        let request = owner
            .messages
            .lock()
            .unwrap()
            .iter()
            .filter_map(|m| serde_json::from_str::<serde_json::Value>(m).ok())
            .find(|m| m["type"] == "laptop_list")
            .unwrap();
        let response = serde_json::json!({
            "type":"laptop_listed", "pane":request["pane"], "side":request["side"], "request":request["request"],
            "result":{"Ok":["~",[{"name":"report", "is_dir":false, "is_link":false, "size":1, "mtime":0, "exact":true}]]}
        });
        raw_frame(&mut owner.stream, 0, response.to_string().as_bytes());
        owner.send(if moving { b"\t\x1b[Bm" } else { b"\t\x1b[Bc" });
        if moving {
            let question = format!("Move {} to travel:~?", linked.join("report").display());
            let rows = owner.wait_for_row("Enter Cancel · Tab Choose · Esc Cancel");
            // A long source path puts the question in the body, wrapped between words.
            if !rows.iter().any(|row| row.contains(&question)) {
                assert_eq!(owner.read_question(b"\x1b[6~").join(" "), question);
            }
            owner.send(b"\t\r");
        }
        let mut write = None;
        wait_until(PATIENCE, "copy asks the laptop client to prepare its destination", || {
            write = owner
                .messages
                .lock()
                .unwrap()
                .iter()
                .filter_map(|m| serde_json::from_str::<serde_json::Value>(m).ok())
                .find(|m| m["message"]["Write"].is_object());
            write.is_some()
        });
        let write = write.unwrap();
        let token = write["token"].as_str().unwrap();
        assert_eq!(write["message"]["Write"]["destination"], "~");
        assert_eq!(
            write["message"]["Write"]["allow"],
            serde_json::json!([]),
            "the server cannot approve replacements on the laptop"
        );
        assert!(!owner.messages.lock().unwrap().iter().any(|m| m.contains("\"Data\"")));
        let ack = |owner: &mut RawClient, sequence| {
            let message = serde_json::json!({"type":"laptop_copy", "token":token, "message":{"Ack":sequence}});
            raw_frame(&mut owner.stream, 0, message.to_string().as_bytes());
        };
        ack(&mut owner, 0);
        let mut content = Vec::new();
        for sequence in 0..5 {
            let mut data = None;
            wait_until(PATIENCE, "job sends a bounded packet", || {
                data = owner
                    .messages
                    .lock()
                    .unwrap()
                    .iter()
                    .filter_map(|m| serde_json::from_str::<serde_json::Value>(m).ok())
                    .find(|m| {
                        m["token"] == token
                            && (m["message"]["Data"]["sequence"] == sequence || m["message"]["Finish"].is_object())
                    });
                data.is_some()
            });
            let data = data.unwrap();
            assert!(data["message"]["Data"].is_object(), "copy ended before packet {sequence}: {data}");
            let packet = &data["message"]["Data"]["packet"];
            if sequence == 0 {
                assert_eq!(packet["Begin"]["bytes"], 3);
                if race {
                    std::fs::remove_file(source.join("report")).unwrap();
                    std::os::unix::fs::symlink(&secret, source.join("report")).unwrap();
                }
            }
            if let Some(chunk) = packet["Chunk"].as_str() {
                use base64::Engine;
                content.extend(base64::engine::general_purpose::STANDARD.decode(chunk).unwrap());
            }
            ack(&mut owner, sequence);
            if race {
                break;
            }
        }
        if race {
            assert!(copy_finished(&owner, token).get("Err").is_some());
            assert!(content.is_empty());
        } else {
            assert_eq!(content, b"abc");
            assert_eq!(std::fs::read(source.join("report")).unwrap(), b"abc", "source stays until installation");
            if outcome == "changed" {
                std::fs::write(source.join("report"), b"changed").unwrap();
            }
            let installed = if outcome == "failed" {
                serde_json::json!({"Err":"Installation failed: source kept"})
            } else {
                serde_json::json!({"Ok":null})
            };
            let message = serde_json::json!({"type":"laptop_copy", "token":token, "message":{"Finish":installed}});
            raw_frame(&mut owner.stream, 0, message.to_string().as_bytes());
            let result = copy_finished(&owner, token);
            assert_eq!(result.get("Ok").is_some(), matches!(outcome, "copy" | "move"), "{result}");
            if outcome == "move" {
                assert!(!source.join("report").exists());
            } else if outcome == "changed" {
                assert_eq!(std::fs::read(source.join("report")).unwrap(), b"changed");
                assert!(result["Err"].as_str().unwrap().contains("Source changed"));
            } else {
                assert_eq!(std::fs::read(source.join("report")).unwrap(), b"abc");
            }
        }
        assert_eq!(std::fs::read(secret).unwrap(), b"not transferred");
    }
}
