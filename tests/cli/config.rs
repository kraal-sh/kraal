use super::support::RawClient;
use crate::common::{Env, git_repo, wait_until};
use std::path::Path;
use std::time::Duration;
#[cfg(target_os = "linux")]
use std::time::Instant;

#[cfg(target_os = "linux")]
#[test]
fn config_reload_applies_new_keys_and_keeps_the_old_ones_when_the_file_is_broken() {
    let env = Env::new("reload");
    env.workspace("w", "layout {\n    pane name=\"first\"\n}\n");
    env.run(&["open", "w"]);
    let mut client = env.client();
    let mut typed = |bytes: &[u8]| std::io::Write::write_all(client.stdin.as_mut().unwrap(), bytes).unwrap();
    let panes = || {
        let v: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        v.as_array().map_or(0, Vec::len)
    };
    let wait_for = |n: usize| {
        let start = Instant::now();
        while panes() != n {
            assert!(start.elapsed() < Duration::from_secs(5), "expected {n} panes");
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    // A key the preset leaves free opens a tab once the configuration says so.
    let path = env.root.join("config/kraal/config.toml");
    let base = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("{base}[keys.leader]\nnew_tab = [\"y\"]\n")).unwrap();
    let out = env.try_run(&["config", "reload"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    typed(b"\x02y");
    wait_for(2);
    // A file that cannot be read is an error, and the keys in use stay.
    std::fs::write(&path, "keys = [").unwrap();
    let out = env.try_run(&["config", "reload"]);
    assert!(!out.status.success(), "a broken file was accepted");
    typed(b"\x02y");
    wait_for(3);
    let _ = client.kill();
    let _ = client.wait();
}

#[test]
fn config_check_reports_tool_problems() {
    let env = Env::new("toolcheck");
    let cfg = env.root.join("config/kraal/config.toml");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str("\n[tools.mine]\ncommand = \"tig\"\nkey = \"g\"\n[tools.both]\ncommand = \"true\"\nwhen = [\"server\", \"local\"]\n");
    std::fs::write(&cfg, text).unwrap();
    let out = env.try_run(&["config", "check"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("tools.mine: key \"R g\" is already \"Git and worktrees\""), "{err}");
    assert!(err.contains("tools.both: when"), "{err}");
    // A workspace file that cannot be read is an error, not a warning.
    env.workspace("broken", "layout {\n    pane {\n");
    let out = env.try_run(&["config", "check"]);
    assert!(!out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(String::from_utf8_lossy(&out.stderr).contains("broken.kdl"));
}

/// `kraal config check` run in `dir`: whether it passed, and what it said on stderr.
fn config_check(env: &Env, dir: &Path) -> (bool, String) {
    let out = env.cmd(&["config", "check"]).current_dir(dir).output().unwrap();
    (out.status.success(), String::from_utf8(out.stderr).unwrap())
}

/// A workspace named like a command of kraal: its name alone runs the command, so the check
/// names the file and the way to open it; `kraal open` takes it like any other.
#[test]
fn the_check_names_a_workspace_that_a_command_hides() {
    let env = Env::new("wsname");
    for name in ["files", "shop"] {
        env.workspace(name, "layout {\n    pane name=\"main\"\n}\n");
    }
    let (ok, said) = config_check(&env, &env.root);
    assert!(ok, "{said}");
    let advice =
        "workspaces/files.kdl: `kraal files` is a command of kraal: open this workspace with `kraal attach files`";
    assert!(said.lines().count() == 1 && said.trim_end().ends_with(advice), "{said}");
    env.run(&["open", "files"]);
    let workspaces: serde_json::Value = serde_json::from_str(&env.run(&["workspaces", "--json"])).unwrap();
    let open = |name: &str| workspaces.as_array().unwrap().iter().any(|w| w["name"] == name && w["open"] == true);
    assert!(open("files") && !open("shop"), "{workspaces}");
}

/// What `kraal config default` prints is a file its own check has nothing to say about, in
/// either preset.
#[test]
fn the_default_config_passes_its_own_check() {
    let env = Env::new("defaultcheck");
    let default = env.run(&["config", "default"]);
    for preset in ["kraal", "classic"] {
        let config = default.replace("preset = \"kraal\"", &format!("preset = \"{preset}\""));
        assert!(config.contains(&format!("preset = \"{preset}\"")), "{config}");
        std::fs::write(env.root.join("config/kraal/config.toml"), config).unwrap();
        assert_eq!(config_check(&env, &env.root), (true, String::new()), "preset {preset}");
    }
}

/// A key kraal does not read is a warning wherever configuration problems are told (the check,
/// a session's start, a reload) and never an error: the rest of the file applies, since the
/// file may also serve a kraal that reads that key.
#[test]
fn unknown_config_keys_are_warnings_and_never_stop_a_session() {
    for preset in ["kraal", "classic"] {
        let env = Env::new(&format!("unknown-{preset}"));
        std::fs::create_dir_all(env.root.join("state/kraal")).unwrap();
        std::fs::write(env.root.join("state/kraal/welcomed"), "").unwrap();
        let path = env.root.join("config/kraal/config.toml");
        let base = std::fs::read_to_string(&path).unwrap();
        let write = |more: &str| {
            // A tool with a key of another kraal's is in the file too: it loads without it.
            let tool = "[tools.pods]\ncommand = \"true\"\nread_onyl = true\n";
            let text = format!("{base}future_setting = 1\n[keys]\npreset = \"{preset}\"\n{tool}{more}");
            std::fs::write(&path, text).unwrap();
        };
        write("");
        let unknown = "warning: future_setting: unknown key\n\
                       warning: tools.pods.read_onyl: unknown key (did you mean read_only?)\n";
        assert_eq!(config_check(&env, &env.root), (true, unknown.into()));

        // The session starts with the shell the file sets, and says it to who attaches.
        env.workspace("w", "layout {\n    pane name=\"first\"\n}\n");
        env.run(&["open", "w"]);
        env.wait_for_text("first", "$");
        let client = RawClient::attach(&env, "w");
        client.wait_for_screen("future_setting: unknown key");
        let rows = || {
            let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
            panes[0]["rows"].as_u64().unwrap()
        };
        let with_bar = rows();

        // A reload says it too, and applies the rest: the panes get the bottom bar's row.
        write("[bar]\nkeys = \"off\"\nkyes = \"context\"\n");
        let out = env.try_run(&["config", "reload"]);
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(
            String::from_utf8_lossy(&out.stderr),
            format!("warning: bar.kyes: unknown key (did you mean keys?)\n{unknown}")
        );
        wait_until(Duration::from_secs(10), "the reload did not apply", || rows() == with_bar + 1);
    }
}

/// The check reads every file kraal reads: the inventories, where a key it does not read is a
/// warning too, your runbooks, and what the repository it runs in ships.
#[test]
fn config_check_reads_the_inventories_the_runbooks_and_the_repository() {
    let env = Env::new("checkall");
    let config = env.root.join("config/kraal");
    let (servers, accounts) = (config.join("servers.toml"), config.join("accounts.toml"));
    std::fs::write(&servers, "[[server]]\nname = \"web\"\nhost = \"web.example.com\"\njmp = \"bastion\"\n").unwrap();
    std::fs::write(&accounts, "[limits]\ntreshold = 80\n").unwrap();
    let runbooks = config.join("runbooks");
    std::fs::create_dir_all(&runbooks).unwrap();
    std::fs::write(runbooks.join("reload.toml"), "steps = [\"true\"]\n").unwrap();
    std::fs::write(runbooks.join("notes.md"), "# Notes\n\n```sh\ntrue\n```\n").unwrap();
    let repo = env.root.join("repo");
    let _git = git_repo(
        &repo,
        &[(".kraal/worktree.toml", "copy = [\".env\"]\n"), (".kraal/runbooks/deploy.toml", "steps = [\"true\"]\n")],
    );
    let warnings = format!(
        "warning: {}: limits.treshold: unknown key (did you mean threshold?)\n\
         warning: {}: server[0].jmp: unknown key (did you mean jump?)\n",
        accounts.display(),
        servers.display()
    );
    assert_eq!(config_check(&env, &repo), (true, warnings.clone()));

    // A file kraal would refuse is an error: one of your runbooks, wherever the check runs.
    std::fs::write(runbooks.join("broken.toml"), "stpes = [\"true\"]\n").unwrap();
    let (ok, said) = config_check(&env, &env.root);
    assert!(!ok && said.contains("runbooks/broken.toml") && said.contains("unknown field `stpes`"), "{said}");
    std::fs::remove_file(runbooks.join("broken.toml")).unwrap();

    // The repository's files, in that repository only.
    std::fs::write(repo.join(".kraal/worktree.toml"), "setpu = [\"npm ci\"]\n").unwrap();
    std::fs::write(repo.join(".kraal/runbooks/deploy.toml"), "steps = []\n").unwrap();
    let (ok, said) = config_check(&env, &repo);
    assert!(!ok && said.contains(".kraal/worktree.toml: ") && said.contains("unknown field `setpu`"), "{said}");
    assert!(said.contains(".kraal/runbooks/deploy.toml: no steps"), "{said}");
    assert!(said.ends_with("kraal: 2 files could not be read\n"), "{said}");
    assert_eq!(config_check(&env, &env.root), (true, warnings));
}
