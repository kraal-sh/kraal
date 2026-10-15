use super::support::{PATIENCE, RawClient, raw_frame};
use crate::common::{Env, wait_until};

fn advertise(client: &mut RawClient, id: &str) {
    let message = serde_json::json!({
        "type": "attach", "version": 5, "cols": 120, "rows": 40,
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
    owner.send(b"c");
    if replace {
        owner.wait_for_row("It replaces");
        assert!(!owner.messages.lock().unwrap().iter().any(|m| m.contains("laptop_copy")));
        owner.send(b"\t\r");
    }
    wait_until(PATIENCE, "laptop copy job requests the selected paths", || {
        owner.messages.lock().unwrap().iter().any(|m| m.contains("\"Start\""))
    });
    let start = owner
        .messages
        .lock()
        .unwrap()
        .iter()
        .filter_map(|m| serde_json::from_str::<serde_json::Value>(m).ok())
        .find(|m| m["type"] == "laptop_copy" && m["message"]["Start"].is_object())
        .unwrap();
    assert_eq!(start["message"]["Start"]["sources"], serde_json::json!(["~/report"]));
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
