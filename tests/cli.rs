//! End-to-end tests: a real server driven through the CLI, in isolated directories.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

struct Env {
    root: PathBuf,
    session: String,
}

impl Env {
    fn new(name: &str) -> Env {
        let root = std::env::temp_dir().join(format!("kraal-e2e-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["run", "state", "config/kraal/workspaces"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        // A quiet, predictable shell.
        let sh = root.join("sh");
        std::fs::write(&sh, "#!/bin/sh\nexport PS1='$ '\nexec sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&sh, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(
            root.join("config/kraal/config.toml"),
            format!("shell = {:?}\nssh_config_hosts = false\n", sh.display().to_string()),
        )
        .unwrap();
        Env { root, session: name.to_string() }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_kraal"));
        c.args(["-s", &self.session]).args(args);
        c.env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env_remove("KRAAL_PANE")
            .env_remove("KRAAL_SOCKET")
            .env_remove("KRAAL_SESSION_NAME");
        c
    }

    fn run(&self, args: &[&str]) -> String {
        let out = self.cmd(args).output().unwrap();
        assert!(out.status.success(), "kraal {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }

    fn try_run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    fn workspace(&self, name: &str, kdl: &str) {
        std::fs::write(self.root.join("config/kraal/workspaces").join(format!("{name}.kdl")), kdl).unwrap();
    }

    fn wait_for_text(&self, pane: &str, text: &str) -> String {
        let start = Instant::now();
        loop {
            let screen = self.run(&["read", "-p", pane]);
            if screen.contains(text) {
                return screen;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "timed out waiting for {text:?} in:\n{screen}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn state_file(&self) -> PathBuf {
        self.root.join("state/kraal/sessions").join(format!("{}.json", self.session))
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.try_run(&["kill"]);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn wait_gone(path: &Path) {
    let start = Instant::now();
    while path.exists() && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
}

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
fn hooks_drive_agent_state() {
    let env = Env::new("hooks");
    env.workspace("w", "layout {\n    pane name=\"agent\"\n}\n");
    env.run(&["open", "w"]);
    let socket = env.root.join("run/kraal/hooks.sock");
    let hook = |payload: &str| {
        let mut c = env.cmd(&["hook", "claude"]);
        c.env("KRAAL_PANE", "agent").env("KRAAL_SOCKET", &socket).stdin(std::process::Stdio::piped());
        let mut child = c.spawn().unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        assert!(out.stdout.is_empty(), "hooks must not print");
    };
    hook(r#"{"hook_event_name":"SessionStart","session_id":"s-1"}"#);
    hook(r#"{"hook_event_name":"UserPromptSubmit","session_id":"s-1"}"#);
    assert!(env.run(&["agents"]).contains("working"));
    hook(r#"{"hook_event_name":"Notification","message":"Claude needs your permission to use Bash"}"#);
    assert_eq!(env.run(&["wait", "-p", "agent", "--until", "blocked", "--timeout", "2"]).trim(), "blocked");
    hook(r#"{"hook_event_name":"Stop","session_id":"s-1"}"#);
    assert_eq!(env.run(&["wait", "-p", "agent", "--until", "done", "--timeout", "2"]).trim(), "done");
    let timeout = env.try_run(&["wait", "-p", "agent", "--until", "working", "--timeout", "0.2"]);
    assert!(!timeout.status.success());
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
    tab name="notes" {
        pane name="shell"
    }
}
"#,
    );
    env.run(&["open", "fleet"]);
    env.wait_for_text("b", "node-b");
    env.run(&["rename", "-p", "shell", "renamed"]);
    env.run(&["kill"]);
    wait_gone(&env.root.join("run/kraal/restore.sock"));
    assert!(env.state_file().exists());
    // Any command starting the server restores the saved layout.
    env.run(&["open", "fleet"]);
    env.wait_for_text("a", "node-a");
    let panes = env.run(&["panes"]);
    assert!(panes.contains("renamed"), "{panes}");
    assert_eq!(panes.lines().count(), 3, "{panes}");
}

#[test]
fn save_writes_a_definition_that_reopens() {
    let env = Env::new("save");
    env.workspace("w", "layout {\n    pane split_direction=\"vertical\" {\n        pane name=\"left\"\n        pane name=\"right\"\n    }\n}\n");
    env.run(&["open", "w"]);
    let out = env.run(&["save", "copy", "--workspace", "w"]);
    assert!(out.contains("copy.kdl"), "{out}");
    let list = env.run(&["workspaces"]);
    assert!(list.contains("copy"), "{list}");
    env.run(&["open", "copy"]);
    let panes = env.run(&["panes"]);
    assert_eq!(panes.matches("left").count(), 2, "{panes}");
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

#[test]
fn server_inventory_and_copy_plans() {
    let env = Env::new("servers");
    env.workspace(
        "lab",
        "layout {\n    pane split_direction=\"vertical\" {\n        pane name=\"web\" command=\"ssh\" { args \"-p\" \"2222\" \"admin@web.lab.invalid\"; }\n        pane name=\"db\" command=\"ssh\" { args \"admin@db.lab.invalid\"; }\n    }\n}\n",
    );
    let list = env.run(&["servers"]);
    assert!(
        list.contains("web") && list.contains("admin@web.lab.invalid:2222") && list.contains("workspace lab"),
        "{list}"
    );
    env.run(&["servers", "add", "ops@bastion.invalid", "--group", "infra"]);
    env.run(&["servers", "import"]);
    let list = env.run(&["servers", "list", "--group", "lab"]);
    assert_eq!(list.lines().count(), 2, "{list}");
    assert!(list.contains("saved"), "imported servers are saved: {list}");
    let json = env.run(&["servers", "list", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 3);

    // Copy plans, without touching the network.
    let file = env.root.join("report.txt");
    std::fs::write(&file, "x").unwrap();
    let up = env.run(&["cp", "--dry-run", file.to_str().unwrap(), "web:/tmp/"]);
    assert!(up.contains("-p 2222") && up.contains("admin@web.lab.invalid:/tmp/"), "{up}");
    let s2s = env.run(&["cp", "--dry-run", "web:/etc/hosts", "db:~/in/"]);
    assert!(s2s.contains("tar cf -") && s2s.contains("admin@db.lab.invalid"), "{s2s}");
    let bad = env.try_run(&["cp", "nosuch:/x", "."]);
    assert!(String::from_utf8_lossy(&bad.stderr).contains("unknown server"));

    env.run(&["servers", "rm", "bastion"]);
    assert!(!env.run(&["servers"]).contains("bastion"));
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

#[test]
fn restore_suspends_commands_but_reconnects_ssh() {
    let env = Env::new("restoresafe");
    // A fake `ssh` that just prints, to stand for a reconnecting SSH pane.
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let ssh = bin.join("ssh");
    std::fs::write(&ssh, "#!/bin/sh\necho \"connected $*\"\nexec sleep 1000\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    env.workspace(
        "w",
        &format!(
            "layout {{\n    pane split_direction=\"vertical\" {{\n        pane name=\"srv\" command=\"{}\" {{ args \"admin@db\"; }}\n        pane name=\"deploy\" command=\"sh\" {{ args \"-c\" \"echo DEPLOYED; exec sleep 1000\"; }}\n    }}\n}}\n",
            ssh.display()
        ),
    );
    env.run(&["open", "w"]);
    env.wait_for_text("deploy", "DEPLOYED");
    env.wait_for_text("srv", "connected admin@db");
    env.run(&["kill"]);
    wait_gone(&env.root.join("run/kraal/restoresafe.sock"));
    env.run(&["open", "w"]);
    env.wait_for_text("srv", "connected admin@db");
    let screen = env.wait_for_text("deploy", "restored session");
    assert!(!screen.lines().any(|l| l.trim() == "DEPLOYED"), "the command must not run again by itself:\n{screen}");
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
fn worktrees_for_agents() {
    let env = Env::new("worktrees");
    let repo = env.root.join("app");
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
    std::fs::write(repo.join("README.md"), "hi\n").unwrap();
    git(&["add", "README.md"]);
    git(&["commit", "-q", "-m", "init"]);
    let wt = |args: &[&str]| env.cmd(&[&["wt"], args].concat()).current_dir(&repo).output().unwrap();
    let out = wt(&["new", "fix-login"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let path = env.root.join("app.worktrees/fix-login");
    assert!(path.join("README.md").exists());
    let list = String::from_utf8(wt(&["list"]).stdout).unwrap();
    assert!(list.contains("fix-login") && list.contains("kraal/fix-login"), "{list}");
    std::fs::write(path.join("README.md"), "changed\n").unwrap();
    assert!(!wt(&["rm", "fix-login"]).status.success(), "uncommitted changes need --force");
    assert!(wt(&["rm", "fix-login", "--force"]).status.success());
    assert!(!path.exists());
}
