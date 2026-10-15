use super::support::{PATIENCE, RawClient, click_account_text};
use crate::common::{Env, wait_until, write_script};
use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// Saves never take another workspace's source. Other existing files need a confirmation
/// which preserves the input on Cancel and expires when the file changes again.
#[test]
fn workspace_creation_failures_keep_input_and_retry_from_both_entries_at_both_sizes() {
    let env = Env::new("wscreateerrors");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    let shell = env.root.join("sh");
    let held = env.root.join("sh-held");
    for size in [(80, 24), (124, 36)] {
        for picker in [false, true] {
            let name = format!("scratch-{}-{picker}", size.0);
            let before = listed_workspaces(&env);
            let panes = env.run(&["panes"]);
            let mut client = RawClient::attach_sized(&env, Some("w"), size);
            client.wait_for_message("\"attached\"");
            std::fs::rename(&shell, &held).unwrap();
            if picker {
                client.send(b"\x02w");
                client.wait_for_row("Workspaces");
            } else {
                client.send(b"\x02Wn");
                client.wait_for_row("Name    ");
            }
            if picker {
                client.send(name.as_bytes());
                let rows = client.wait_for_row("Enter Add");
                assert!(!rows.iter().any(|r| r.contains("Tab Actions")));
                client.send(b"\r");
                client.wait_for_row("Name    ");
                assert_eq!(listed_workspaces(&env), before);
            }
            client.send(format!("\x01\x0b {name} ").as_bytes());
            client.send(b"\r");
            let rows = client.wait_for_row("Enter Retry");
            for text in [&name, "Add workspace", "Couldn't start the shell.", "No workspace was added.", "Esc Cancel"] {
                assert!(rows.iter().any(|r| r.contains(text)), "{size:?}: {}", rows.join("\n"));
            }
            assert_eq!(listed_workspaces(&env), before);
            assert_eq!(env.run(&["panes"]), panes);
            // Cursor movement keeps the error; an edit clears it and keeps the cursor's place.
            client.send(b"\x1b[D");
            client.settle();
            assert!(client.rows().iter().any(|r| r.contains("Enter Retry")));
            client.send(b"-fixed");
            client.wait_for_row("Enter Add");
            let fixed = format!("{name}-fixed");
            client.send(b"\r");
            client.wait_for_row("Enter Retry");
            assert_eq!(listed_workspaces(&env), before);
            std::fs::rename(&held, &shell).unwrap();
            client.send(b"\r");
            client.wait_for_rows("the creation prompt closed", |rows| !rows.iter().any(|r| r.contains("Name    ")));
            let workspaces = listed_workspaces(&env);
            assert_eq!(workspaces.len(), before.len() + 1);
            assert_eq!(workspaces.iter().filter(|w| *w == &format!("{fixed}:  (open)")).count(), 1);
            assert_eq!(env.run(&["panes"]).lines().count(), panes.lines().count() + 1);
        }
    }
}

#[test]
fn workspace_search_add_and_closed_rename_work_at_both_sizes() {
    for size in [(80, 24), (124, 36)] {
        let env = Env::new(&format!("ws-list-{}", size.0));
        let folder = env.root.join("project");
        std::fs::create_dir(&folder).unwrap();
        let source = format!("layout {{ cwd {:?}; pane name=\"shell\"; }}\n", folder.display().to_string());
        env.workspace("source", &source);
        env.run(&["open", "source"]);
        let path = env.root.join("config/kraal/workspaces/source.kdl");
        let mut client = RawClient::attach_sized(&env, Some("source"), size);
        client.wait_for_message("\"attached\"");
        client.wait_for_row("$ ");
        if size.0 == 80 {
            click_account_text(&mut client, "source");
        } else {
            click_account_text(&mut client, "WORKSPACES");
        }
        client.wait_for_row("Search");
        client.send(b"billing");
        client.wait_for_row("No matching workspaces.");
        assert_eq!(listed_workspaces(&env), ["source: source.kdl (open)"]);
        click_account_text(&mut client, "+ Add workspace");
        client.wait_for_row("Name    billing");
        client.wait_for_row("Starts in");
        assert_eq!(listed_workspaces(&env), ["source: source.kdl (open)"]);
        client.send(b"\x1b");
        client.wait_for_row("Search  billing");
        client.send(b"\r");
        client.wait_for_row("Name    billing");
        click_account_text(&mut client, " Add ");
        client.wait_for_row("Workspace added");
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let added = panes.as_array().unwrap().iter().find(|p| p["workspace"] == "billing").unwrap();
        assert_eq!(Path::new(added["cwd"].as_str().unwrap()).canonicalize().unwrap(), folder.canonicalize().unwrap());
        client.wait_for_row("$ ");
        client.send(b"\x02Wx");
        client.wait_for_row("Closed · 1 tab · 1 pane");
        let before = env.run(&["panes", "--json"]);
        let list: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
        let id = list.as_array().unwrap().iter().find(|w| w["name"] == "billing").unwrap()["id"].clone();
        client.send(b"\t");
        client.wait_for_row("Rename");
        client.send(b"\x1b[B\r");
        client.wait_for_row("Rename workspace");
        client.send(b"\x01\x0bbilling/review\r");
        client.wait_for_row("Workspace renamed");
        assert_eq!(env.run(&["panes", "--json"]), before, "renaming a closed workspace starts no programs");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        assert!(!path.with_extension("kdl.bak").exists());
        env.run(&["kill"]);
        env.run(&["open", "source"]);
        let list: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
        let renamed = list.as_array().unwrap().iter().find(|w| w["name"] == "billing/review").unwrap();
        assert_eq!(renamed["id"], id);
        assert_eq!(renamed["open"], false);
        assert_eq!(std::fs::read_to_string(path).unwrap(), source);
    }
}

