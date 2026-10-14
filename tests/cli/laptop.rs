use super::support::{PATIENCE, RawClient, raw_frame};
use crate::common::{Env, wait_until};

fn advertise(client: &mut RawClient, id: &str) {
    let message = serde_json::json!({
        "type": "attach", "version": 4, "cols": 120, "rows": 40,
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
