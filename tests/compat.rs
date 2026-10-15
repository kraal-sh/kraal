//! Cross-version tests: this kraal with a session that another release started, and that
//! release refused by a session this kraal started. `KRAAL_OLD` is the other release's
//! binary: `tests/compat.sh` fetches the releases kraal upgrades from and runs these with
//! each of them.

/// Where the tests run, as the other end-to-end tests do.
#[path = "common/isolation.rs"]
mod isolation;

#[path = "common/script.rs"]
mod script;
use script::write_script;

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

const SKIP: &str = "needs KRAAL_OLD, another release's kraal: tests/compat.sh runs it";

/// A kraal binary and its release.
struct Kraal {
    bin: PathBuf,
    version: String,
}

impl Kraal {
    fn this() -> Kraal {
        Kraal { bin: env!("CARGO_BIN_EXE_kraal").into(), version: env!("CARGO_PKG_VERSION").into() }
    }

    fn old() -> Kraal {
        let bin = PathBuf::from(std::env::var_os("KRAAL_OLD").expect(SKIP));
        let out = Command::new(&bin).arg("--version").output().unwrap();
        let said = String::from_utf8_lossy(&out.stdout);
        let version = said.trim().trim_start_matches("kraal ").to_string();
        assert!(out.status.success() && !version.is_empty(), "{}: {said}", bin.display());
        Kraal { bin, version }
    }
}

/// A session in directories of its own, which kraal of either release drives.
struct Env {
    root: PathBuf,
    session: String,
}