#[test]
fn workspace_duplicate_is_saved_closed_and_starts_commands_only_on_enter() {
    for size in [(80, 24), (124, 36)] {
        let env = Env::new(&format!("ws-duplicate-{}", size.0));
        let bin = env.root.join("bin");
        let account = env.root.join("agent-home");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::create_dir_all(&account).unwrap();
        write_script(
            &bin.join("claude"),
            r#"#!/bin/sh
[ "$2" = status ] && exit 0
mkdir -p "$CLAUDE_CONFIG_DIR/projects/work"
printf '{"message":1}\n' >> "$CLAUDE_CONFIG_DIR/projects/work/$2.jsonl"
if [ -e "$CLAUDE_CONFIG_DIR/split-launch" ]; then
    printf '%s' "$1" >> "$CLAUDE_CONFIG_DIR/launches"
    touch "$CLAUDE_CONFIG_DIR/partial-launch"
    while [ ! -e "$CLAUDE_CONFIG_DIR/finish-launch" ]; do sleep 0.01; done
    shift
    printf ' %s' "$@" >> "$CLAUDE_CONFIG_DIR/launches"
    printf '\n' >> "$CLAUDE_CONFIG_DIR/launches"
else
    printf '%s\n' "$*" >> "$CLAUDE_CONFIG_DIR/launches"
fi
echo original-agent-output
while IFS= read -r line; do :; done
"#,
        );
        std::fs::write(
            env.root.join("config/kraal/accounts.toml"),
            format!(
                "[[account]]\nid = \"test-claude-work\"\nname = \"work\"\nagent = \"claude\"\nhome = {:?}\n",
                account.display().to_string()
            ),
        )
        .unwrap();
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        let job = format!("echo job >> {}/jobs; echo original-job-output; exec sleep 100", env.root.display());
        let source = format!(
            r#"// Keep the recipe unchanged.
layout {{
    cwd {:?}
    tab name="work" focus=true {{
        pane name="shell" focus=true
        pane name="job" command="sh" {{ args "-c" {:?}; }}
    }}
    tab name="agent" {{ pane name="agent" agent="claude:work" account_id="test-claude-work" {{ args "--model" "opus"; }}; }}
}}
"#,
            env.root.display().to_string(),
            job
        );
        env.workspace("api", &source);
        env.workspace("api-copy", "layout { pane; }\n");
        let opened = env.cmd(&["open", "api"]).env("PATH", &path).output().unwrap();
        assert!(opened.status.success(), "{}", String::from_utf8_lossy(&opened.stderr));
        env.wait_for_text("job", "original-job-output");
        env.wait_for_text("agent", "original-agent-output");
        let original_launch = std::fs::read_to_string(account.join("launches")).unwrap();
        assert!(original_launch.contains("--session-id"));
        let original_id =
            serde_json::from_str::<serde_json::Value>(&env.run(&["workspaces", "--json"])).unwrap()[0]["id"].clone();
        let mut client = RawClient::attach_sized(&env, Some("api"), size);
        client.wait_for_row("$ ");
        // Output can arrive before the periodic scan recognises the agent.
        wait_until(PATIENCE, "the original agent was not detected as idle", || {
            let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
            panes
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["name"] == "agent" && p["agent"] == "claude" && p["state"] == "idle")
        });
        let original_panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        client.send(b"\x02wapi\t");
        click_account_text(&mut client, "Duplicate");
        client.wait_for_row("Name    api-copy-2");
        client.wait_for_row("Programs wait for you to start them.");
        client.send(b"\x1b");
        client.wait_for_row("Search  api");
        client.send(b"\t");
        click_account_text(&mut client, "Duplicate");
        client.wait_for_row("Name    api-copy-2");
        client.send(b"\r");
        client.wait_for_row("Workspace duplicated");
        let list: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
        let copy = list.as_array().unwrap().iter().find(|w| w["name"] == "api-copy-2").unwrap();
        assert_eq!(copy["open"], false);
        assert_eq!(copy["tabs"], 2);
        assert_eq!(copy["panes"], 3);
        assert_ne!(copy["id"], original_id);
        assert!(copy["definition"].is_null());
        assert_eq!(serde_json::from_str::<serde_json::Value>(&env.run(&["panes", "--json"])).unwrap(), original_panes);
        let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(env.state_file()).unwrap()).unwrap();
        assert!(saved["workspaces"].as_array().unwrap().iter().any(|w| w["id"] == copy["id"] && w["closed"] == true));
        env.run(&["kill"]);
        let opened = env.cmd(&["open", "api"]).env("PATH", &path).output().unwrap();
        assert!(opened.status.success());
        env.wait_for_text_since_restore("agent", "original-agent-output");
        let launches = std::fs::read_to_string(account.join("launches")).unwrap();
        let jobs = std::fs::read_to_string(env.root.join("jobs")).unwrap();
        let list: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
        let restarted_copy = list.as_array().unwrap().iter().find(|w| w["name"] == "api-copy-2").unwrap();
        assert_eq!(restarted_copy["id"], copy["id"]);
        assert_eq!(restarted_copy["open"], false);
        env.run(&["open", "api-copy-2"]);
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let current_source: Vec<_> = panes.as_array().unwrap().iter().filter(|p| p["workspace"] == "api").collect();
        let copied: Vec<_> = panes.as_array().unwrap().iter().filter(|p| p["workspace"] == "api-copy-2").collect();
        assert_eq!(copied.len(), 3);
        for p in &copied {
            assert!(current_source.iter().all(|original| original["id"] != p["id"]));
            if p["name"] != "shell" {
                assert_eq!(p["suspended"], true);
                let output = env.run(&["read", "-p", &p["id"].to_string()]);
                assert!(!output.contains("original-agent-output"));
            }
        }
        assert_eq!(std::fs::read_to_string(account.join("launches")).unwrap(), launches);
        assert_eq!(std::fs::read_to_string(env.root.join("jobs")).unwrap(), jobs);
        let shell = copied.iter().find(|p| p["name"] == "shell").unwrap();
        env.run(&["send", "-p", &shell["id"].to_string(), "--enter", "printf 'copy-shell-%s\\n' ready"]);
        env.wait_for_text(&shell["id"].to_string(), "copy-shell-ready");
        let agent = copied.iter().find(|p| p["name"] == "agent").unwrap();
        assert_eq!(agent["account_id"], current_source.iter().find(|p| p["name"] == "agent").unwrap()["account_id"]);
        let mut client = RawClient::attach_sized(&env, Some("api-copy-2"), size);
        client.wait_for_message("\"attached\"");
        client.send(format!("\x1b]7701;show;{}\x07", agent["id"]).as_bytes());
        client.wait_for_row("--session-id");
        client.wait_for_row("enter run");
        std::fs::write(account.join("split-launch"), "").unwrap();
        client.send(b"\r");
        wait_until(PATIENCE, "the copied agent did not begin writing its arguments", || {
            account.join("partial-launch").exists()
        });
        let mut all_launches = String::new();
        wait_until(PATIENCE, "the copied agent did not start", || {
            all_launches = crate::common::read_complete_lines(account.join("launches")).unwrap_or_default();
            // The first read sees an unterminated argument, before the stand-in may finish it.
            if !account.join("finish-launch").exists() {
                assert!(all_launches.is_empty(), "an unterminated launch is not ready");
                std::fs::write(account.join("finish-launch"), "").unwrap();
            }
            all_launches.lines().count() == launches.lines().count() + 1
        });
        std::fs::remove_file(account.join("split-launch")).unwrap();
        let copied_launch = all_launches.lines().last().unwrap();
        let copied_session = copied_launch.split_whitespace().nth(1).unwrap();
        assert_eq!(copied_launch, format!("--session-id {copied_session} --model opus"));
        assert_ne!(Some(copied_session), original_launch.split_whitespace().nth(1));
        assert_eq!(std::fs::read_to_string(env.root.join("jobs")).unwrap(), jobs);
        let job = copied.iter().find(|p| p["name"] == "job").unwrap();
        client.send(format!("\x1b]7701;show;{}\x07", job["id"]).as_bytes());
        client.wait_for_row("echo job");
        client.wait_for_row("enter run");
        client.send(b"\r");
        wait_until(PATIENCE, "the copied command did not start", || {
            std::fs::read_to_string(env.root.join("jobs")).unwrap().lines().count() == jobs.lines().count() + 1
        });
        let source_path = env.root.join("config/kraal/workspaces/api.kdl");
        assert_eq!(std::fs::read_to_string(&source_path).unwrap(), source);
        assert!(!source_path.with_extension("kdl.bak").exists());
        assert!(!env.root.join("config/kraal/workspaces/api-copy-2.kdl").exists());

        // The fake agent writes a transcript but never sends hooks. Close/Open and a server
        // restart must both recover the copy's conversation, without borrowing the source's.
        // The question lists the programs as the periodic scan knows them, and asks again when
        // that list changed under it: answer once the scan has recognised the copied agent.
        wait_until(PATIENCE, "the copied agent was not detected as idle", || {
            let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
            panes
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["id"] == agent["id"] && p["agent"] == "claude" && p["state"] == "idle")
        });
        client.send(b"\x02Wx");
        client.wait_for_row("The layout will be kept.");
        client.wait_for_row("claude: idle");
        client.send(b"y");
        client.wait_for_row("Closed · 2 tabs · 3 panes");
        env.run(&["open", "api-copy-2"]);
        let resumed = format!("--resume {copied_session} --model opus");
        wait_until(PATIENCE, "Close/Open did not resume the copied conversation without hooks", || {
            std::fs::read_to_string(account.join("launches")).unwrap().lines().filter(|l| *l == resumed).count() == 1
        });
        env.run(&["kill"]);
        let opened = env.cmd(&["open", "api-copy-2"]).env("PATH", &path).output().unwrap();
        assert!(opened.status.success());
        wait_until(PATIENCE, "server restart did not resume the copied conversation without hooks", || {
            std::fs::read_to_string(account.join("launches")).unwrap().lines().filter(|l| *l == resumed).count() == 2
        });
    }
}

#[test]
fn workspace_moves_keep_focus_share_the_list_and_survive_restart() {
    for size in [(80, 24), (124, 36)] {
        let env = Env::new(&format!("ws-order-{}", size.0));
        let docs = "// Keep this recipe.\nlayout { pane command=\"true\" start_suspended=true; }\n";
        env.workspace("api", "layout { pane name=\"api-shell\"; }\n");
        env.workspace("docs", docs);
        env.workspace("web", "layout { pane name=\"web-shell\"; }\n");
        for name in ["api", "docs", "web"] {
            env.run(&["open", name]);
        }
        let mut client = RawClient::attach_sized(&env, Some("docs"), size);
        client.wait_for_message("\"attached\"");
        client.send(b"\x02Wx");
        client.wait_for_row("Closed · 1 tab · 1 pane");
        let mut other = RawClient::attach_sized(&env, Some("web"), size);
        other.wait_for_row("$ ");
        other.send(b"\x02w");
        other.wait_for_row("Search");
        let panes = env.run(&["panes", "--json"]);
        client.send(b"docs\t");
        click_account_text(&mut client, "Clear search to reorder");
        assert_eq!(listed_workspaces(&env), ["api: api.kdl (open)", "docs: docs.kdl", "web: web.kdl (open)"]);
        client.send(b"\t");
        click_account_text(&mut client, "Move up");
        let reordered = ["docs: docs.kdl", "api: api.kdl (open)", "web: web.kdl (open)"];
        wait_until(PATIENCE, "workspace order was not saved", || listed_workspaces(&env) == reordered);
        other.wait_for_rows("the other client's list has the same order", |rows| {
            // From the right: the pane's border, then the list's, regardless of the sidebar.
            let names: Vec<_> = rows
                .iter()
                .filter_map(|row| {
                    let row = row.rsplit('│').nth(2)?.trim();
                    ["docs", "api", "web"].into_iter().find(|name| row.ends_with(name))
                })
                .collect();
            names.windows(3).any(|rows| rows == ["docs", "api", "web"])
        });
        assert_eq!(env.run(&["panes", "--json"]), panes, "moving a closed workspace starts no programs");
        client.close_overlay("Search");
        other.close_overlay("Search");
        client.send(b"printf 'focus-api\\n'\r");
        other.send(b"printf 'focus-web\\n'\r");
        env.wait_for_text("api-shell", "focus-api");
        env.wait_for_text("web-shell", "focus-web");
        assert!(!env.run(&["read", "-p", "api-shell"]).contains("focus-web"));
        assert!(!env.run(&["read", "-p", "web-shell"]).contains("focus-api"));
        let before: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
        env.run(&["kill"]);
        assert_eq!(listed_workspaces(&env), ["docs: docs.kdl", "api: api.kdl", "web: web.kdl"]);
        env.run(&["open", "api"]);
        assert_eq!(listed_workspaces(&env), reordered);
        let after: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
        for i in 0..3 {
            assert_eq!(after[i]["id"], before[i]["id"]);
        }
        assert_eq!(std::fs::read_to_string(env.root.join("config/kraal/workspaces/docs.kdl")).unwrap(), docs);
        assert!(!env.root.join("config/kraal/workspaces/docs.kdl.bak").exists());
    }
}

#[test]
fn workspace_delete_undo_and_restore_after_restart_keep_both_reused_names() {
    for size in [(80, 24), (124, 36)] {
        let env = Env::new(&format!("ws-delete-{}", size.0));
        let source = "// shared layout\nlayout { pane name=\"original\"; }\n";
        env.workspace("api", source);
        env.run(&["open", "api"]);
        let original: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
        let identity = original[0]["id"].clone();
        let path = env.root.join("config/kraal/workspaces/api.kdl");
        let mut client = RawClient::attach_sized(&env, Some("api"), size);
        client.wait_for_row("$ ");
        client.send(b"\x02w\t");
        click_account_text(&mut client, "Delete");
        client.wait_for_row("Delete api?");
        client.wait_for_row("Project files will be kept.");
        client.send(b"\r");
        client.wait_for_row("Search");
        assert_eq!(env.run(&["workspaces", "--json"]), serde_json::to_string_pretty(&original).unwrap() + "\n");
        client.send(b"\t");
        click_account_text(&mut client, "Delete");
        client.wait_for_row("Delete api?");
        client.send(b"y");
        client.wait_for_row("Ctrl+Z Undo");
        assert!(listed_workspaces(&env).is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        client.send(b"\x1a");
        client.wait_for_row("Workspace restored");
        let restored: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
        assert_eq!(restored[0]["id"], identity);
        assert_eq!(restored[0]["open"], false);
        assert_eq!(env.run(&["panes", "--json"]).trim(), "[]");
        client.send(b"\t");
        click_account_text(&mut client, "Delete");
        client.wait_for_row("Delete api?");
        client.send(b"y");
        client.wait_for_row("Workspace deleted");
        env.run(&["kill"]);
        assert!(listed_workspaces(&env).is_empty(), "the stopped-session scan must also exclude the source");
        env.run(&["tunnel", "list"]);
        let mut client = RawClient::attach_sized(&env, None, size);
        client.wait_for_row("Deleted workspaces (1)");
        assert_eq!(env.run(&["panes", "--json"]).trim(), "[]", "attaching must not add a replacement workspace");
        click_account_text(&mut client, "+ Add workspace");
        client.wait_for_row("Add workspace");
        client.send(b"\x01\x0bapi\r");
        client.wait_for_row("Workspace added");
        let replacement: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
        assert_ne!(replacement[0]["id"], identity);
        let panes = env.run(&["panes", "--json"]);
        client.send(b"\x02w");
        click_account_text(&mut client, "Deleted workspaces (1)");
        client.wait_for_row("Deleted today");
        client.send(b"\r");
        client.wait_for_row("Name    api-restored");
        client.send(b"\x1b");
        client.wait_for_row("Deleted today");
        client.send(b"\r");
        client.wait_for_row("Restore workspace");
        client.send(b"\r");
        client.wait_for_row("Workspace restored");
        let restored: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
        let restored = restored.as_array().unwrap();
        let old = restored.iter().find(|w| w["name"] == "api-restored").unwrap();
        let new = restored.iter().find(|w| w["name"] == "api").unwrap();
        assert_eq!(old["id"], identity);
        assert_eq!(old["open"], false);
        assert_eq!(new["id"], replacement[0]["id"]);
        assert_eq!(env.run(&["panes", "--json"]), panes, "Restore starts no programs");
        assert_eq!(std::fs::read_to_string(path).unwrap(), source);
    }
}

#[test]
fn deleting_a_scanned_source_never_runs_it_and_restores_its_saved_contents() {
    let env = Env::new("ws-delete-source");
    let marker = env.root.join("ran");
    let command = format!("echo started > {}", marker.display());
    let source = format!("layout {{ pane command=\"sh\" {{ args \"-c\" {command:?}; }}; }}\n");
    env.workspace("recipe", &source);
    env.run(&["tunnel", "list"]);
    let mut client = RawClient::attach_sized(&env, None, (80, 24));
    client.wait_for_message("\"attached\"");
    client.send(b"\x02wrecipe\t");
    click_account_text(&mut client, "Delete");
    client.wait_for_row("Delete recipe?");
    client.send(b"y");
    client.wait_for_row("Workspace deleted");
    assert!(!marker.exists());
    let path = env.root.join("config/kraal/workspaces/recipe.kdl");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
    env.run(&["kill"]);
    assert!(!env.run(&["workspaces", "--json"]).contains("recipe"));
    std::fs::remove_file(&path).unwrap();
    env.run(&["tunnel", "list"]);
    let mut client = RawClient::attach_sized(&env, None, (80, 24));
    client.wait_for_message("\"attached\"");
    client.send(b"\x02w");
    click_account_text(&mut client, "Deleted workspaces (1)");
    client.wait_for_row("recipe");
    client.send(b"\r");
    client.wait_for_row("Workspace restored");
    assert!(!marker.exists());
    assert!(!path.exists());
    let restored: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    let recipe = restored.as_array().unwrap().iter().find(|w| w["name"] == "recipe").unwrap();
    assert_eq!(recipe["open"], false);
    assert_eq!(recipe["panes"], 1);
    env.run(&["open", "recipe"]);
    wait_until(PATIENCE, "the explicitly opened recipe did not run", || {
        crate::common::read_complete_lines(&marker).is_some()
    });
}

#[test]
fn workspace_save_errors_keep_the_name_and_full_reason_at_both_sizes() {
    let env = Env::new("wssaveerrors");
    let dir = env.root.join("workspaces-link");
    std::os::unix::fs::symlink(env.root.join("config/kraal/workspaces"), &dir).unwrap();
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    for size in [(80, 24), (124, 36)] {
        let name = format!("copy-{}-a  b-{}", size.0, "long-name-".repeat(5));
        let path = dir.join(format!("{name}.kdl"));
        std::fs::create_dir(&path).unwrap();
        let mut client = RawClient::attach_sized(&env, Some("w"), size);
        client.wait_for_message("\"attached\"");
        client.send(b"\x02Ws");
        client.wait_for_row("Export layout");
        client.send(format!("\x01\x0b {}\r", path.display()).as_bytes());
        let expected = format!("{} is not a regular file", path.canonicalize().unwrap().display());
        let rows = client.wait_for_rows("the full export error and Retry", |rows| {
            // The pane and prompt keep their right borders even when the strip is covered.
            // Remove only the prompt's padding: a path can continue across rows and contain spaces.
            let message: String = rows
                .iter()
                .filter_map(|r| r.rsplit('│').nth(2))
                .filter_map(|r| r.strip_prefix(' ')?.strip_suffix(' '))
                .collect();
            message.contains(&expected) && rows.iter().any(|r| r.contains("Enter Retry"))
        });
        assert!(rows.iter().any(|r| r.contains("Esc Cancel")));
        std::fs::remove_dir(&path).unwrap();
        client.send(b"\r");
        client.wait_for_row("Layout exported");
        assert!(path.is_file());
        assert!(!path.with_extension("kdl.bak").exists());
        assert_eq!(env.run(&["panes"]).lines().count(), 1);
    }
}

#[test]
fn workspace_saves_check_ownership_and_confirm_the_exact_file() {
    let env = Env::new("wssavechecked");
    let dir = env.root.join("config/kraal/workspaces");
    let old = "layout {\n    pane name=\"original\"\n}\n";
    env.workspace("a", old);
    env.workspace("b", old);
    env.workspace("copy", old);
    env.run(&["open", "a"]);
    env.run(&["open", "b"]);
    let refused = env.try_run(&["export", dir.join("b.kdl").to_str().unwrap(), "--workspace", "a"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("belongs to workspace"));
    assert_eq!(std::fs::read_to_string(dir.join("b.kdl")).unwrap(), old);
    let refused = env.try_run(&["export", dir.join("copy.kdl").to_str().unwrap(), "--workspace", "a"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("--replace"));
    assert!(!dir.join("copy.kdl.bak").exists());

    let mut client = RawClient::attach(&env, "a");
    client.wait_for_message("\"attached\"");
    client.send(b"\x02Ws");
    client.wait_for_row("Export layout");
    client.send(format!("\x01\x0b{}\r", dir.join("copy.kdl").display()).as_bytes());
    client.wait_for_row("Replace file?");
    client.send(b"\r");
    client.wait_for_row("Export layout");
    assert_eq!(std::fs::read_to_string(dir.join("copy.kdl")).unwrap(), old);
    client.send(b"\r");
    client.wait_for_row("Replace file?");
    std::fs::write(dir.join("copy.kdl"), "edited after confirmation").unwrap();
    client.send(b"y");
    // The reason stays visible even when the full destination is wider than the prompt.
    client.wait_for_row("changed while saving");
    client.send(b"\r");
    client.wait_for_row("Replace file?");
    client.send(b"y");
    client.wait_for_row("Layout exported");
    assert_eq!(std::fs::read_to_string(dir.join("copy.kdl.bak")).unwrap(), "edited after confirmation");
    assert_eq!(listed_workspaces(&env), ["a: a.kdl (open)", "b: b.kdl (open)"]);
    // Saving the linked source after a manual edit also asks, without restarting its panes.
    std::fs::write(dir.join("a.kdl"), "invalid KDL from an editor").unwrap();
    client.send(b"\x1b\x1b\x02Ws");
    client.wait_for_row("Export layout");
    client.send(format!("\x01\x0b{}\r", dir.join("a.kdl").display()).as_bytes());
    client.wait_for_row("File changed");
    client.send(b"y");
    client.wait_for_row("Layout exported");
    wait_until(PATIENCE, "the confirmed source was not saved", || {
        std::fs::read_to_string(dir.join("a.kdl.bak")).is_ok_and(|s| s == "invalid KDL from an editor")
    });
    assert_eq!(env.run(&["panes"]).lines().count(), 2);
}

/// Independent sessions share files, not revisions. They cannot overwrite each other's saves,
/// including when both try to create the same copy at once.
#[test]
fn two_sessions_cannot_silently_replace_each_others_layouts() {
    let first = Env::new("wssavefirst");
    let second = Env::new("wssavesecond");
    let shared = first.root.join("config/kraal/workspaces");
    let alias = second.root.join("config/kraal/workspaces");
    std::fs::remove_dir(&alias).unwrap();
    std::os::unix::fs::symlink(&shared, &alias).unwrap();
    let old = "layout {\n    pane name=\"shared\"\n}\n";
    first.workspace("work", old);
    first.run(&["open", "work"]);
    second.run(&["open", "work"]);
    first.run(&["run", "--name", "first", "--", "sleep", "100"]);
    second.run(&["run", "--name", "second", "--", "sleep", "100"]);
    first.run(&["export", shared.join("work.kdl").to_str().unwrap(), "--workspace", "work", "--replace"]);
    let winner = std::fs::read_to_string(shared.join("work.kdl")).unwrap();
    let refused = second.try_run(&["export", shared.join("work.kdl").to_str().unwrap(), "--workspace", "work"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stdout).contains("File changed"));
    assert_eq!(std::fs::read_to_string(shared.join("work.kdl")).unwrap(), winner);
    assert_eq!(std::fs::read_to_string(shared.join("work.kdl.bak")).unwrap(), old);
    // Restarting cannot silently adopt the version written by the other session.
    second.run(&["kill"]);
    second.run(&["open", "work"]);
    assert!(
        !second.try_run(&["export", shared.join("work.kdl").to_str().unwrap(), "--workspace", "work"]).status.success()
    );
    let mut one = first.cmd(&["export", shared.join("copy.kdl").to_str().unwrap(), "--workspace", "work"]);
    let mut two = second.cmd(&["export", shared.join("copy.kdl").to_str().unwrap(), "--workspace", "work"]);
    one.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    two.stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    let one = one.spawn().unwrap();
    let two = two.spawn().unwrap();
    let (one, two) = (one.wait_with_output().unwrap(), two.wait_with_output().unwrap());
    assert_ne!(one.status.success(), two.status.success(), "{one:?} {two:?}");
    let copy = std::fs::read_to_string(shared.join("copy.kdl")).unwrap();
    let expected = if one.status.success() { "name=\"first\"" } else { "name=\"second\"" };
    assert!(copy.contains(expected), "{copy}");
    assert!(!shared.join("copy.kdl.bak").exists());
}

/// `kraal export` run in a pane exports that pane's workspace, as every command run in a pane acts
/// where it runs: not the workspace a terminal shows, nor the first one. `--workspace` names
/// another, and outside a pane the one on screen is saved.
#[test]
fn export_in_a_pane_uses_the_workspace_of_that_pane() {
    let env = Env::new("savehere");
    let dir = env.root.join("config/kraal/workspaces");
    env.workspace("w", "layout {\n    pane name=\"first\"\n}\n");
    env.workspace("v", "layout {\n    pane name=\"second\"\n}\n");
    env.run(&["open", "w"]);
    env.run(&["open", "v"]);
    // A terminal shows w, and the command runs in the pane of v.
    let client = RawClient::attach(&env, "w");
    client.wait_for_message("\"attached\"");
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let second = panes.as_array().unwrap().iter().find(|p| p["name"] == "second").unwrap()["id"].to_string();
    let in_pane = |args: &[&str]| {
        let out = env.cmd(args).env("KRAAL_PANE", &second).env("KRAAL_SESSION", &env.session).output().unwrap();
        assert!(out.status.success(), "kraal {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let saved = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap_or_default();

    std::fs::remove_file(dir.join("v.kdl")).unwrap();
    assert!(in_pane(&["export", dir.join("v.kdl").to_str().unwrap()]).contains("/v.kdl"));
    // Recreating a missing definition keeps the persistent workspace file lock.
    assert_eq!(names_in(&dir), [".v.kdl.lock", "v.kdl", "w.kdl"]);
    assert!(in_pane(&["export", dir.join("copy.kdl").to_str().unwrap()]).contains("/copy.kdl"));
    assert!(saved("copy.kdl").contains("name=\"second\"") && !saved("copy.kdl").contains("first"));
    assert!(in_pane(&["export", dir.join("named.kdl").to_str().unwrap(), "--workspace", "w"]).contains("/named.kdl"));
    assert!(saved("named.kdl").contains("name=\"first\""), "{}", saved("named.kdl"));
    // Outside a pane: the workspace on screen.
    std::fs::remove_file(dir.join("w.kdl")).unwrap();
    assert!(env.run(&["export", dir.join("w.kdl").to_str().unwrap()]).contains("/w.kdl"));
}

/// `kraal workspaces --json`, a line each: name, file, and whether it is open.
fn listed_workspaces(env: &Env) -> Vec<String> {
    let list: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    let line = |w: &serde_json::Value| {
        let file = w["definition"].as_str().and_then(|p| Path::new(p).file_name()).unwrap_or_default();
        let open = if w["open"] == true { " (open)" } else { "" };
        format!("{}: {}{open}", w["name"].as_str().unwrap(), file.to_string_lossy())
    };
    list.as_array().unwrap().iter().map(line).collect()
}

/// The names in `dir`, sorted.
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> =
        std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    names
}

/// The rows naming the definitions the sidebar lists as Closed, once the screen settles.
fn closed_sidebar_rows(client: &RawClient) -> Vec<String> {
    client.settle();
    client.rows().windows(2).filter(|w| w[1].contains("Closed")).map(|w| w[0].clone()).collect()
}

/// Rename keeps the identity and source, including after restart. Only an explicit save
/// writes the source file.
#[test]
fn a_renamed_workspace_keeps_its_file_unchanged() {
    let env = Env::new("wsrename");
    let dir = env.root.join("config/kraal/workspaces");
    env.workspace("perso", "layout {\n    pane name=\"shell\"\n}\n");
    env.workspace("work", "layout {\n    pane name=\"other\"\n}\n");
    env.run(&["open", "perso"]);
    let identity =
        || serde_json::from_str::<serde_json::Value>(&env.run(&["workspaces", "--json"])).unwrap()[0]["id"].clone();
    let id = identity();
    assert!(id.as_str().is_some());
    let mut client = RawClient::attach(&env, "perso");
    client.wait_for_message("\"attached\"");
    client.wait_for_row("Closed");
    client.send(b"\x02We");
    client.wait_for_row("Rename workspace");
    // Home, then the prefix.
    client.send(b"\x1b[Hinfra - \r");
    client.wait_for_row("Workspace renamed");
    assert_eq!(identity(), id);
    assert_eq!(names_in(&dir), ["perso.kdl", "work.kdl"]);
    let shown = closed_sidebar_rows(&client);
    assert!(shown.len() == 1 && shown[0].contains("work"), "{shown:?}");
    assert_eq!(listed_workspaces(&env), ["infra - perso: perso.kdl (open)", "work: work.kdl"]);
    let text = env.run(&["workspaces"]);
    assert!(!text.lines().any(|l| l.starts_with("perso ")), "{text}");
    // Export proposes a copy; replacing the source is an explicit choice.
    client.send(b"\x02Ws");
    client.wait_for_row("Export layout");
    client.send(format!("\x01\x0b{}\r", dir.join("perso.kdl").display()).as_bytes());
    client.wait_for_row("Replace file?");
    client.send(b"y");
    client.wait_for_row("Layout exported");
    assert_eq!(names_in(&dir), [".perso.kdl.lock", "perso.kdl", "perso.kdl.bak", "work.kdl"]);
    // Restored after a restart, still linked to its file.
    env.run(&["kill"]);
    env.run(&["open", "infra - perso"]);
    assert_eq!(listed_workspaces(&env), ["infra - perso: perso.kdl (open)", "work: work.kdl"]);
    assert_eq!(env.run(&["panes"]).lines().count(), 1);
    assert_eq!(identity(), id);
}

/// Existing list names are refused, but workspace names have no file-name restrictions.
#[test]
fn a_workspace_rename_never_replaces_a_file() {
    let env = Env::new("wsrenameexists");
    let dir = env.root.join("config/kraal/workspaces");
    env.workspace("a", "layout { pane name=\"one\"; }\n");
    env.workspace("b", "layout { pane name=\"two\"; }\n");
    env.run(&["open", "a"]);
    let mut client = RawClient::attach(&env, "a");
    client.wait_for_message("\"attached\"");
    client.send(b"\x02We");
    client.wait_for_row("Rename workspace");
    client.send(b"\x7fb\r");
    client.wait_for_row("This name is already used.");
    client.send(b"\x7fx/y\r");
    client.wait_for_row("Workspace renamed");
    assert_eq!(listed_workspaces(&env), ["x/y: a.kdl (open)", "b: b.kdl"]);
    assert_eq!(names_in(&dir), ["a.kdl", "b.kdl"]);
    assert_eq!(std::fs::read_to_string(dir.join("a.kdl")).unwrap(), "layout { pane name=\"one\"; }\n");
    assert_eq!(std::fs::read_to_string(dir.join("b.kdl")).unwrap(), "layout { pane name=\"two\"; }\n");
}

/// Changing a workspace's name in the saved session preserves its source link: opening by the
/// source file's name reuses it. Explicit export and another rename keep the same files.
#[test]
fn a_workspace_renamed_without_its_file_is_still_that_files() {
    let env = Env::new("wslegacy");
    let dir = env.root.join("config/kraal/workspaces");
    env.workspace("perso", "layout {\n    pane name=\"shell\"\n}\n");
    env.workspace("work", "layout {\n    pane name=\"other\"\n}\n");
    env.run(&["open", "perso"]);
    env.run(&["kill"]);
    let state = std::fs::read_to_string(env.state_file()).unwrap();
    assert_eq!(state.matches("\"name\": \"perso\"").count(), 1, "{state}");
    std::fs::write(env.state_file(), state.replace("\"name\": \"perso\"", "\"name\": \"infra - perso\"")).unwrap();
    // The old name switches to it rather than starting it again.
    env.run(&["open", "perso"]);
    assert_eq!(listed_workspaces(&env), ["infra - perso: perso.kdl (open)", "work: work.kdl"]);
    assert_eq!(env.run(&["panes"]).lines().count(), 1);
    let mut client = RawClient::attach(&env, "perso");
    client.wait_for_message("\"attached\"");
    let shown = closed_sidebar_rows(&client);
    assert!(shown.len() == 1 && shown[0].contains("work"), "{shown:?}");
    let saved =
        env.run(&["export", dir.join("perso.kdl").to_str().unwrap(), "--workspace", "infra - perso", "--replace"]);
    assert!(saved.trim_end().ends_with("/perso.kdl"), "{saved}");
    assert_eq!(names_in(&dir), [".perso.kdl.lock", "perso.kdl", "perso.kdl.bak", "work.kdl"]);
    client.send(b"\x02We");
    client.wait_for_row("Rename workspace");
    client.send(b"\r");
    client.wait_for_row("Workspace renamed");
    assert_eq!(names_in(&dir), [".perso.kdl.lock", "perso.kdl", "perso.kdl.bak", "work.kdl"]);
}

/// A definition renamed on disk while its workspace is open stays linked to it, and its new name
/// switches to the workspace. Deleted, it is written again by the next save.
#[test]
fn a_definition_renamed_on_disk_stays_linked() {
    let env = Env::new("wsmoved");
    let dir = env.root.join("config/kraal/workspaces");
    env.workspace("perso", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "perso"]);
    std::fs::rename(dir.join("perso.kdl"), dir.join("moved.kdl")).unwrap();
    wait_until(Duration::from_secs(10), "the renamed file is never found", || {
        listed_workspaces(&env) == ["perso: moved.kdl (open)"]
    });
    env.run(&["open", "moved"]);
    assert_eq!(env.run(&["panes"]).lines().count(), 1);
    std::fs::remove_file(dir.join("moved.kdl")).unwrap();
    env.run(&["export", dir.join("moved.kdl").to_str().unwrap(), "--workspace", "perso"]);
    assert_eq!(names_in(&dir), [".moved.kdl.lock", "moved.kdl"]);
}

#[test]
fn closing_a_workspace_lists_its_programs_and_background_jobs_from_both_entries() {
    let env = Env::new("wsclosejobs");
    env.workspace("api", "layout {\n    pane name=\"shell\"\n}\n");
    env.workspace("other", "layout {\n    pane name=\"unrelated\" command=\"sleep\" { args \"4259\"; }\n}\n");
    env.run(&["open", "api"]);
    env.run(&["open", "other"]);
    let job_pid = env.root.join("job.pid");
    env.run(&["send", "-p", "shell", "--enter", &format!("sleep 4257 & echo $! > '{}'", job_pid.display())]);
    wait_until(Duration::from_secs(10), "the background job never started", || {
        let Some(pid) = crate::common::read_complete_lines(&job_pid) else { return false };
        !pid.trim().is_empty()
            && Command::new("ps")
                .args(["-p", pid.trim(), "-o", "comm="])
                .output()
                .is_ok_and(|out| out.status.success() && String::from_utf8_lossy(&out.stdout).trim().ends_with("sleep"))
    });
    env.run(&["run", "--name", "foreground", "--target", "shell", "--", "sh", "-c", "echo ready; exec sleep 4258"]);
    env.wait_for_text("foreground", "ready");
    let before = env.run(&["panes"]);
    for size in [(80, 24), (124, 36)] {
        for picker in [false, true] {
            let mut client = RawClient::attach_sized(&env, Some(if picker { "other" } else { "api" }), size).traced();
            client.wait_for_message("\"attached\"");
            if picker {
                client.send(b"\x02w");
                client.wait_for_row("Workspaces");
                client.send(b"api");
                client.wait_for_row("Enter Open · Tab Actions · Esc Close");
                client.send(b"\x18");
            } else {
                client.send(b"\x02Wx");
            }
            client.wait_for_row("Close api?");
            let question = client.read_question(b"\x1b[6~").join("\n");
            for text in [
                "The layout will be kept.",
                "These programs will stop:",
                "sleep 4257 (in the background)",
                "sleep 4258",
                "Unsaved work in these programs may be lost.",
            ] {
                assert!(question.contains(text), "{size:?}, picker={picker}: {question}");
            }
            assert!(!question.contains("4259"), "{question}");
            assert!(!question.contains("linked file") && !question.contains("workspace file"), "{question}");
            assert!(!question.contains("(pane "), "{question}");
            assert!(!question.contains("The session will stop."), "{question}");
            client.wait_for_row("Stop and close");
            client.send(b"\r");
            client
                .wait_for_rows("Cancel closed the question", |rows| !rows.iter().any(|r| r.contains("Stop and close")));
            assert_eq!(env.run(&["panes"]), before);
        }
    }
    let mut client = RawClient::attach(&env, "api").traced();
    let other_client = RawClient::attach(&env, "api");
    other_client.wait_for_message("\"attached\"");
    client.wait_for_message("\"attached\"");
    client.send(b"\x02Wx");
    client.wait_for_row("Stop and close");
    client.send(b"y");
    client.wait_for_rows("the workspace closed", |rows| !rows.iter().any(|r| r.contains("Stop and close")));
    other_client.wait_for_row("unrelated");
    let panes = env.run(&["panes"]);
    assert!(!panes.contains("shell") && !panes.contains("foreground") && panes.contains("unrelated"), "{panes}");
    assert!(listed_workspaces(&env).contains(&"api: api.kdl".to_string()));
    assert_eq!(
        std::fs::read_to_string(env.root.join("config/kraal/workspaces/api.kdl")).unwrap(),
        "layout {\n    pane name=\"shell\"\n}\n"
    );
    env.run(&["open", "api"]);
    let panes = env.run(&["panes"]);
    assert!(panes.contains("shell") && panes.contains("foreground"), "{panes}");
}

#[test]
fn closing_a_workspace_without_a_file_keeps_it_and_the_last_session() {
    let env = Env::new("wscloselast");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    let pid = env.server_pid().unwrap();
    let mut client = RawClient::attach(&env, "w").traced();
    client.wait_for_message("\"attached\"");
    client.send(b"\x02Wn");
    client.wait_for_row("Name    ");
    client.send(b"\x01\x0bscratch\r");
    client.wait_for_rows("the workspace was created", |rows| !rows.iter().any(|r| r.contains("Name    ")));
    client.wait_for_row("$ ");
    for size in [(80, 24), (124, 36)] {
        client.resize(size);
        client.send(b"\x02Wx");
        client.wait_for_row("Closed · 1 tab · 1 pane");
        assert!(listed_workspaces(&env).contains(&"scratch: ".to_string()));
        assert_eq!(env.server_pid(), Some(pid));
        assert!(!client.messages.lock().unwrap().iter().any(|m| m.contains(r#""type":"exit""#)));
        client.send(b"\r");
        client.wait_for_rows("scratch reopened", |rows| !rows.iter().any(|r| r.contains("Workspaces")));
        client.wait_for_row("$ ");
        assert!(listed_workspaces(&env).contains(&"scratch:  (open)".to_string()));
    }
    client.send(b"\x02Wx");
    client.wait_for_row("Closed · 1 tab · 1 pane");
    client.send(b"\x1b");
    client.wait_for_rows("the workspace picker closed", |rows| !rows.iter().any(|r| r.contains("Workspaces")));
    client.wait_for_row("$ ");
    client.send(b"\x02Wx");
    client.wait_for_row("Workspaces");
    wait_until(PATIENCE, "both workspaces did not close", || {
        let workspaces: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
        workspaces.as_array().is_some_and(|ws| ws.len() == 2 && ws.iter().all(|w| w["open"] == false))
    });
    assert_eq!(env.server_pid(), Some(pid));
    assert!(env.state_file().exists());
    assert!(!client.messages.lock().unwrap().iter().any(|m| m.contains(r#""type":"exit""#)));
    env.run(&["kill"]);
    env.run(&["open", "scratch"]);
    assert!(listed_workspaces(&env).contains(&"scratch:  (open)".to_string()));
    assert!(listed_workspaces(&env).contains(&"w: w.kdl".to_string()));
}

/// An empty workspace remains usable, saved and attached after its last shell exits.
#[test]
fn the_last_pane_leaves_an_empty_saved_workspace() {
    let env = Env::new("lastexit");
    env.workspace("w", "layout {\n    pane name=\"only\"\n}\n");
    env.run(&["open", "w"]);
    let pid = env.server_pid().unwrap();
    let client = RawClient::attach(&env, "w");
    client.wait_for_message("\"attached\"");
    let before: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    env.run(&["send", "-p", "only", "--enter", "exit"]);
    client.wait_for_row("+ Add pane");
    assert_eq!(env.server_pid(), Some(pid));
    let after: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    assert_eq!(after[0]["id"], before[0]["id"]);
    assert_eq!(after[0]["tabs"], 0);
    assert_eq!(after[0]["panes"], 0);
    env.run(&["kill"]);
    env.run(&["open", "w"]);
    let restored: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    assert_eq!(restored, after);
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_row("+ Add pane");
    client.send(b"\r");
    wait_until(PATIENCE, "Enter did not add a pane", || {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        panes.as_array().is_some_and(|p| p.len() == 1)
    });
}

/// Closing keeps the personal layout even when its imported source changes or disappears.
#[test]
fn close_and_reopen_keep_unsaved_tabs_panes_and_identity() {
    let env = Env::new("closekept");
    let source = "layout {\n    pane name=\"shell\"\n}\n";
    env.workspace("w", source);
    env.run(&["open", "w"]);
    env.run(&["run", "--tab", "--name", "job", "--", "sh", "-c", "echo ready; exec sleep 4209"]);
    env.wait_for_text("job", "ready");
    let before: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    let mut client = RawClient::attach(&env, "w");
    let other = RawClient::attach(&env, "w");
    client.wait_for_message("\"attached\"");
    client.send(b"\x02Wx");
    client.wait_for_row("The layout will be kept.");
    client.send(b"y");
    client.wait_for_row("Closed · 2 tabs · 2 panes");
    other.wait_for_row("Closed · 2 tabs · 2 panes");
    let closed: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    assert_eq!(closed[0]["id"], before[0]["id"]);
    assert_eq!(closed[0]["open"], false);
    assert!(env.run(&["workspaces"]).contains("closed"));
    assert_eq!(std::fs::read_to_string(env.root.join("config/kraal/workspaces/w.kdl")).unwrap(), source);
    std::fs::remove_file(env.root.join("config/kraal/workspaces/w.kdl")).unwrap();
    env.run(&["kill"]);
    let saved = std::fs::read(env.state_file()).unwrap();
    let stopped: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    assert_eq!(stopped, closed);
    assert_eq!(std::fs::read(env.state_file()).unwrap(), saved, "listing never rewrites saved state");
    env.run(&["open", "w"]);
    let reopened: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    assert_eq!(reopened[0]["id"], before[0]["id"]);
    assert_eq!(reopened[0]["tabs"], 2);
    assert_eq!(reopened[0]["panes"], 2);
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    assert!(panes.as_array().unwrap().iter().any(|p| p["name"] == "job" && p["suspended"] == true));
    assert!(env.run(&["read", "-p", "job"]).contains("ready"));
}

#[test]
fn close_preparation_failure_keeps_the_programs_and_offers_retry() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new("closefull");
    env.workspace("w", "layout { pane name=\"only\"; }\n");
    env.run(&["open", "w"]);
    env.run(&["send", "-p", "only", "--enter", "echo waiting"]);
    env.wait_for_text("only", "waiting");
    wait_until(PATIENCE, "no saved state", || env.state_file().exists());
    let sessions = env.root.join("state/kraal/sessions-v2");
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o500)).unwrap();
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_message("\"attached\"");
    client.send(b"\x02Wx");
    client.wait_for_row("Nothing was stopped.");
    env.run(&["send", "-p", "only", "--enter", "echo still-running"]);
    env.wait_for_text("only", "still-running");
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o700)).unwrap();
    client.send(b"y");
    client.wait_for_row("Closed · 1 tab · 1 pane");
}

#[test]
fn commands_outside_kraal_skip_a_closed_first_workspace() {
    let env = Env::new("closedfirst");
    let bin = env.root.join("bin");
    let home = env.root.join("account");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    write_script(&bin.join("ssh"), "#!/bin/sh\necho connected\nexec sleep 60\n");
    write_script(&bin.join("claude"), "#!/bin/sh\n[ \"$2\" = status ] && exit 0\necho agent-ready\nexec sleep 60\n");
    std::fs::write(
        env.root.join("config/kraal/accounts.toml"),
        format!(
            "[[account]]\nid = 'test-claude-work'\nname = \"work\"\nagent = \"claude\"\nhome = {:?}\n",
            home.display().to_string()
        ),
    )
    .unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    env.workspace("a", "layout { pane name=\"first\"; }\n");
    env.workspace("b", "layout { pane name=\"second\"; }\n");
    assert!(env.cmd(&["open", "a"]).env("PATH", &path).status().unwrap().success());
    env.run(&["open", "b"]);
    let mut client = RawClient::attach(&env, "a");
    client.wait_for_row("$ ");
    client.send(b"\x02Wx");
    client.wait_for_row("Closed · 1 tab · 1 pane");
    drop(client);
    // No attached client, and the first retained workspace is Closed.
    let restarted = env.cmd(&["restart", "--yes"]).env("PATH", &path).output().unwrap();
    assert!(restarted.status.success(), "{}", String::from_utf8_lossy(&restarted.stderr));
    let run = env.run(&["run", "--name", "new", "--", "sh"]);
    // Address the server without a pane or attached terminal as the agent's destination.
    let agent = env
        .cmd(&["agent", "claude", "--account", "work", "--tab"])
        .env("KRAAL_SOCKET", env.root.join("run/kraal/closedfirst.sock"))
        .output()
        .unwrap();
    assert!(agent.status.success(), "{}", String::from_utf8_lossy(&agent.stderr));
    let agent = String::from_utf8(agent.stdout).unwrap();
    env.run(&["ssh", "example.invalid", "--tab"]);
    env.run(&["files", env.root.to_str().unwrap()]);
    env.wait_for_text(agent.trim(), "agent-ready");
    let panes: Vec<serde_json::Value> = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let ssh = panes.iter().find(|p| p["command"][0] == "ssh").unwrap();
    env.wait_for_text(&ssh["id"].to_string(), "connected");
    assert_eq!(panes.len(), 5);
    assert!(panes.iter().all(|p| p["workspace"] == "b"), "{panes:?}");
    assert!(panes.iter().any(|p| p["id"].as_u64() == Some(run.trim().parse::<u64>().unwrap())));
    let workspaces: Vec<serde_json::Value> = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    assert_eq!(workspaces[0]["name"], "a");
    assert_eq!(workspaces[0]["open"], false);
}

#[test]
fn a_server_runbook_after_the_last_close_gets_an_open_saved_workspace() {
    let env = Env::new("runafterclose");
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(&bin.join("ssh"), "#!/bin/sh\nfor a; do cmd=$a; done\nexec sh -c \"$cmd\"\n");
    std::fs::write(env.root.join("config/kraal/servers.toml"), "[[server]]\nname = \"box\"\nhost = \"box.invalid\"\n")
        .unwrap();
    std::fs::create_dir_all(env.root.join("config/kraal/runbooks")).unwrap();
    std::fs::write(env.root.join("config/kraal/runbooks/check.toml"), "on = \"server\"\nsteps = [\"echo checked\"]\n")
        .unwrap();
    env.workspace("w", "layout { pane name=\"original\"; }\n");
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    assert!(env.cmd(&["open", "w"]).env("PATH", &path).status().unwrap().success());
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_row("$ ");
    client.send(b"\x02Wx");
    client.wait_for_row("Closed · 1 tab · 1 pane");
    let result = env.run(&["runbook", "run", "check", "--on", "box", "--yes", "--wait"]);
    assert!(result.contains("finished: 1 ok, 0 failed, 0 skipped"), "{result}");
    env.wait_for_text("box", "checked");
    let workspaces: Vec<serde_json::Value> = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    assert_eq!(workspaces.len(), 2);
    assert_eq!(workspaces[0]["name"], "w");
    assert_eq!(workspaces[0]["open"], false);
    assert_eq!(workspaces[1]["open"], true);
    assert_eq!(workspaces[1]["panes"], 1);
    env.run(&["kill"]);
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(env.state_file()).unwrap()).unwrap();
    assert_eq!(saved["workspaces"][1]["tabs"][0]["root"]["Pane"]["name"], "box");
}

#[test]
fn the_ssh_group_command_reopens_its_closed_workspace() {
    let env = Env::new("groupreopen");
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(&bin.join("ssh"), "#!/bin/sh\necho connected\nexec sleep 60\n");
    std::fs::write(
        env.root.join("config/kraal/servers.toml"),
        "[[server]]\nname = \"box\"\nhost = \"box.invalid\"\ngroups = [\"prod\"]\n",
    )
    .unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    assert!(env.cmd(&["ssh", "prod", "--group"]).env("PATH", &path).status().unwrap().success());
    env.wait_for_text("box", "connected");
    let before: Vec<serde_json::Value> = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    let name = before[0]["name"].as_str().unwrap();
    let mut client = RawClient::attach(&env, name);
    client.wait_for_message("\"attached\"");
    client.send(b"\x02Wx");
    client.wait_for_row("The layout will be kept.");
    client.send(b"y");
    client.wait_for_row("Closed · 1 tab · 1 pane");
    env.run(&["ssh", "prod", "--group"]);
    env.wait_for_text_since_restore("box", "connected");
    let after: Vec<serde_json::Value> = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0]["id"], before[0]["id"]);
    assert_eq!(after[0]["open"], true);
    assert_eq!(after[0]["panes"], 1);
}

#[test]
fn a_crash_during_close_finishes_stopping_without_reopening_the_workspace() {
    let env = Env::new("closecrash");
    env.workspace("w", "layout { pane name=\"job\" command=\"sh\" { args \"-c\" \"trap '' HUP TERM; echo ready; while :; do sleep 1; done\"; }; }\n");
    env.workspace("other", "layout { pane; }\n");
    env.run(&["open", "w"]);
    env.wait_for_text("job", "ready");
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_message("\"attached\"");
    client.send(b"\x02Wx");
    client.wait_for_row("The layout will be kept.");
    client.send(b"y");
    wait_until(PATIENCE, "close was not prepared", || {
        std::fs::read(env.state_file())
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .is_some_and(|s| s["workspaces"][0]["close_pending"].is_object())
    });
    let pid = env.server_pid().unwrap();
    // SAFETY: only the isolated server this test started, after its close intent reached disk.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    wait_until(PATIENCE, "the crashed server still runs", || env.server_pid().is_none());
    // Its process information can disappear before its listening socket closes.
    let socket = env.root.join("run/kraal/closecrash.sock");
    wait_until(PATIENCE, "the crashed server still listens", || {
        std::os::unix::net::UnixStream::connect(&socket).is_err()
    });
    env.run(&["open", "other"]);
    let list: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    let ws = list.as_array().unwrap().iter().find(|w| w["name"] == "w").unwrap();
    assert_eq!(ws["open"], false);
    assert_eq!(ws["panes"], 1);
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    assert!(panes.as_array().unwrap().iter().all(|p| p["workspace"] != "w"));
}

#[test]
fn a_crash_during_delete_finishes_the_deletion_and_keeps_it_restorable() {
    let env = Env::new("deletecrash");
    env.workspace("w", "layout { pane name=\"job\" command=\"sh\" { args \"-c\" \"trap '' HUP TERM; echo ready; while :; do sleep 1; done\"; }; }\n");
    env.run(&["open", "w"]);
    env.wait_for_text("job", "ready");
    let mut client = RawClient::attach_sized(&env, Some("w"), (80, 24));
    client.wait_for_message("\"attached\"");
    client.send(b"\x02w\t");
    click_account_text(&mut client, "Delete");
    client.wait_for_row("Stop and delete");
    client.send(b"y");
    wait_until(PATIENCE, "deletion was not prepared", || {
        std::fs::read(env.state_file())
            .ok()
            .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
            .is_some_and(|s| {
                s["workspaces"][0]["delete_pending"] == true && s["workspaces"][0]["close_pending"].is_object()
            })
    });
    let pid = env.server_pid().unwrap();
    // SAFETY: only this test's server, after its deletion intent reached disk.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    wait_until(PATIENCE, "the crashed server still runs", || env.server_pid().is_none());
    let socket = env.root.join("run/kraal/deletecrash.sock");
    wait_until(PATIENCE, "the crashed server still listens", || {
        std::os::unix::net::UnixStream::connect(&socket).is_err()
    });
    env.run(&["tunnel", "list"]);
    assert!(listed_workspaces(&env).is_empty());
    assert_eq!(env.run(&["panes", "--json"]).trim(), "[]");
    let mut client = RawClient::attach_sized(&env, None, (80, 24));
    client.wait_for_row("Deleted workspaces (1)");
    click_account_text(&mut client, "Deleted workspaces (1)");
    client.wait_for_row("Deleted today");
    client.send(b"\r");
    client.wait_for_row("Workspace restored");
    let restored: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    assert_eq!(restored[0]["name"], "w");
    assert_eq!(restored[0]["open"], false);
    assert_eq!(restored[0]["panes"], 1);
    assert_eq!(env.run(&["panes", "--json"]).trim(), "[]");
}

#[test]
fn importing_a_file_previews_then_keeps_commands_waiting_and_export_preserves_the_source() {
    let env = Env::new("layout-transfer");
    let path = env.root.join("recipe.kdl");
    let marker = env.root.join("must-not-run");
    let source = format!(
        "// handwritten\nlayout {{ pane command=\"touch\" {{ args {:?}; }}; unknown; }}\n",
        marker.display().to_string()
    );
    std::fs::write(&path, &source).unwrap();
    let file = path.to_str().unwrap();
    let refused = env.try_run(&["import", file]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stdout).contains("Commands wait for you to start them"));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("No workspace was added"));
    assert!(!marker.exists());
    let added = env.run(&["import", file, "--name", "personal", "--yes"]);
    assert!(added.contains("unknown") && added.contains("Workspace \"personal\" added"), "{added}");
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    assert!(panes.as_array().unwrap().iter().filter(|p| p["workspace"] == "personal").all(|p| p["suspended"] == true));
    assert!(!marker.exists());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
    let again = env.try_run(&["import", file, "--name", "duplicate", "--yes"]);
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("already used by personal"));
    let exported = env.root.join("config/kraal/workspaces/shared.kdl");
    assert!(env.run(&["export", exported.to_str().unwrap(), "--workspace", "personal"]).contains("Layout exported"));
    assert!(!listed_workspaces(&env).iter().any(|w| w.starts_with("shared:")));
    let refused = env.try_run(&["export", file, "--workspace", "personal"]);
    assert!(!refused.status.success());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
    env.run(&["export", file, "--workspace", "personal", "--replace"]);
    assert_eq!(std::fs::read_to_string(path.with_extension("kdl.bak")).unwrap(), source);
    env.run(&["kill"]);
    env.run(&["open", "personal"]);
    assert!(!marker.exists());
    assert!(!listed_workspaces(&env).iter().any(|w| w.starts_with("shared:")));
}