impl Env {
    fn new(name: &str) -> Env {
        isolation::check();
        // Under /tmp: macOS's temporary directory is too long for the sockets kraal makes there.
        let root = PathBuf::from("/tmp").join(format!("kraal-compat-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["home", "run", "state", "config/kraal/workspaces"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        let sh = root.join("sh");
        write_script(&sh, "#!/bin/sh\nexport PS1='$ '\nexec sh\n");
        std::fs::write(
            root.join("config/kraal/config.toml"),
            format!("shell = {:?}\nssh_config_hosts = false\n", sh.display().to_string()),
        )
        .unwrap();
        std::fs::write(root.join("config/kraal/workspaces/w.kdl"), "layout {\n    pane name=\"first\"\n}\n").unwrap();
        Env { root, session: name.to_string() }
    }

    fn cmd(&self, kraal: &Kraal, args: &[&str]) -> Command {
        let mut c = Command::new(&kraal.bin);
        c.env("HOME", self.root.join("home"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .args(["-s", &self.session])
            .args(args);
        c
    }

    fn try_run(&self, kraal: &Kraal, args: &[&str]) -> Output {
        self.cmd(kraal, args).output().unwrap()
    }

    fn run(&self, kraal: &Kraal, args: &[&str]) -> String {
        let out = self.try_run(kraal, args);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "kraal {} {args:?} failed: {err}", kraal.version);
        String::from_utf8(out.stdout).unwrap()
    }

    fn socket(&self) -> PathBuf {
        self.root.join("run/kraal").join(format!("{}.sock", self.session))
    }

    /// The panes, as `kraal panes --json` lists them.
    fn panes(&self, kraal: &Kraal) -> Vec<serde_json::Value> {
        let listed = self.run(kraal, &["panes", "--json"]);
        serde_json::from_str::<serde_json::Value>(&listed).unwrap().as_array().unwrap().clone()
    }

    fn wait_for_text(&self, kraal: &Kraal, pane: &str, text: &str) {
        let start = Instant::now();
        loop {
            let screen = self.run(kraal, &["read", "-p", pane, "--lines", "100"]);
            if screen.contains(text) {
                return;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "{text:?} never shown in {pane}:\n{screen}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn refused_attach(&self, kraal: &Kraal, proxy: Option<&Kraal>) -> String {
        let mut command = self.cmd(kraal, if proxy.is_some() { &["attach", "--host", "box"] } else { &["attach"] });
        if let Some(proxy) = proxy {
            let bin = self.root.join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            write_script(&bin.join("ssh"), "#!/bin/sh\nexec \"$COMPAT_PROXY\" -s \"$COMPAT_SESSION\" proxy\n");
            command
                .env("PATH", format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default()))
                .env("COMPAT_PROXY", &proxy.bin)
                .env("COMPAT_SESSION", &self.session);
        }
        let pair = portable_pty::native_pty_system()
            .openpty(portable_pty::PtySize { rows: 30, cols: 100, pixel_width: 0, pixel_height: 0 })
            .unwrap();
        let mut cmd = portable_pty::CommandBuilder::new(command.get_program());
        cmd.args(command.get_args());
        for (key, value) in command.get_envs() {
            match value {
                Some(value) => cmd.env(key, value),
                None => cmd.env_remove(key),
            }
        }
        let mut child = pair.slave.spawn_command(cmd).unwrap();
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().unwrap();
        let read = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let mut buf = [0; 8192];
            while let Ok(n @ 1..) = reader.read(&mut buf) {
                bytes.extend_from_slice(&buf[..n]);
            }
            String::from_utf8_lossy(&bytes).into_owned()
        });
        let start = Instant::now();
        while child.try_wait().unwrap().is_none() {
            if start.elapsed() > Duration::from_secs(20) {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("mismatched client did not exit");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        read.join().unwrap()
    }

    /// The pid of the server listening on this session's socket.
    fn server_pid(&self) -> u32 {
        self.pong()["pid"].as_u64().unwrap() as u32
    }

    /// What the server says of itself (its answer to a ping), asked as any release asks.
    fn pong(&self) -> serde_json::Value {
        self.ask(serde_json::json!({ "cmd": "ping" }))["result"]["Ok"]["data"].clone()
    }

    /// The server's answer to `request`, asked through its socket as any release asks.
    fn ask(&self, request: serde_json::Value) -> serde_json::Value {
        self.ask_version(2, request)
    }

    fn ask_version(&self, version: u64, request: serde_json::Value) -> serde_json::Value {
        let mut stream = std::os::unix::net::UnixStream::connect(self.socket()).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let message = serde_json::json!({ "type": "request", "version": version, "request": request }).to_string();
        let mut frame = (message.len() as u32).to_le_bytes().to_vec();
        frame.push(0);
        frame.extend_from_slice(message.as_bytes());
        stream.write_all(&frame).unwrap();
        let mut header = [0u8; 5];
        stream.read_exact(&mut header).unwrap();
        let mut payload = vec![0u8; u32::from_le_bytes(header[..4].try_into().unwrap()) as usize];
        stream.read_exact(&mut payload).unwrap();
        serde_json::from_slice(&payload).unwrap()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.try_run(&Kraal::this(), &["kill"]);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Both real clients refuse a server of the other generation without changing its session.
#[test]
#[ignore = "needs KRAAL_OLD, another release's kraal: tests/compat.sh runs it"]
fn different_protocols_refuse_attachment_in_both_directions() {
    let (old, this) = (Kraal::old(), Kraal::this());
    for (name, server, client, cp, sp) in [("refuse-new", &old, &this, 6, 2), ("refuse-old", &this, &old, 2, 6)] {
        let env = Env::new(name);
        env.run(server, &["open", "w"]);
        env.run(server, &["send", "-p", "first", "--enter", "echo", "kept-$((40+2))"]);
        env.wait_for_text(server, "first", "kept-42");
        let pid = env.server_pid();
        for remote in [false, true] {
            let shown = env.refused_attach(client, remote.then_some(server));
            assert!(shown.contains(&format!("client protocol {cp}, server protocol {sp}")), "{shown}");
            let fix = if cp < sp {
                "kraal update".into()
            } else if remote {
                format!("kraal -s {name} remote-install box")
            } else {
                format!("kraal -s {name} restart")
            };
            assert!(shown.contains(&fix), "{shown}");
            if cp > sp {
                assert!(!shown.contains("\x1b[?1049h"), "refusal must precede terminal setup: {shown}");
            }
        }
        assert_eq!(env.server_pid(), pid);
        env.wait_for_text(server, "first", "kept-42");
        let refused = env.try_run(client, &["run", "--name", "forbidden", "--", "sh"]);
        assert!(!refused.status.success());
        assert!(String::from_utf8_lossy(&refused.stderr).contains("protocol mismatch"));
        assert!(!env.panes(server).iter().any(|p| p["name"] == "forbidden"));
        if cp > sp {
            let listed = env.try_run(client, &["workspaces"]);
            assert!(!listed.status.success());
            assert!(String::from_utf8_lossy(&listed.stderr).contains("protocol mismatch"));
        }
    }
}

#[test]
#[ignore = "needs KRAAL_OLD, another release's kraal: tests/compat.sh runs it"]
fn this_kraal_stops_an_old_server_and_restores_its_saved_session() {
    let (old, this) = (Kraal::old(), Kraal::this());
    let env = Env::new("compat-kill");
    env.run(&old, &["open", "w"]);
    env.run(&old, &["rename", "-p", "first", "kept"]);
    env.run(&old, &["send", "-p", "kept", "--enter", "echo", "before-$((40+2))"]);
    env.wait_for_text(&old, "kept", "before-42");
    let stopped = env.run(&this, &["kill"]);
    assert!(stopped.contains("stopped session compat-kill"), "{stopped}");
    assert!(std::os::unix::net::UnixStream::connect(env.socket()).is_err());
    let saved = env.root.join("state/kraal/sessions/compat-kill.json");
    let original = std::fs::read(&saved).unwrap();
    env.run(&this, &["open", "w"]);
    env.wait_for_text(&this, "kept", "before-42");
    assert_eq!(std::fs::read(saved).unwrap(), original);
}

/// A session one release runs, and listens for, is neither started again nor forgotten by the
/// other in the runtime directory they share, whichever of the two runs it: one command after
/// the other here, not what two of them do at the same instant.
#[test]
#[ignore = "needs KRAAL_OLD, another release's kraal: tests/compat.sh runs it"]
fn neither_release_starts_or_forgets_a_session_the_other_runs() {
    let (old, this) = (Kraal::old(), Kraal::this());
    for (name, runs, other) in [("compat-held-a", &old, &this), ("compat-held-b", &this, &old)] {
        let env = Env::new(name);
        env.run(runs, &["open", "w"]);
        let pid = env.server_pid();
        let dir = if runs.bin == this.bin { "sessions-v2" } else { "sessions" };
        let saved = env.root.join("state/kraal").join(dir).join(format!("{name}.json"));
        let start = Instant::now();
        while !saved.exists() {
            assert!(start.elapsed() < Duration::from_secs(60), "the layout was never saved");
            std::thread::sleep(Duration::from_millis(50));
        }
        // Each ends by itself, refused: a second server that started would run until it is
        // killed here, and so would a `forget` that waited.
        let refused = |args: &[&str]| {
            let mut command = env.cmd(other, args).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
            let start = Instant::now();
            loop {
                if let Some(status) = command.try_wait().unwrap() {
                    break !status.success();
                }
                if start.elapsed() > Duration::from_secs(60) {
                    let _ = command.kill();
                    let _ = command.wait();
                    break false;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        let (theirs, ours) = (&other.version, &runs.version);
        assert!(refused(&["server", "--session", name]), "kraal {theirs} started a second server beside {ours}");
        assert!(refused(&["forget"]) && saved.exists(), "kraal {theirs} forgot a session kraal {ours} runs");
        assert_eq!(env.server_pid(), pid, "the session's server");
        env.run(runs, &["kill"]);
    }
}

/// Both supported releases must keep their snapshots and name-based workspace rules when
/// this kraal adds account identities to the shared file.
#[test]
#[ignore = "needs KRAAL_OLD, another release's kraal: tests/compat.sh runs it"]
fn account_migration_preserves_running_release_snapshots_and_rules() {
    let (old, this) = (Kraal::old(), Kraal::this());
    let env = Env::new("compat-accounts");
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(&bin.join("claude"), "#!/bin/sh\necho \"account-ready:$CLAUDE_CONFIG_DIR\"\nexec sleep 1000\n");
    let home = env.root.join("work-home");
    std::fs::create_dir_all(&home).unwrap();
    let file = env.root.join("config/kraal/accounts.toml");
    let source = format!(
        "# Kept from the old release.\n[[account]]\nname = 'work'\nagent = 'claude'\nhome = {:?}\n[[rule]]\nworkspace = 'w'\naccounts = ['work']\n",
        home.to_str().unwrap()
    );
    std::fs::write(&file, &source).unwrap();
    env.run(&old, &["accounts", "limits", "--threshold", "85"]);
    let source = std::fs::read_to_string(&file).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let out = env.cmd(&old, &["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stderr),
        std::fs::read_to_string(env.root.join("state/kraal/server-compat-accounts.log")).unwrap_or_default()
    );
    let open_agent = || {
        let answer = env.ask_version(
            env.pong()["protocol"].as_u64().unwrap_or(2),
            serde_json::json!({ "cmd": "open_agent", "agent": "claude", "account": null,
            "placement": "tab", "near": "first", "cwd": null }),
        );
        assert!(answer["result"]["Ok"].is_object(), "kraal {}: {answer}", old.version);
    };
    open_agent();
    let saved = env.root.join("state/kraal/sessions/compat-accounts.json");
    let start = Instant::now();
    while !std::fs::read_to_string(&saved).is_ok_and(|s| s.contains("claude:work")) {
        assert!(start.elapsed() < Duration::from_secs(60), "the old server never saved its agent");
        std::thread::sleep(Duration::from_millis(50));
    }
    // Keep the old server listening but prevent its next write from hiding an erroneous
    // migration of this snapshot. Always resume it before Env shuts it down.
    struct Stopped(libc::pid_t);
    impl Drop for Stopped {
        fn drop(&mut self) {
            // SAFETY: the server belongs to this test.
            unsafe {
                libc::kill(self.0, libc::SIGCONT);
            }
        }
    }
    let pid = env.server_pid() as libc::pid_t;
    // SAFETY: the server belongs to this test.
    assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
    let stopped = Stopped(pid);
    let before = std::fs::read(&saved).unwrap();
    env.run(&this, &["accounts", "limits", "--threshold", "75"]);
    assert_eq!(
        std::fs::read(&saved).unwrap(),
        before,
        "a running {} owns its snapshot, even without a session lock",
        old.version
    );
    drop(stopped);
    let accounts: toml::Value = toml::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    let id = accounts["account"][0]["id"].as_str().unwrap();
    assert_eq!(accounts["rule"][0]["accounts"][0].as_str(), Some("work"));
    assert_eq!(accounts["rule"][0]["account_ids"][0].as_str(), Some(format!("claude:@{id}").as_str()));
    assert_eq!(std::fs::read_to_string(file.with_extension("toml.kraal-bak")).unwrap(), source);
    // The replacement server needs the same mock agent on PATH as the old one.
    let out = env.cmd(&this, &["restart", "--yes"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    open_agent();
    env.run(&this, &["accounts", "limits", "--threshold", "70"]);
    assert_eq!(std::fs::read_to_string(file.with_extension("toml.kraal-bak")).unwrap(), source);
    let agents: Vec<_> = env.panes(&this).into_iter().filter(|p| p["command"][0] == "claude").collect();
    assert_eq!(agents.len(), 2, "both agents use the workspace rule before and after migration");
    for pane in agents {
        env.wait_for_text(&this, &pane["id"].to_string(), &format!("account-ready:{}", home.display()));
    }
}

/// `kraal restart` brings a session of the other release to this kraal, with its panes and what
/// they showed.
#[test]
#[ignore = "needs KRAAL_OLD, another release's kraal: tests/compat.sh runs it"]
fn a_session_of_another_release_restarts_on_this_kraal() {
    let (old, this) = (Kraal::old(), Kraal::this());
    let env = Env::new("compat-restart");
    env.run(&old, &["open", "w"]);
    env.run(&old, &["send", "-p", "first", "--enter", "echo", "before-$((40+2))"]);
    env.wait_for_text(&old, "first", "before-42");
    let before = env.server_pid();
    let listed = env.run(&this, &["ls"]);
    assert!(listed.contains(&old.version) && listed.contains(&format!("kraal -s {} restart", env.session)), "{listed}");
    let out = env.run(&this, &["restart", "--yes"]);
    assert!(out.contains(&format!("restarted session {}", env.session)), "{out}");
    assert_ne!(env.server_pid(), before, "the same server still runs");
    let pong = env.pong();
    assert_eq!(pong["version"], this.version.as_str(), "{pong}");
    assert_eq!(pong["protocol"], 6, "the session runs this kraal: {pong}");
    env.wait_for_text(&this, "first", "before-42");
    env.run(&this, &["send", "-p", "first", "--enter", "echo", "after-$((40+2))"]);
    env.wait_for_text(&this, "first", "after-42");
    // This server writes the newer state itself, so later restarts keep using it.
    env.run(&this, &["restart", "--yes"]);
    env.wait_for_text(&this, "first", "after-42");
    let ls = env.run(&this, &["ls"]);
    assert!(ls.contains(&format!("running  {}", this.version)), "{ls}");
}

fn saved_scrollback(env: &Env) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    let dir = env.root.join("state/kraal/scrollback-v2").join(&env.session);
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .map(|entry| {
            let path = entry.unwrap().path();
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect()
}

#[test]
#[ignore = "needs KRAAL_OLD, another release's kraal: tests/compat.sh runs it"]
fn a_forgotten_legacy_session_cannot_restart_until_its_marker_is_removed() {
    let (old, this) = (Kraal::old(), Kraal::this());
    let env = Env::new("compat-forgotten");
    env.run(&old, &["open", "w"]);
    env.run(&old, &["send", "-p", "first", "--enter", "echo", "before-$((40+2))"]);
    env.wait_for_text(&old, "first", "before-42");
    env.run(&old, &["kill"]);
    let legacy = env.root.join("state/kraal/sessions").join(format!("{}.json", env.session));
    let original = std::fs::read(&legacy).unwrap();
    env.run(&this, &["forget"]);
    assert_eq!(std::fs::read(&legacy).unwrap(), original);
    let current = env.root.join("state/kraal/sessions-v2").join(format!("{}.json", env.session));
    let marker = current.with_extension("forgotten");
    assert!(marker.is_file());
    assert!(!current.exists());
    env.run(&old, &["open", "w"]);
    env.wait_for_text(&old, "first", "before-42");
    env.run(&old, &["rename", "-p", "first", "reopened"]);
    env.run(&old, &["send", "-p", "reopened", "--enter", "echo", "reopened-$((40+2))"]);
    env.wait_for_text(&old, "reopened", "reopened-42");
    let pid = env.server_pid();
    let panes = env.panes(&old);
    for args in [&["restart"][..], &["restart", "--yes"][..]] {
        let out = env.try_run(&this, args);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "restart discarded the forgotten legacy session: {err}");
        assert!(err.contains(&format!("not restarting session {}", env.session)), "{err}");
        assert!(err.contains("was forgotten") && err.contains(&*marker.to_string_lossy()), "{err}");
        assert!(err.contains("instead of this running session's layout and scrollback"), "{err}");
        assert!(err.contains("remove") && err.contains("import the legacy state"), "{err}");
        assert!(err.contains(&format!("kraal -s {} kill", env.session)), "{err}");
        assert!(!String::from_utf8_lossy(&out.stdout).contains("with its layout, scrollback"));
        assert_eq!(env.server_pid(), pid, "the old server was stopped");
        assert_eq!(env.panes(&old), panes);
        assert!(marker.is_file());
        assert!(!current.exists());
    }
    env.run(&old, &["send", "-p", "reopened", "--enter", "echo", "still-$((40+2))"]);
    env.wait_for_text(&old, "reopened", "still-42");
    // Follow the recovery guidance to import the old server's latest layout and scrollback.
    env.run(&this, &["kill"]);
    std::fs::remove_file(marker).unwrap();
    env.run(&this, &["open", "w"]);
    let restored = env.panes(&this);
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0]["name"], "reopened");
    for text in ["before-42", "reopened-42", "still-42"] {
        env.wait_for_text(&this, "reopened", text);
    }
}

#[test]
#[ignore = "needs KRAAL_OLD, another release's kraal: tests/compat.sh runs it"]
fn a_downgrade_restores_the_upgrade_copy_and_cannot_overwrite_new_state() {
    let (old, this) = (Kraal::old(), Kraal::this());
    let env = Env::new("compat-downgrade");
    let config = env.root.join("config/kraal/config.toml");
    let definition = env.root.join("config/kraal/workspaces/w.kdl");
    let original_files = [std::fs::read(&config).unwrap(), std::fs::read(&definition).unwrap()];
    env.run(&old, &["open", "w"]);
    env.run(&old, &["rename", "-p", "first", "at-upgrade"]);
    env.run(&old, &["send", "-p", "at-upgrade", "--enter", "echo", "upgrade-$((40+2))"]);
    env.wait_for_text(&old, "at-upgrade", "upgrade-42");
    env.run(&old, &["kill"]);
    let legacy = env.root.join("state/kraal/sessions").join(format!("{}.json", env.session));
    let original = std::fs::read(&legacy).unwrap();
    let legacy_scrollback = || {
        std::fs::read_dir(env.root.join("state/kraal/scrollback").join(&env.session))
            .unwrap()
            .map(|e| {
                let e = e.unwrap();
                (e.file_name(), std::fs::read(e.path()).unwrap())
            })
            .collect::<std::collections::BTreeMap<_, _>>()
    };
    let original_scrollback = legacy_scrollback();
    assert!(!original_scrollback.is_empty());
    env.run(&this, &["open", "w"]);
    env.wait_for_text(&this, "at-upgrade", "upgrade-42");
    env.run(&this, &["rename", "-p", "at-upgrade", "after-upgrade"]);
    env.run(&this, &["send", "-p", "after-upgrade", "--enter", "echo", "new-$((40+2))"]);
    env.wait_for_text(&this, "after-upgrade", "new-42");
    env.run(&this, &["kill"]);
    let current = env.root.join("state/kraal/sessions-v2").join(format!("{}.json", env.session));
    let saved = std::fs::read(&current).unwrap();
    let state: serde_json::Value = serde_json::from_slice(&saved).unwrap();
    assert_eq!(state["version"], 2);
    assert!(state.get("revision").is_none());
    let scrollback = saved_scrollback(&env);
    assert!(!scrollback.is_empty());
    assert_eq!(std::fs::read(&legacy).unwrap(), original);
    assert_eq!(legacy_scrollback(), original_scrollback);
    assert_eq!(std::fs::read_dir(legacy.parent().unwrap()).unwrap().count(), 1);

    env.run(&old, &["open", "w"]);
    let panes = env.panes(&old);
    assert_eq!(panes.len(), 1);
    assert_eq!(panes[0]["name"], "at-upgrade");
    env.wait_for_text(&old, "at-upgrade", "upgrade-42");
    env.run(&old, &["rename", "-p", "at-upgrade", "old-writer"]);
    env.run(&old, &["send", "-p", "old-writer", "--enter", "echo", "downgraded-$((40+2))"]);
    env.wait_for_text(&old, "old-writer", "downgraded-42");
    let pid = env.server_pid();
    // A restart must explain the separate saved states before stopping the downgraded server.
    for args in [&["restart"][..], &["restart", "--yes"][..]] {
        let out = env.try_run(&this, args);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "restart discarded the downgraded session: {err}");
        assert!(err.contains(&format!("not restarting session {}", env.session)), "{err}");
        assert!(err.contains("newer saved state") && err.contains(&*current.to_string_lossy()), "{err}");
        assert!(err.contains("instead of this running session's layout and scrollback"), "{err}");
        assert!(err.contains(&format!("kraal -s {} kill", env.session)), "{err}");
        assert!(!String::from_utf8_lossy(&out.stdout).contains("with its layout, scrollback"));
        assert_eq!(env.server_pid(), pid, "the downgraded server was stopped");
        assert_eq!(env.panes(&old)[0]["name"], "old-writer");
        env.wait_for_text(&old, "old-writer", "downgraded-42");
        assert_eq!(std::fs::read(&current).unwrap(), saved);
        assert_eq!(saved_scrollback(&env), scrollback);
    }
    env.run(&old, &["kill"]);
    assert_eq!(std::fs::read(&current).unwrap(), saved);
    assert_eq!(saved_scrollback(&env), scrollback);
    env.run(&old, &["forget"]);
    assert_eq!(std::fs::read(&current).unwrap(), saved);
    assert_eq!(saved_scrollback(&env), scrollback);

    env.run(&this, &["open", "w"]);
    env.wait_for_text(&this, "after-upgrade", "new-42");
    let workspaces: serde_json::Value = serde_json::from_str(&env.run(&this, &["workspaces", "--json"])).unwrap();
    assert_eq!(workspaces[0]["id"], state["workspaces"][0]["id"]);
    assert_eq!([std::fs::read(config).unwrap(), std::fs::read(definition).unwrap()], original_files);
}

#[test]
#[ignore = "needs KRAAL_OLD, another release's kraal: tests/compat.sh runs it"]
fn a_new_empty_session_is_invisible_to_an_older_release() {
    let (old, this) = (Kraal::old(), Kraal::this());
    let env = Env::new("compat-new-state");
    env.run(&this, &["open", "w"]);
    env.run(&this, &["close", "first"]);
    env.run(&this, &["kill"]);
    let current = env.root.join("state/kraal/sessions-v2").join(format!("{}.json", env.session));
    let saved = std::fs::read(&current).unwrap();
    let state: serde_json::Value = serde_json::from_slice(&saved).unwrap();
    assert_eq!(state["workspaces"][0]["empty"], true);
    assert_eq!(state["workspaces"][0]["tabs"], serde_json::json!([]));
    let listed = env.run(&old, &["ls"]);
    assert!(!listed.contains(&env.session), "{listed}");
    env.run(&old, &["forget"]);
    assert_eq!(std::fs::read(&current).unwrap(), saved);
    env.run(&old, &["open", "w"]);
    assert_eq!(env.panes(&old)[0]["name"], "first", "the old release opens its KDL");
    env.run(&old, &["kill"]);
    env.run(&old, &["forget"]);
    assert_eq!(std::fs::read(&current).unwrap(), saved);
    env.run(&this, &["open", "w"]);
    assert!(env.panes(&this).is_empty());
    let workspaces: serde_json::Value = serde_json::from_str(&env.run(&this, &["workspaces", "--json"])).unwrap();
    assert_eq!(workspaces[0]["id"], state["workspaces"][0]["id"]);
}