#[test]
fn workspace_cli_quotes_names_and_shortens_home_paths_in_rendered_output() {
    let env = Env::new("workspace-messages");
    let path = env.root.join("home/recipe.kdl");
    std::fs::write(&path, "layout { pane command=\"true\"; }").unwrap();
    let imported = env.run(&["import", path.to_str().unwrap(), "--name", "imported", "--yes"]);
    assert!(imported.lines().any(|line| line == "~/recipe.kdl"), "{imported}");
    assert!(imported.contains("Workspace \"imported\" added."), "{imported}");
    let exported = env.run(&["export", "~/shared.kdl", "--workspace", "imported"]);
    assert_eq!(exported.trim(), "Layout exported\n~/shared.kdl");
    let opened = env.run(&["open", "imported"]);
    assert!(opened.contains("workspace \"imported\" is open"), "{opened}");
}

#[test]
fn importing_a_bare_filename_uses_the_callers_directory() {
    let env = Env::new("import-relative");
    let project = env.root.join("project");
    std::fs::create_dir(&project).unwrap();
    let source = "layout { pane command=\"true\"; }";
    let other = "layout { pane command=\"false\"; }";
    std::fs::write(project.join("layout.kdl"), source).unwrap();
    std::fs::write(env.root.join("home/layout.kdl"), other).unwrap();
    let out = env.cmd(&["import", "layout.kdl", "--yes"]).current_dir(&project).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("Workspace \"layout\" added"));
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let pane = panes.as_array().unwrap().iter().find(|p| p["workspace"] == "layout").unwrap();
    assert_eq!(pane["command"], serde_json::json!(["true"]));
    assert_eq!(pane["suspended"], true);
    let workspaces: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    let workspace = workspaces.as_array().unwrap().iter().find(|w| w["name"] == "layout").unwrap();
    assert_eq!(workspace["definition"], std::fs::canonicalize(project.join("layout.kdl")).unwrap().to_str().unwrap());
    assert_eq!(std::fs::read_to_string(project.join("layout.kdl")).unwrap(), source);
    assert_eq!(std::fs::read_to_string(env.root.join("home/layout.kdl")).unwrap(), other);
}

#[test]
fn from_file_and_export_use_the_shared_forms_at_both_sizes() {
    for size in [(80, 24), (124, 36)] {
        let env = Env::new(&format!("file-forms-{}", size.0));
        env.workspace("base", "layout { pane; }");
        env.run(&["open", "base"]);
        let path = env.root.join("imported.kdl");
        let original = "// retain this\nlayout { pane command=\"false\"; }\n";
        std::fs::write(&path, original).unwrap();
        let mut client = RawClient::attach_sized(&env, Some("base"), size);
        client.wait_for_message("\"attached\"");
        client.send(b"\x02Wn");
        client.wait_for_row("From file...");
        click_account_text(&mut client, "From file...");
        client.send(format!("{}\t", path.display()).as_bytes());
        client.wait_for_row("Commands wait for you to start them.");
        client.send(b"\r");
        client.wait_for_row("Workspace added");
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let imported = panes.as_array().unwrap().iter().find(|p| p["workspace"] == "imported").unwrap();
        assert_eq!(imported["suspended"], true);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        client.send(b"\x02Ws");
        client.wait_for_row("Workspaces are saved automatically.");
        let copy = env.root.join("copy.kdl");
        client.send(format!("\x01\x0b{}\r", copy.display()).as_bytes());
        client.wait_for_row("Layout exported");
        assert!(std::fs::read_to_string(copy).unwrap().contains("command=\"false\""));
        assert_eq!(std::fs::read_to_string(path).unwrap(), original);
    }
}

#[test]
fn legacy_zellij_import_keeps_manual_bytes_and_backs_up_replacements() {
    let env = Env::new("import-zellij-backup");
    let from = env.root.join("layouts");
    std::fs::create_dir(&from).unwrap();
    let source = "// keep all comments\nlayout { pane; unknown; }\n";
    std::fs::write(from.join("manual.kdl"), source).unwrap();
    env.workspace("manual", "layout { pane name=\"before\"; }");
    let dest = env.root.join("config/kraal/workspaces/manual.kdl");
    let old = std::fs::read_to_string(&dest).unwrap();
    env.run(&["import-zellij", "--from", from.to_str().unwrap()]);
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), old);
    env.run(&["import-zellij", "--from", from.to_str().unwrap(), "--force"]);
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), source);
    assert_eq!(std::fs::read_to_string(dest.with_extension("kdl.bak")).unwrap(), old);
    env.run(&["open", "manual"]);
    std::fs::write(from.join("manual.kdl"), "layout { pane name=\"replacement\"; }").unwrap();
    let skipped = env.try_run(&["import-zellij", "--from", from.to_str().unwrap(), "--force"]);
    assert!(skipped.status.success());
    assert!(String::from_utf8_lossy(&skipped.stderr).contains("belongs to workspace"));
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), source);
    env.run(&["kill"]);
    let skipped = env.try_run(&["import-zellij", "--from", from.to_str().unwrap(), "--force"]);
    assert!(skipped.status.success());
    assert!(String::from_utf8_lossy(&skipped.stderr).contains("belongs to workspace"));
    assert_eq!(std::fs::read_to_string(&dest).unwrap(), source);
    assert_eq!(std::fs::read_to_string(dest.with_extension("kdl.bak")).unwrap(), old);
    assert!(env.server_pid().is_none());
}

#[test]
fn file_review_rechecks_changes_stops_only_on_confirmation_and_undo_keeps_the_local_layout() {
    for size in [(80, 24), (124, 36)] {
        let env = Env::new(&format!("source-review-{}", size.0));
        env.workspace(
            "source",
            "layout { pane name=\"original\" command=\"sh\" { args \"-c\" \"echo running; exec sleep 60\"; }; }",
        );
        env.run(&["open", "source"]);
        env.wait_for_text("original", "running");
        let path = env.root.join("config/kraal/workspaces/source.kdl");
        let marker = env.root.join("must-not-run");
        let mut client = RawClient::attach_sized(&env, Some("source"), size);
        client.wait_for_message("\"attached\"");
        std::fs::write(&path, "layout { pane command=\"false\"; }").unwrap();
        client.send(b"\x02w");
        client.wait_for_row("File changed");
        client.send(b"\t");
        click_account_text(&mut client, "Review");
        client.wait_for_row("Saved workspace:");
        let changed = format!(
            "layout {{ pane command=\"touch\" {{ args \"{}\"; }}; pane command=\"false\"; }}",
            marker.display()
        );
        std::fs::write(&path, &changed).unwrap();
        click_account_text(&mut client, "Load from file");
        client.wait_for_row("File changed again");
        let before: Vec<serde_json::Value> = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        assert_eq!(before.len(), 1);
        assert_eq!(before[0]["name"], "original");
        click_account_text(&mut client, "Load from file");
        client.wait_for_row("Stop and load");
        client.send(b"n");
        client.wait_for_row("Saved workspace:");
        let cancelled: Vec<serde_json::Value> = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        assert_eq!(cancelled.len(), 1);
        assert_eq!(cancelled[0]["id"], before[0]["id"]);
        click_account_text(&mut client, "Load from file");
        client.wait_for_row("Stop and load");
        client.send(b"y");
        client.wait_for_row("Layout loaded");
        let panes: Vec<serde_json::Value> = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        assert_eq!(panes.len(), 2);
        assert!(panes.iter().all(|p| p["suspended"] == true));
        assert!(!marker.exists());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), changed);
        client.send(b"\t");
        click_account_text(&mut client, "Undo load");
        client.wait_for_row("Layout restored");
        let workspaces: Vec<serde_json::Value> = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
        assert_eq!(workspaces.len(), 1);
        assert_eq!(workspaces[0]["open"], false);
        assert_eq!(workspaces[0]["panes"], 1);
        env.run(&["kill"]);
        drop(client);
        std::fs::remove_file(&path).unwrap();
        env.run(&["open", "source"]);
        let panes: Vec<serde_json::Value> = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        assert_eq!(panes.len(), 1);
        assert_eq!(panes[0]["name"], "original");
        assert_eq!(panes[0]["suspended"], true);
        assert!(!path.exists());
        assert!(!marker.exists());
    }
}

#[test]
fn workspace_commands_keep_identity_sources_and_recover_deletions_after_restart() {
    let env = Env::new("workspace-lifecycle-cli");
    let source = "// Keep this recipe.\nlayout { pane command=\"sleep 60\" start_suspended=true; }\n";
    env.workspace("api", source);
    env.run(&["open", "api"]);
    let list = || serde_json::from_str::<serde_json::Value>(&env.run(&["workspaces", "--json"])).unwrap();
    let id = list()[0]["id"].as_str().unwrap().to_string();
    env.run(&["workspaces", "rename", &id, "backend"]);
    assert_eq!(list()[0]["id"], id);
    assert_eq!(list()[0]["name"], "backend");
    let path = env.root.join("config/kraal/workspaces/api.kdl");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
    assert!(!path.with_extension("kdl.bak").exists());
    env.run(&["workspaces", "close", &id]);
    assert_eq!(list()[0]["open"], false);
    assert!(env.run(&["panes", "--json"]).trim() == "[]");
    let denied = env.try_run(&["workspaces", "delete", &id]);
    assert!(!denied.status.success());
    assert!(String::from_utf8_lossy(&denied.stderr).contains("--yes"));
    assert_eq!(list()[0]["id"], id);
    env.run(&["workspaces", "delete", &id, "--yes"]);
    assert_eq!(list(), serde_json::json!([]));
    env.run(&["kill"]);
    let deleted = env.run(&["workspaces", "--deleted", "--json"]);
    let deleted: serde_json::Value = serde_json::from_str(&deleted).unwrap();
    assert_eq!(deleted[0]["id"], id);
    env.run(&["workspaces", "restore", &id]);
    assert_eq!(list()[0]["id"], id);
    assert_eq!(list()[0]["open"], false);
    assert!(env.run(&["panes", "--json"]).trim() == "[]");
    assert_eq!(std::fs::read_to_string(path).unwrap(), source);
}

#[test]
fn workspace_commands_reject_control_characters_without_losing_workspaces() {
    let env = Env::new("workspace-name-rules");
    env.run(&["workspaces", "create", "original"]);
    let path = env.root.join("recipe.kdl");
    std::fs::write(&path, "layout { pane command=\"true\"; }\n").unwrap();
    for args in [
        vec!["workspaces", "create", "new\n"],
        vec!["workspaces", "rename", "original", "\tnew"],
        vec!["import", path.to_str().unwrap(), "--name", "new\u{85}name", "--yes"],
    ] {
        let output = env.try_run(&args);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("Choose a name without control characters."));
    }
    let list: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["name"], "original");
    let identity = list[0]["id"].clone();
    env.run(&["workspaces", "delete", "original", "--yes"]);
    let output = env.try_run(&["workspaces", "restore", "original", "--name", "new\r"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Choose a name without control characters."));
    let deleted: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--deleted", "--json"])).unwrap();
    assert_eq!(deleted[0]["id"], identity);
    env.run(&["workspaces", "restore", "original", "--name", "recovered"]);
    let list: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    assert_eq!(list[0]["id"], identity);
    assert_eq!(list[0]["name"], "recovered");
}

#[test]
fn workspace_commands_create_refuse_collisions_and_require_approval_to_stop_programs() {
    let env = Env::new("workspace-create-cli");
    let out = env.cmd(&["workspaces", "create", "api"]).current_dir(&env.root).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!env.try_run(&["workspaces", "create", "api"]).status.success());
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    assert_eq!(panes.as_array().unwrap().len(), 1);
    assert_eq!(Path::new(panes[0]["cwd"].as_str().unwrap()).canonicalize().unwrap(), env.root.canonicalize().unwrap());
    env.run(&["run", "--name", "busy", "sh", "-c", "echo workspace-ready; sleep 60"]);
    env.wait_for_text("busy", "workspace-ready");
    let denied = env.try_run(&["workspaces", "close", "api"]);
    assert!(!denied.status.success());
    let preview = String::from_utf8_lossy(&denied.stdout);
    assert!(preview.contains("These programs will stop:"), "{preview}");
    assert!(preview.contains("sleep 60"), "{preview}");
    assert!(String::from_utf8_lossy(&denied.stderr).contains("--yes"));
    assert!(env.run(&["panes", "--json"]).contains("busy"));
    env.run(&["workspaces", "close", "api", "--yes"]);
    env.run(&["workspaces", "delete", "api", "--yes"]);
    env.run(&["workspaces", "create", "api"]);
    assert!(!env.try_run(&["workspaces", "restore", "api"]).status.success());
    env.run(&["workspaces", "restore", "api", "--name", "recovered"]);
    let list: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    let recovered = list.as_array().unwrap().iter().find(|w| w["name"] == "recovered").unwrap();
    assert_eq!(recovered["open"], false);
    assert_eq!(recovered["panes"], 2);
}
