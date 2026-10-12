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
        // Under /tmp: macOS's temporary directory is too long for the sockets kraal makes there,
        // and kraal would fall back to a directory every test shares.
        let root = PathBuf::from("/tmp").join(format!("kraal-e2e-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["run", "state", "config/kraal/workspaces"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        // A quiet, predictable shell.
        let sh = root.join("sh");
        write_script(&sh, "#!/bin/sh\nexport PS1='$ '\nexec sh\n");
        std::fs::write(
            root.join("config/kraal/config.toml"),
            format!("shell = {:?}\nssh_config_hosts = false\n", sh.display().to_string()),
        )
        .unwrap();
        Env { root, session: name.to_string() }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        self.cmd_with(Path::new(env!("CARGO_BIN_EXE_kraal")), args)
    }

    /// [`Env::cmd`] run by another kraal binary (a server it starts runs that binary too).
    fn cmd_with(&self, kraal: &Path, args: &[&str]) -> Command {
        let mut c = self.isolated(kraal);
        c.args(["-s", &self.session]).args(args);
        c
    }

    /// `program` in this test's directories, out of any kraal session.
    fn isolated(&self, program: &Path) -> Command {
        let mut c = Command::new(program);
        c.env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env_remove("KRAAL_PANE")
            .env_remove("KRAAL_SOCKET")
            .env_remove("KRAAL_SESSION")
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
        self.wait_for(pane, text, |screen| screen)
    }

    /// Wait for `text` below the line marking what a restart restored, and return that part.
    fn wait_for_text_since_restore(&self, pane: &str, text: &str) -> String {
        self.wait_for(pane, text, since_restore)
    }

    fn wait_for(&self, pane: &str, text: &str, part: impl Fn(&str) -> &str) -> String {
        let start = Instant::now();
        loop {
            let screen = self.run(&["read", "-p", pane]);
            let seen = part(&screen);
            // A narrow pane wraps long lines: the text may run over two rows.
            if seen.contains(text) || seen.lines().collect::<String>().contains(text) {
                return seen.to_string();
            }
            assert!(start.elapsed() < Duration::from_secs(10), "timed out waiting for {text:?} in:\n{screen}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The pid of this test's server: a `server --session <name>` process whose environment has
    /// this test's runtime directory.
    #[cfg(not(target_os = "linux"))]
    fn server_pid(&self) -> Option<i32> {
        let want = format!("XDG_RUNTIME_DIR={}", self.root.join("run").display());
        let found = Command::new("pgrep").args(["-f", &format!("server --session {}$", self.session)]).output().ok()?;
        String::from_utf8_lossy(&found.stdout).lines().filter_map(|l| l.trim().parse::<i32>().ok()).find(|pid| {
            // `ps -E` shows the environment after the command.
            Command::new("ps")
                .args(["-E", "-ww", "-o", "command=", "-p", &pid.to_string()])
                .output()
                .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(&want))
        })
    }

    /// The pid of this test's server, from /proc.
    #[cfg(target_os = "linux")]
    fn server_pid(&self) -> Option<i32> {
        let want = format!("XDG_RUNTIME_DIR={}", self.root.join("run").display());
        std::fs::read_dir("/proc").ok()?.filter_map(|e| e.ok()).find_map(|e| {
            let pid: i32 = e.file_name().to_str()?.parse().ok()?;
            let cmdline = std::fs::read(e.path().join("cmdline")).ok()?;
            let args: Vec<&[u8]> = cmdline.split(|b| *b == 0).filter(|a| !a.is_empty()).collect();
            let tail = [&b"server"[..], b"--session", self.session.as_bytes()];
            if args.len() < 3 || args[args.len() - 3..] != tail {
                return None;
            }
            let environ = std::fs::read(e.path().join("environ")).ok()?;
            environ.split(|b| *b == 0).any(|v| v == want.as_bytes()).then_some(pid)
        })
    }

    /// Start a long-running command; its output lines arrive on the receiver (read with a
    /// deadline, so a stalled stream fails the test instead of hanging it).
    fn follow(&self, args: &[&str]) -> (std::process::Child, std::sync::mpsc::Receiver<String>) {
        use std::io::BufRead;
        let mut child = self.cmd(args).stdout(std::process::Stdio::piped()).spawn().unwrap();
        let out = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        (child, rx)
    }

    /// A client attached in a terminal (`script`, from util-linux, provides one), and what is
    /// typed into it.
    #[cfg(target_os = "linux")]
    fn client(&self) -> std::process::Child {
        self.client_showing(&["attach"], Path::new("/dev/null"))
    }

    /// [`Env::client`] attached with `args`, everything it writes to its terminal kept in
    /// `screen`.
    #[cfg(target_os = "linux")]
    fn client_showing(&self, args: &[&str], screen: &Path) -> std::process::Child {
        let attach = self.cmd(args);
        let line = std::iter::once(attach.get_program())
            .chain(attach.get_args())
            .map(|a| format!("'{}'", a.to_string_lossy()))
            .collect::<Vec<_>>()
            .join(" ");
        let mut c = Command::new("script");
        // The terminal `script` makes has no size until it is given one.
        c.args(["-q", "-f", "-c", &format!("stty cols 100 rows 30; exec {line}")]).arg(screen);
        for (k, v) in attach.get_envs() {
            match v {
                Some(v) => c.env(k, v),
                None => c.env_remove(k),
            };
        }
        c.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        let child = c.spawn().expect("script (util-linux)");
        std::thread::sleep(Duration::from_millis(800));
        child
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

/// Write an executable script. A child process writes it: a file this process held open for
/// writing could be inherited by a program another test starts at that moment, and running the
/// script would then fail with "text file busy".
/// A client attached through the session's socket, as kraal's own client is: it sends what a
/// terminal would, and keeps what the server draws.
struct RawClient {
    stream: std::os::unix::net::UnixStream,
    screen: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    /// The messages the server sent, as JSON.
    messages: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

impl RawClient {
    fn attach(env: &Env, workspace: &str) -> RawClient {
        RawClient::attach_installed(env, workspace, None)
    }

    /// Attach, saying which kraal is installed (a local client says its own).
    fn attach_installed(env: &Env, workspace: &str, installed: Option<&str>) -> RawClient {
        let installed = installed.map(|v| format!(r#","installed":"{v}""#)).unwrap_or_default();
        RawClient::attach_with(env, workspace, &installed)
    }

    /// Attach as a client that follows a live restart, as kraal's own does.
    #[cfg(target_os = "linux")]
    fn attach_following(env: &Env, workspace: &str) -> RawClient {
        RawClient::attach_with(env, workspace, r#","live":true"#)
    }

    /// Attach, with `extra` fields in the hello.
    fn attach_with(env: &Env, workspace: &str, extra: &str) -> RawClient {
        use std::io::Read;
        let socket = env.root.join("run/kraal").join(format!("{}.sock", env.session));
        let mut stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
        let hello = format!(
            r#"{{"type":"attach","version":2,"cols":120,"rows":40,"workspace":"{workspace}","remote":false{extra}}}"#
        );
        raw_frame(&mut stream, 0, hello.as_bytes());
        let screen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (mut reader, kept, said) = (stream.try_clone().unwrap(), screen.clone(), messages.clone());
        std::thread::spawn(move || {
            let mut header = [0u8; 5];
            while reader.read_exact(&mut header).is_ok() {
                let mut payload = vec![0u8; u32::from_le_bytes(header[..4].try_into().unwrap()) as usize];
                if reader.read_exact(&mut payload).is_err() {
                    break;
                }
                match header[4] {
                    1 => kept.lock().unwrap().extend_from_slice(&payload),
                    _ => said.lock().unwrap().push(String::from_utf8_lossy(&payload).into_owned()),
                }
            }
        });
        RawClient { stream, screen, messages }
    }

    /// Wait until the server has sent a message containing `text`.
    fn wait_for_message(&self, text: &str) {
        let start = Instant::now();
        while !self.messages.lock().unwrap().iter().any(|m| m.contains(text)) {
            assert!(
                start.elapsed() < Duration::from_secs(15),
                "{text:?} never sent: {:?}",
                self.messages.lock().unwrap()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Bytes as the terminal sends them, one read each.
    fn send(&mut self, bytes: &[u8]) {
        raw_frame(&mut self.stream, 1, bytes);
    }

    /// A bracketed paste, sent as a terminal does: in reads of 64 KiB.
    fn paste(&mut self, data: &[u8]) {
        let all = [&b"\x1b[200~"[..], data, b"\x1b[201~"].concat();
        for chunk in all.chunks(64 << 10) {
            self.send(chunk);
        }
    }

    /// Wait until the server has drawn `text` on the client's screen.
    fn wait_for_screen(&self, text: &str) {
        let start = Instant::now();
        while !String::from_utf8_lossy(&self.screen.lock().unwrap()).contains(text) {
            assert!(start.elapsed() < Duration::from_secs(10), "{text:?} never shown");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn raw_frame(w: &mut impl std::io::Write, kind: u8, payload: &[u8]) {
    let mut frame = (payload.len() as u32).to_le_bytes().to_vec();
    frame.push(kind);
    frame.extend_from_slice(payload);
    w.write_all(&frame).unwrap();
}

/// Wait until the pane named `name` is seen as production.
fn wait_production(env: &Env, name: &str) {
    let start = Instant::now();
    loop {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let pane = panes.as_array().unwrap().iter().find(|p| p["name"] == name).cloned().unwrap();
        if pane["production"].is_string() {
            return;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "never seen as production: {pane}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn write_script(path: &Path, body: &str) {
    use std::io::Write;
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("cat > \"$1\" && chmod 755 \"$1\"")
        .arg("sh")
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(body.as_bytes()).unwrap();
    assert!(child.wait().unwrap().success(), "writing {}", path.display());
}

/// What a pane shows below the line marking the text a restart restored (all of it without one):
/// what runs in the pane now wrote that.
fn since_restore(screen: &str) -> &str {
    match screen.rfind("kraal: restored from") {
        Some(at) => screen[at..].split_once('\n').map_or("", |(_, rest)| rest),
        None => screen,
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

/// Other agents' hooks, as each one calls `kraal hook`; and a hook for another agent than the
/// one a pane runs (Cursor runs Claude Code's hooks) changes nothing there.
#[test]
fn other_agents_hooks_drive_their_state() {
    let env = Env::new("hooks-other");
    env.workspace("w", "layout {\n    pane name=\"gemini\"\n    pane name=\"copilot\"\n    pane name=\"cursor\"\n}\n");
    env.run(&["open", "w"]);
    let socket = env.root.join("run/kraal/hooks-other.sock");
    let hook = |pane: &str, args: &[&str], payload: &str| {
        let mut c = env.cmd(&[&["hook"], args].concat());
        c.env("KRAAL_PANE", pane).env("KRAAL_SOCKET", &socket).stdin(std::process::Stdio::piped());
        let mut child = c.spawn().unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success() && out.stdout.is_empty() && out.stderr.is_empty(), "hooks must stay silent");
    };
    let wait = |pane: &str, state: &str| {
        assert_eq!(env.run(&["wait", "-p", pane, "--until", state, "--timeout", "3"]).trim(), state, "{pane}");
    };
    // Gemini CLI's hooks leave a declined question to the screen: it runs in its pane, busy.
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let gemini = bin.join("gemini");
    write_script(&gemini, "#!/bin/sh\necho started\nwhile :; do sleep 0.3; printf .; done\n");
    env.run(&["send", "-p", "gemini", "--enter", &gemini.display().to_string()]);
    env.wait_for_text("gemini", "started");
    hook("gemini", &["gemini"], r#"{"hook_event_name":"BeforeAgent","session_id":"g-1","prompt":"fix it"}"#);
    wait("gemini", "working");
    hook(
        "gemini",
        &["gemini"],
        r#"{"hook_event_name":"Notification","session_id":"g-1","notification_type":"ToolPermission","message":"Tool Shell requires execution","details":{"type":"exec"}}"#,
    );
    wait("gemini", "blocked");
    hook("gemini", &["gemini"], r#"{"hook_event_name":"AfterAgent","session_id":"g-1"}"#);
    wait("gemini", "done");
    // Copilot's own format names the event on the command line only.
    hook("copilot", &["copilot", "userPromptSubmitted"], r#"{"sessionId":"c-1","timestamp":1,"cwd":"/","prompt":"x"}"#);
    wait("copilot", "working");
    hook(
        "copilot",
        &["copilot", "notification"],
        r#"{"sessionId":"c-1","hook_event_name":"Notification","notification_type":"permission_prompt","message":"Copilot needs permission"}"#,
    );
    wait("copilot", "blocked");
    hook("copilot", &["copilot", "agentStop"], r#"{"sessionId":"c-1","stopReason":"end_turn"}"#);
    wait("copilot", "done");
    // A pane running Cursor's CLI.
    let cursor = bin.join("cursor-agent");
    write_script(&cursor, "#!/bin/sh\necho started\nsleep 30\n");
    env.run(&["send", "-p", "cursor", "--enter", &cursor.display().to_string()]);
    env.wait_for_text("cursor", "started");
    hook(
        "cursor",
        &["claude"],
        r#"{"hook_event_name":"UserPromptSubmit","session_id":"x","cursor_version":"2026.10.01"}"#,
    );
    hook("cursor", &["claude"], r#"{"hook_event_name":"UserPromptSubmit","session_id":"x"}"#);
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let cursor_pane = panes.as_array().unwrap().iter().find(|p| p["name"] == "cursor").unwrap();
    assert_ne!(cursor_pane["agent"], "claude", "{cursor_pane}");
    hook(
        "cursor",
        &["cursor"],
        r#"{"hook_event_name":"stop","conversation_id":"c","status":"completed","loop_count":0}"#,
    );
    wait("cursor", "done");
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let cursor_pane = panes.as_array().unwrap().iter().find(|p| p["name"] == "cursor").unwrap();
    assert_eq!(cursor_pane["agent"], "cursor", "{cursor_pane}");
    env.run(&["keys", "-p", "cursor", "ctrl+c"]);
    env.run(&["keys", "-p", "gemini", "ctrl+c"]);
}

/// droid tells a cancelled turn by waiting for a prompt, without a stop: the pane is idle again.
/// It says the same after a turn that finished, which stays done until it is seen.
#[test]
fn a_cancelled_turn_ends_idle_and_a_finished_one_stays_done() {
    let env = Env::new("hooks-idle");
    env.workspace("w", "layout {\n    pane name=\"droid\"\n}\n");
    env.run(&["open", "w"]);
    let socket = env.root.join("run/kraal/hooks-idle.sock");
    let hook = |payload: &str| {
        let mut c = env.cmd(&["hook", "droid"]);
        c.env("KRAAL_PANE", "droid").env("KRAAL_SOCKET", &socket).stdin(std::process::Stdio::piped());
        let mut child = c.spawn().unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
        assert!(child.wait_with_output().unwrap().status.success());
    };
    let wait = |state: &str| {
        assert_eq!(env.run(&["wait", "-p", "droid", "--until", state, "--timeout", "3"]).trim(), state);
    };
    let prompt = r#"{"hook_event_name":"UserPromptSubmit","session_id":"d-1","prompt":"fix it"}"#;
    let waiting = r#"{"hook_event_name":"Notification","session_id":"d-1","notification_type":"idle_prompt","message":"Droid is waiting for input"}"#;
    hook(prompt);
    wait("working");
    hook(waiting);
    wait("idle");
    hook(prompt);
    hook(r#"{"hook_event_name":"Stop","session_id":"d-1"}"#);
    wait("done");
    hook(waiting);
    let still = env.try_run(&["wait", "-p", "droid", "--until", "idle", "--timeout", "0.5"]);
    assert!(!still.status.success(), "a finished turn nobody looked at went idle");
    wait("done");
}

/// OpenCode's plugin reports its changes only, never its end: run under a wrapper kraal does not
/// recognise (`sh -c 'opencode; …'`), the agent stays while the process its plugin reported
/// from runs, and goes when that process ends.
#[test]
fn a_hooked_agent_under_a_wrapper_stays_while_its_process_runs() {
    let env = Env::new("hooks-wrapped");
    env.workspace("w", "layout {\n    pane name=\"oc\"\n}\n");
    env.run(&["open", "w"]);
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let opencode = bin.join("opencode");
    let go = env.root.join("go");
    // Stands for OpenCode: its plugin runs `kraal hook opencode <state>` as a child of its own.
    write_script(
        &opencode,
        &format!(
            "#!/bin/sh\n'{}' hook opencode working\necho reported\nwhile [ ! -e '{}' ]; do sleep 0.1; done\n",
            env!("CARGO_BIN_EXE_kraal"),
            go.display()
        ),
    );
    let wrapped = format!("sh -c '{}; echo finished; sleep 30'", opencode.display());
    env.run(&["send", "-p", "oc", "--enter", &wrapped]);
    env.wait_for_text("oc", "reported");
    let agent = || {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let pane = panes.as_array().unwrap().iter().find(|p| p["name"] == "oc").cloned().unwrap();
        (pane["agent"].as_str().map(String::from), pane["state"].as_str().map(String::from))
    };
    // A few scans of the foreground (the wrapper, no agent) later, still there.
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(agent(), (Some("opencode".into()), Some("working".into())));
    std::fs::write(&go, "").unwrap();
    env.wait_for_text("oc", "finished");
    let start = Instant::now();
    while agent().0.is_some() {
        assert!(start.elapsed() < Duration::from_secs(5), "the agent outlived its process");
        std::thread::sleep(Duration::from_millis(100));
    }
    env.run(&["keys", "-p", "oc", "ctrl+c"]);
}

#[test]
fn agents_resume_after_a_restart_but_never_replay_a_one_shot_run() {
    let env = Env::new("resume");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let claude = bin.join("claude");
    write_script(&claude, "#!/bin/sh\necho started\nsleep 0.5\n");
    let claude = claude.display().to_string();
    let socket = env.root.join("run/kraal/resume.sock");
    let hook = |pane: &str, session: &str| {
        let mut c = env.cmd(&["hook", "claude"]);
        c.env("KRAAL_PANE", pane).env("KRAAL_SOCKET", &socket).stdin(std::process::Stdio::piped());
        let mut child = c.spawn().unwrap();
        use std::io::Write;
        let payload = format!(r#"{{"hook_event_name":"SessionStart","session_id":"{session}"}}"#);
        child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
        assert!(child.wait_with_output().unwrap().status.success());
    };
    let codex = bin.join("codex");
    write_script(&codex, "#!/bin/sh\necho started\nsleep 0.5\n");
    let codex = codex.display().to_string();
    // An interactive run and a one-shot one, each with a known conversation, both exited; one
    // whose words kraal cannot tell from a prompt; a one-shot run hidden behind an option kraal
    // cannot read; and a Codex fork, whose conversation is the fork's own.
    env.run(&["run", "-n", "chat", "--", &claude, "--model", "opus"]);
    env.run(&["run", "-n", "oneshot", "--", &claude, "-p", "perform deployment"]);
    env.run(&["run", "-n", "odd", "--", &claude, "--brand-new-flag", "Deploy now"]);
    env.run(&["run", "-n", "printed", "--", &claude, "--debug", "api", "--print", "Deploy release"]);
    env.run(&["run", "-n", "fork", "--", &codex, "fork", "parent-1", "Deploy release"]);
    hook("chat", "s-1");
    hook("oneshot", "s-2");
    hook("odd", "s-3");
    hook("printed", "s-5");
    let mut c = env.cmd(&["hook", "codex", r#"{"type":"agent-turn-complete","thread-id":"child-1"}"#]);
    c.env("KRAAL_PANE", "fork").env("KRAAL_SOCKET", &socket);
    assert!(c.status().unwrap().success());
    for name in ["chat", "oneshot", "odd", "printed", "fork"] {
        env.run(&["wait", "-p", name, "--until", "exit", "--timeout", "10"]);
    }
    env.run(&["kill"]);
    env.run(&["open", "w"]);
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let command = |name: &str| -> Vec<String> {
        let p = panes.as_array().unwrap().iter().find(|p| p["name"] == name).unwrap();
        p["command"].as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_string()).collect()
    };
    assert_eq!(command("chat"), [claude.as_str(), "--resume", "s-1", "--model", "opus"], "it resumes");
    assert_eq!(command("oneshot"), [claude.as_str(), "-p", "perform deployment"], "never run again as a resume");
    // Kept whole, and waiting: Enter runs it as shown.
    assert_eq!(command("odd"), [claude.as_str(), "--resume", "s-3", "--brand-new-flag", "Deploy now"]);
    let odd = panes.as_array().unwrap().iter().find(|p| p["name"] == "odd").unwrap();
    assert_eq!(odd["suspended"], true, "{odd}");
    assert_eq!(command("printed"), [claude.as_str(), "--debug", "api", "--print", "Deploy release"]);
    // The fork's own conversation, never the fork again with its prompt; waiting for Enter.
    assert_eq!(command("fork"), [codex.as_str(), "resume", "child-1"]);
    let fork = panes.as_array().unwrap().iter().find(|p| p["name"] == "fork").unwrap();
    assert_eq!(fork["suspended"], true, "{fork}");
}

#[test]
#[cfg(target_os = "linux")]
fn supervise_ends_what_its_program_left_however_it_left() {
    let env = Env::new("supervise");
    let gone = |pidfile: &Path| {
        let pid = std::fs::read_to_string(pidfile).unwrap().trim().to_string();
        let start = Instant::now();
        while Path::new(&format!("/proc/{pid}")).exists() {
            assert!(start.elapsed() < Duration::from_secs(5), "{pid} still runs");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    // Nothing holds the test's own output if a helper survives (the test then fails, not hangs).
    let quiet = |mut c: std::process::Command| {
        c.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        c
    };
    // A helper started with a cleared environment, in a session of its own, by a program that
    // then ends: an orphan nothing tags, and outside the program's group.
    let pidfile = env.root.join("helper.pid");
    let helper = format!("env -i setsid sh -c 'sleep 600 & echo $! > {}'", pidfile.display());
    let status = quiet(env.cmd(&["supervise", "sh", "-c", &format!("{helper}; sleep 0.3; exit 3")])).status().unwrap();
    assert_eq!(status.code(), Some(3), "the program's own status");
    gone(&pidfile);
    // Stopped while the program runs: the program and the helper both go.
    let pidfile = env.root.join("helper2.pid");
    let helper = format!("env -i setsid sh -c 'sleep 600 & echo $! > {}'", pidfile.display());
    let mut supervisor =
        quiet(env.cmd(&["supervise", "sh", "-c", &format!("{helper}; exec sleep 600")])).spawn().unwrap();
    let start = Instant::now();
    while std::fs::read_to_string(&pidfile).map_or(true, |p| p.trim().is_empty()) {
        assert!(start.elapsed() < Duration::from_secs(5), "the helper never started");
        std::thread::sleep(Duration::from_millis(20));
    }
    // SAFETY: plain syscall on our own child.
    unsafe {
        libc::kill(supervisor.id() as libc::pid_t, libc::SIGTERM);
    }
    let start = Instant::now();
    while supervisor.try_wait().unwrap().is_none() {
        assert!(start.elapsed() < Duration::from_secs(5), "the supervisor did not end");
        std::thread::sleep(Duration::from_millis(20));
    }
    gone(&pidfile);
}

#[test]
#[cfg(target_os = "linux")]
fn codex_usage_leaves_nothing_behind() {
    let env = Env::new("usage");
    let home = env.root.join("home");
    std::fs::create_dir_all(home.join(".codex")).unwrap();
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let pidfile = env.root.join("helper.pid");
    // A `codex` that leaves a helper behind (no environment, a session of its own), then answers.
    write_script(
        &bin.join("codex"),
        &format!(
            r#"#!/bin/sh
env -i setsid sh -c 'sleep 600 >/dev/null 2>&1 & echo $! > {}'
IFS= read -r line
printf '%s\n' '{{"id":1,"result":{{}}}}'
IFS= read -r line
IFS= read -r line
printf '%s\n' '{{"id":2,"result":{{"rateLimits":{{"primary":{{"usedPercent":42,"windowDurationMins":300}}}}}}}}'
sleep 600
"#,
            pidfile.display()
        ),
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let mut c = env.cmd(&["accounts", "refresh"]);
    c.env("HOME", &home).env("PATH", path).stdin(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    let out = c.output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("42%"), "{text}");
    let pid = std::fs::read_to_string(&pidfile).unwrap().trim().to_string();
    let start = Instant::now();
    while Path::new(&format!("/proc/{pid}")).exists() {
        assert!(start.elapsed() < Duration::from_secs(5), "the helper Codex left still runs");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn an_agent_that_may_send_its_prompt_again_is_shown_before_it_restarts() {
    let env = Env::new("showfirst");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let claude = bin.join("claude");
    write_script(&claude, "#!/bin/sh\necho \"ran: $*\"\nsleep 0.3\n");
    let claude = claude.display().to_string();
    let socket = env.root.join("run/kraal/showfirst.sock");
    let mut client: Option<RawClient> = None;
    // An option kraal does not know; and `--worktree update`, where `update` names a worktree
    // (it is not Claude's `update` subcommand, and the conversation is resumed).
    for (pane, args, session) in [
        ("odd", ["--brand-new-flag", "Deploy now"].as_slice(), "s-9"),
        ("wt", ["--worktree", "update", "Deploy release"].as_slice(), "s-10"),
    ] {
        let mut run = vec!["run", "-n", pane, "--", claude.as_str()];
        run.extend_from_slice(args);
        env.run(&run);
        let mut c = env.cmd(&["hook", "claude"]);
        c.env("KRAAL_PANE", pane).env("KRAAL_SOCKET", &socket).stdin(std::process::Stdio::piped());
        let mut child = c.spawn().unwrap();
        use std::io::Write;
        let start = format!(r#"{{"hook_event_name":"SessionStart","session_id":"{session}"}}"#);
        child.stdin.take().unwrap().write_all(start.as_bytes()).unwrap();
        assert!(child.wait_with_output().unwrap().status.success());
        env.run(&["wait", "-p", pane, "--until", "exit", "--timeout", "10"]);
        env.run(&["focus", pane]);
        let client = client.get_or_insert_with(|| RawClient::attach(&env, "w"));
        env.wait_for_text(pane, "ran:");
        // The first Enter shows what would run; nothing runs.
        client.send(b"\r");
        let screen = env.wait_for_text(pane, "cannot tell");
        assert_eq!(screen.matches("ran:").count(), 1, "{pane}: {screen}");
        // The second runs it, exactly as shown (once the note is up: Enter before that goes to it).
        std::thread::sleep(Duration::from_millis(500));
        client.send(b"\r");
        // Lines may wrap in the pane (a space at a line end is lost): compare without spaces.
        let flat = |s: &str| s.chars().filter(|c| !c.is_whitespace()).collect::<String>();
        let expected = flat(&format!("ran: --resume {session} {}", args.join(" ")));
        let start = Instant::now();
        let screen = loop {
            let screen = flat(&env.run(&["read", "-p", pane]));
            if screen.contains(&expected) {
                break screen;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "{pane} never ran: {screen}");
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_eq!(screen.matches("ran:").count(), 2, "{pane}: {screen}");
    }
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
    tab name="tab 2" {
        pane name="shell"
    }
}
"#,
    );
    env.run(&["open", "fleet"]);
    env.wait_for_text("b", "node-b");
    env.run(&["rename", "-p", "shell", "renamed"]);
    let pid = env.server_pid().expect("the server runs");
    env.run(&["kill"]);
    // `kill` returns once the server has saved the layout and exited.
    assert!(env.server_pid().is_none(), "server {pid} still runs");
    assert!(env.state_file().exists());
    // Any command starting the server restores the saved layout.
    env.run(&["open", "fleet"]);
    env.wait_for_text("a", "node-a");
    let panes = env.run(&["panes"]);
    assert!(panes.contains("renamed"), "{panes}");
    assert_eq!(panes.lines().count(), 3, "{panes}");
    // A tab named "tab 2" keeps its name; older versions saved a tab without a name that way,
    // and in their states it shows what its pane runs.
    assert!(panes.contains("tab 2"), "{panes}");
    env.run(&["kill"]);
    let state = std::fs::read_to_string(env.state_file()).unwrap();
    assert!(state.contains("\"revision\": 1,"), "{state}");
    std::fs::write(env.state_file(), state.replace("\"revision\": 1,", "")).unwrap();
    env.run(&["open", "fleet"]);
    env.wait_for_text("a", "node-a");
    let panes = env.run(&["panes"]);
    assert!(!panes.contains("tab 2") && panes.contains("renamed"), "{panes}");
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
    let json = env.run(&["panes", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let lefts = v.as_array().unwrap().iter().filter(|p| p["name"] == "left").count();
    assert_eq!(lefts, 2, "one in each workspace: {json}");
    // What has not run yet still waits, and a tab of floating panes alone is kept.
    env.workspace(
        "w2",
        "layout {\n    tab {\n        pane name=\"tiled\"\n        floating_panes {\n            pane name=\"float\" command=\"sleep\" start_suspended=true { args \"100\"; }\n        }\n    }\n}\n",
    );
    env.run(&["open", "w2"]);
    env.run(&["close", "w2:tiled"]);
    env.run(&["save", "copy2", "--workspace", "w2"]);
    let saved = std::fs::read_to_string(env.root.join("config/kraal/workspaces/copy2.kdl")).unwrap();
    assert!(saved.contains("name=\"float\"") && saved.contains("start_suspended=true"), "{saved}");
    // A name is a file name in the definitions directory, nothing else.
    for name in ["../escape", "/tmp/escape", ".hidden"] {
        let out = env.try_run(&["save", name, "--workspace", "w"]);
        assert!(!out.status.success(), "{name} was accepted");
    }
    assert!(!env.root.join("config/kraal/escape.kdl").exists());
}

#[test]
fn commands_run_in_a_pane_stay_in_its_session() {
    let a = Env::new("sessa");
    a.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    a.run(&["open", "w"]);
    let base = || {
        let mut c = Command::new(env!("CARGO_BIN_EXE_kraal"));
        c.env("XDG_RUNTIME_DIR", a.root.join("run"))
            .env("XDG_STATE_HOME", a.root.join("state"))
            .env("XDG_CONFIG_HOME", a.root.join("config"))
            .env_remove("KRAAL_PANE")
            .env_remove("KRAAL_SOCKET")
            .env_remove("KRAAL_SESSION")
            .env_remove("KRAAL_SESSION_NAME");
        c
    };
    // A second session in the same directories, whose pane has the same id.
    let b = |args: &[&str]| base().args(["-s", "sessb"]).args(args).output().unwrap();
    assert!(b(&["open", "w"]).status.success());
    let id = |out: &str| -> String {
        let v: serde_json::Value = serde_json::from_str(out).unwrap();
        v[0]["id"].to_string()
    };
    let pane_a = id(&a.run(&["panes", "--json"]));
    let pane_b = id(&String::from_utf8(b(&["panes", "--json"]).stdout).unwrap());
    assert_eq!(pane_a, pane_b);
    // As if typed in session a's pane: no `-s`, the pane's variables.
    let in_pane = |args: &[&str]| {
        base()
            .args(args)
            .env("KRAAL_SESSION", "sessa")
            .env("KRAAL_SOCKET", a.root.join("run/kraal/sessa.sock"))
            .env("KRAAL_PANE", &pane_a)
            .output()
            .unwrap()
    };
    // Another session's pane is never the one the command runs in.
    let out = in_pane(&["-s", "sessb", "close"]);
    assert!(!out.status.success(), "closed a pane of sessb");
    assert!(String::from_utf8_lossy(&b(&["panes"]).stdout).contains("one"));
    // `kill` without `-s` stops the session it runs in, even with another session named in
    // `$KRAAL_SESSION_NAME` (exported by a shell's startup files, inherited by every pane).
    let out = base()
        .arg("kill")
        .env("KRAAL_SESSION", "sessa")
        .env("KRAAL_SOCKET", a.root.join("run/kraal/sessa.sock"))
        .env("KRAAL_PANE", &pane_a)
        .env("KRAAL_SESSION_NAME", "sessb")
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!a.try_run(&["panes"]).status.success(), "sessa still runs");
    assert!(b(&["panes"]).status.success(), "sessb was stopped");
    // Outside kraal, `$KRAAL_SESSION_NAME` still names the session.
    let named = base().arg("panes").env("KRAAL_SESSION_NAME", "sessb").output().unwrap();
    assert!(named.status.success() && String::from_utf8_lossy(&named.stdout).contains("one"));
    let _ = b(&["kill"]);
}

#[test]
fn kill_says_when_the_layout_was_not_saved() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new("unsaved");
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    env.run(&["open", "w"]);
    let sessions = env.root.join("state/kraal/sessions");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o500)).unwrap();
    let out = env.try_run(&["kill"]);
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o700)).unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("could not be saved"), "{err}");
    assert!(env.server_pid().is_none(), "the server stopped anyway");
}

#[test]
fn a_failed_start_leaves_no_empty_workspace() {
    let env = Env::new("emptyws");
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    // A server with no workspace yet, asked for an agent on an account that does not exist.
    let mut server = env.cmd(&["server"]).spawn().unwrap();
    let socket = env.root.join("run/kraal/emptyws.sock");
    let start = Instant::now();
    while std::os::unix::net::UnixStream::connect(&socket).is_err() {
        assert!(start.elapsed() < Duration::from_secs(5), "the server did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = env.cmd(&["agent", "claude", "--account", "nope"]).env("KRAAL_SOCKET", &socket).output().unwrap();
    assert!(!out.status.success());
    let list = env.run(&["workspaces", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&list).unwrap();
    assert!(v.as_array().unwrap().iter().all(|w| w["name"] != "main" || w["open"] == false), "{list}");
    env.run(&["open", "w"]);
    env.run(&["kill"]);
    let _ = server.wait();
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
    // Which kraal session's agents to list: given to a server saved already, never a bad name.
    env.run(&["servers", "add", "ops@bastion.invalid", "--agents", "main"]);
    let json = env.run(&["servers", "list", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let bastion = v.as_array().unwrap().iter().find(|s| s["name"] == "bastion").cloned().unwrap();
    assert_eq!(bastion["agents"], "main", "{bastion}");
    assert!(!env.try_run(&["servers", "add", "ops@other.invalid", "--agents", "../x"]).status.success());

    // Copy plans, without touching the network.
    let file = env.root.join("report.txt");
    std::fs::write(&file, "x").unwrap();
    let up = env.run(&["cp", "--dry-run", file.to_str().unwrap(), "web:/tmp/"]);
    assert!(up.contains("-p 2222") && up.contains("admin@web.lab.invalid"), "{up}");
    let s2s = env.run(&["cp", "--dry-run", "web:/etc/hosts", "db:~/in/"]);
    assert!(s2s.contains("tar cf -") && s2s.contains("admin@db.lab.invalid"), "{s2s}");
    let bad = env.try_run(&["cp", "nosuch:/x", "."]);
    assert!(String::from_utf8_lossy(&bad.stderr).contains("unknown server"));

    env.run(&["servers", "rm", "bastion"]);
    assert!(!env.run(&["servers"]).contains("bastion"));
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

#[test]
fn scrollback_survives_a_server_restart() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new("scrollback");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    // More lines than the screen holds, then a coloured line with wide characters.
    env.run(&["send", "-p", "shell", "--enter", "seq 1 300; printf '\\033[31m%s\\033[0m 日本\\n' red-$((6*7))"]);
    env.wait_for_text("shell", "red-42 日本");
    env.run(&["kill"]);
    // Saved with its colours, for the user's eyes only.
    let dir = env.root.join("state/kraal/scrollback/scrollback");
    let files: Vec<PathBuf> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().path()).collect();
    assert_eq!(files.len(), 1, "{files:?}");
    let saved = std::fs::read_to_string(&files[0]).unwrap();
    assert!(saved.contains("\x1b[0;31mred-42\x1b[0m 日本"), "{saved:?}");
    assert_eq!(std::fs::metadata(&files[0]).unwrap().permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
    // Back after the restart, history included, with what the new shell writes below a marker.
    env.run(&["open", "w"]);
    let screen = env.run(&["read", "-p", "shell"]);
    assert!(screen.contains("red-42 日本") && screen.contains("kraal: restored from"), "{screen}");
    let all = env.run(&["read", "-p", "shell", "--lines", "1000"]);
    assert!(all.lines().any(|l| l == "1"), "{all}");
    env.run(&["send", "-p", "shell", "--enter", "echo after-$((1+1))"]);
    env.wait_for_text_since_restore("shell", "after-2");
    // Restarted again: both restores show, in order.
    env.run(&["kill"]);
    env.run(&["open", "w"]);
    let all = env.run(&["read", "-p", "shell", "--lines", "1000"]);
    let at = |text: &str, from: usize| match all[from..].find(text) {
        Some(i) => from + i,
        None => panic!("no {text:?} after byte {from} of:\n{all}"),
    };
    let first = at("kraal: restored from", at("red-42 日本", at("300", 0)));
    at("kraal: restored from", at("after-2", first));
    // `kraal forget` deletes it with the layout.
    env.run(&["kill"]);
    env.run(&["forget"]);
    assert!(!dir.exists() && !env.state_file().exists());
    // Turned off, what was saved goes, and nothing is saved any more.
    env.run(&["open", "w"]);
    env.run(&["send", "-p", "shell", "--enter", "echo secret-$((2+2))"]);
    env.wait_for_text("shell", "secret-4");
    env.run(&["kill"]);
    assert!(dir.exists());
    let config = env.root.join("config/kraal/config.toml");
    let base = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{base}save_scrollback = false\n")).unwrap();
    env.run(&["open", "w"]);
    let screen = env.run(&["read", "-p", "shell"]);
    assert!(!screen.contains("secret-4") && !screen.contains("restored from"), "{screen}");
    env.run(&["kill"]);
    assert!(!dir.exists());
}

/// A server that dies without stopping (a crash) saved what its panes showed shortly before.
#[cfg(target_os = "linux")]
#[test]
fn scrollback_survives_a_crash() {
    let env = Env::new("crash");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    env.run(&["send", "-p", "shell", "--enter", "echo before-$((3*3))"]);
    env.wait_for_text("shell", "before-9");
    // Written while the pane prints, then named by the state.
    let dir = env.root.join("state/kraal/scrollback/crash");
    let saved = || {
        let files = std::fs::read_dir(&dir).into_iter().flatten().flatten();
        files.filter(|e| std::fs::read_to_string(e.path()).unwrap_or_default().contains("before-9")).count() == 1
    };
    let named = || std::fs::read_to_string(env.state_file()).unwrap_or_default().contains(".ans\"");
    let start = Instant::now();
    while !(saved() && named()) {
        assert!(start.elapsed() < Duration::from_secs(10), "the scrollback was not saved while the server ran");
        std::thread::sleep(Duration::from_millis(50));
    }
    let pid = env.server_pid().expect("the server's pid");
    // SAFETY: plain syscall.
    unsafe { libc::kill(pid, libc::SIGKILL) };
    wait_gone(&PathBuf::from(format!("/proc/{pid}")));
    env.run(&["open", "w"]);
    let screen = env.run(&["read", "-p", "shell"]);
    assert!(screen.contains("before-9") && screen.contains("kraal: restored from"), "{screen}");
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

#[test]
fn events_stream_and_hooks() {
    let env = Env::new("events");
    let cfg = env.root.join("config/kraal/config.toml");
    let marker = env.root.join("hook.txt");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str(&format!(
        "\n[hooks]\n\"pane.failed\" = 'echo \"$KRAAL_EVENT $KRAAL_CODE $KRAAL_NAME\" > {}'\n",
        marker.display()
    ));
    std::fs::write(&cfg, text).unwrap();
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    // From the start of the session: nothing is missed, however late the subscription is live.
    env.run(&["run", "--name", "job", "--", "sh", "-c", "exit 3"]);
    let (mut events, lines) = env.follow(&["events", "--since", "0", "pane.*"]);
    let mut seen = Vec::new();
    while seen.len() < 3 {
        let line = lines.recv_timeout(Duration::from_secs(10)).expect("an event in time");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        if v["name"] != "job" {
            continue;
        }
        seen.push(v["type"].as_str().unwrap().to_string());
        if v["type"] == "pane.exited" {
            assert_eq!(v["code"], 3, "{line}");
            assert_eq!(v["command"][0], "sh", "{line}");
        }
    }
    assert_eq!(seen, vec!["pane.opened", "pane.exited", "pane.failed"]);
    let _ = events.kill();
    let _ = events.wait();
    let start = Instant::now();
    while !marker.exists() && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(std::fs::read_to_string(&marker).unwrap().trim(), "pane.failed 3 job");
    // Live events come too, after the replay.
    let (mut live, lines) = env.follow(&["events", "--since", "0", "pane.failed"]);
    let line = lines.recv_timeout(Duration::from_secs(10)).expect("replayed event");
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!((v["type"].as_str(), v["name"].as_str()), (Some("pane.failed"), Some("job")), "{line}");
    assert!(v["seq"].as_u64().is_some() && v["boot"].as_u64().is_some(), "{line}");
    env.run(&["run", "--name", "again", "--", "sh", "-c", "exit 4"]);
    let line = lines.recv_timeout(Duration::from_secs(10)).expect("live event");
    assert!(line.contains("\"again\""), "{line}");
    let _ = live.kill();
    let _ = live.wait();
    // A replay longer than a client's queue still arrives whole.
    for i in 0..40 {
        env.run(&["run", "--name", &format!("many{i}"), "-x", "--", "true"]);
    }
    let (mut all, lines) = env.follow(&["events", "--since", "0", "pane.opened"]);
    let mut opened = 0;
    while opened < 40 {
        let line = lines.recv_timeout(Duration::from_secs(10)).expect("the whole replay");
        if line.contains("\"many") {
            opened += 1;
        }
    }
    let _ = all.kill();
    let _ = all.wait();
    let out = env.try_run(&["events", "pane.exitd"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no such event"));
}

#[test]
fn production_panes_refuse_unasked_cli_input() {
    let env = Env::new("prodguard");
    let tf = env.root.join("infra");
    std::fs::create_dir_all(tf.join(".terraform")).unwrap();
    std::fs::write(tf.join(".terraform/environment"), "prod\n").unwrap();
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    env.run(&["run", "--cwd", &tf.display().to_string(), "--name", "ops", "--", "sh"]);
    // The Terraform workspace is seen by the directory scan, every few seconds.
    let start = Instant::now();
    loop {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let ops = panes.as_array().unwrap().iter().find(|p| p["name"] == "ops").cloned().unwrap();
        if ops["production"].is_string() {
            assert_eq!(ops["production"], "Terraform workspace prod");
            assert_eq!(ops["contexts"][0], "tf:prod");
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "never seen as production: {ops}");
        std::thread::sleep(Duration::from_millis(100));
    }
    let out = env.try_run(&["send", "-p", "ops", "echo", "hi"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("is production"), "{}", String::from_utf8_lossy(&out.stderr));
    env.run(&["send", "-p", "ops", "--production", "--enter", "echo", "SENT-ON-PURPOSE"]);
    env.wait_for_text("ops", "SENT-ON-PURPOSE");
    env.run(&["send", "-p", "main", "--enter", "echo", "not-production"]);
}

#[cfg(unix)]
#[test]
fn runbooks_run_locally_and_on_servers() {
    let env = Env::new("runbooks");
    // A fake ssh: runs the remote script here, and fails for the server "bad".
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(
        &bin.join("ssh"),
        "#!/bin/sh\nfor a; do dest=$cmd; cmd=$a; done\ncase $dest in *bad*) echo unreachable >&2; exit 255;; esac\nexec sh -c \"$cmd\"\n",
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let run = |args: &[&str]| env.cmd(args).env("PATH", &path).output().unwrap();
    std::fs::write(
        env.root.join("config/kraal/servers.toml"),
        "[[server]]\nname = \"a\"\nhost = \"a.lan\"\ngroups = [\"web\"]\n\n[[server]]\nname = \"bad\"\nhost = \"bad.lan\"\ngroups = [\"web\"]\n\n[[server]]\nname = \"c\"\nhost = \"c.lan\"\ngroups = [\"web\"]\n",
    )
    .unwrap();
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    assert!(run(&["open", "w"]).status.success());

    let out = run(&["runbook", "new", "greet"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let file = env.root.join("config/kraal/runbooks/greet.toml");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), file.display().to_string());
    std::fs::write(
        &file,
        "description = \"say hello\"\nsteps = [\"echo hello $WHO\", \"echo bye\"]\n[[param]]\nname = \"WHO\"\ndefault = \"world\"\n",
    )
    .unwrap();
    let list = String::from_utf8(run(&["runbook", "list"]).stdout).unwrap();
    assert!(list.contains("greet") && list.contains("say hello"), "{list}");

    let out = run(&["runbook", "run", "greet", "--dry-run", "-p", "WHO=kraal"]);
    let plan = String::from_utf8_lossy(&out.stdout);
    assert!(
        plan.contains("Run greet on this machine?")
            && plan.contains("WHO=kraal")
            && plan.contains("1. echo hello $WHO"),
        "{plan}"
    );
    let out = run(&["runbook", "run", "greet", "--yes", "--wait", "-p", "WHO=kraal"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}{}", String::from_utf8_lossy(&out.stderr));
    assert!(stdout.contains("finished: 1 ok, 0 failed, 0 skipped"), "{stdout}");
    let log = stdout.rsplit("log ").next().unwrap().trim().to_string();
    let log_text = std::fs::read_to_string(&log).unwrap();
    assert!(
        log_text.contains("param     WHO=kraal") && log_text.contains("hello kraal") && log_text.contains("1 ok"),
        "{log_text}"
    );

    // On a group, one server at a time: the failing one stops the run.
    std::fs::write(
        env.root.join("config/kraal/runbooks/check.toml"),
        "on = \"server\"\nsteps = [\"echo checked $KRAAL_SERVER\"]\n",
    )
    .unwrap();
    // A failing line inside a step fails it, even when later lines succeed.
    std::fs::write(
        env.root.join("config/kraal/runbooks/multi.toml"),
        "steps = [\"false\\necho after-false\", \"echo second-step\"]\n",
    )
    .unwrap();
    let out = run(&["runbook", "run", "multi", "--yes", "--wait"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success() && stdout.contains("finished: 0 ok, 1 failed"), "{stdout}");
    let log = std::fs::read_to_string(stdout.rsplit("log ").next().unwrap().trim()).unwrap();
    let output = log.split("\n--- ").nth(1).unwrap_or_default();
    assert!(output.contains("step 1 failed"), "{log}");
    assert!(!output.contains("after-false") && !output.contains("second-step"), "{log}");

    let out = run(&["runbook", "run", "check", "--yes"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("--on"), "servers must be named");
    let out = run(&["runbook", "run", "check", "--on", "@web", "--yes", "--wait"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "{stdout}");
    assert!(stdout.contains("3 servers (a, bad, c), one at a time, stopping at the first failure"), "{stdout}");
    assert!(stdout.contains("finished: 1 ok, 1 failed, 1 skipped"), "{stdout}");
    let screen = String::from_utf8(run(&["read", "-p", "a"]).stdout).unwrap();
    assert!(screen.contains("checked a"), "{screen}");
    let screen = String::from_utf8(run(&["read", "-p", "c (skipped)"]).stdout).unwrap();
    assert!(screen.contains("skipped: an earlier target failed") && !screen.contains("checked c"), "{screen}");

    // All at once: one fails at once while the others still run. The run must wait for them,
    // not spin (this once overflowed the server's stack).
    std::fs::write(
        env.root.join("config/kraal/runbooks/wide.toml"),
        "on = \"server\"\nrolling = 0\nsteps = [\"sleep 0.5\", \"echo wide $KRAAL_SERVER\"]\n",
    )
    .unwrap();
    let out = run(&["runbook", "run", "wide", "--on", "@web", "--yes", "--wait"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("all at once"), "{stdout}");
    assert!(stdout.contains("finished: 2 ok, 1 failed, 0 skipped"), "{stdout}");
    assert!(run(&["ls"]).status.success(), "the server is still alive");

    // A target over its timeout is stopped, even when it ignores SIGTERM.
    std::fs::write(
        env.root.join("config/kraal/runbooks/slow.toml"),
        "timeout = \"1\"\nsteps = [\"trap '' TERM; sleep 60\"]\n",
    )
    .unwrap();
    let start = Instant::now();
    let out = run(&["runbook", "run", "slow", "--yes", "--wait"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("finished: 0 ok, 1 failed"), "{stdout}");
    assert!(start.elapsed() < Duration::from_secs(20), "stopped by SIGKILL after the grace period");
    let log = std::fs::read_to_string(stdout.rsplit("log ").next().unwrap().trim()).unwrap();
    assert!(log.contains("over its timeout") && log.contains("timed out, stopped"), "{log}");

    // What a timed-out target left running in its process group is stopped too, before the
    // target counts as timed out.
    let pidfile = env.root.join("left.pid");
    std::fs::write(
        env.root.join("config/kraal/runbooks/leaves.toml"),
        format!(
            "timeout = \"1\"\nsteps = [\"(trap '' TERM HUP; exec sleep 60) & echo $! > {}; wait\"]\n",
            pidfile.display()
        ),
    )
    .unwrap();
    let start = Instant::now();
    let out = run(&["runbook", "run", "leaves", "--yes", "--wait"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("finished: 0 ok, 1 failed"), "{stdout}");
    assert!(start.elapsed() < Duration::from_secs(20));
    let left: libc::pid_t = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
    // SAFETY: signal 0 only checks that the process exists.
    assert_ne!(unsafe { libc::kill(left, 0) }, 0, "the background sleep {left} still runs");

    // The log has all of a target's output, not only what its pane still shows.
    std::fs::write(env.root.join("config/kraal/runbooks/long.toml"), "steps = [\"seq 1 3000\"]\n").unwrap();
    let out = run(&["runbook", "run", "long", "--yes", "--wait"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let log = std::fs::read_to_string(stdout.rsplit("log ").next().unwrap().trim()).unwrap();
    assert!(log.contains("\n1\n2\n3\n") && log.contains("\n3000\n"), "{log}");

    // A server that dies during a run: the next one says so in the run's log.
    std::fs::write(env.root.join("config/kraal/runbooks/cut.toml"), "steps = [\"echo cut-started\", \"sleep 60\"]\n")
        .unwrap();
    assert!(run(&["runbook", "run", "cut", "--yes"]).status.success());
    let runs = env.root.join("state/kraal/runs");
    let log = std::fs::read_dir(&runs)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .find(|p| p.to_string_lossy().contains("-cut-") && p.extension().is_some_and(|x| x == "log"))
        .expect("the run's log");
    // Its output reaches the disk while it runs.
    let start = Instant::now();
    while !std::fs::read_to_string(log.with_extension("0.out")).unwrap_or_default().contains("cut-started") {
        assert!(start.elapsed() < Duration::from_secs(10), "no output streamed");
        std::thread::sleep(Duration::from_millis(50));
    }
    let pid = env.server_pid().expect("the server's pid");
    // SAFETY: plain syscall.
    unsafe { libc::kill(pid, libc::SIGKILL) };
    wait_gone(&PathBuf::from(format!("/proc/{pid}")));
    let _ = std::fs::remove_file(env.root.join("run/kraal/runbooks.sock"));
    env.run(&["open", "w"]);
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(
        text.contains("cut-started") && text.contains("interrupted") && text.contains("unfinished: this machine"),
        "{text}"
    );
    // The restarted server may be writing its run ledger (a temporary file, then a rename).
    let start = Instant::now();
    loop {
        let left: Vec<_> = std::fs::read_dir(&runs).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name()).collect();
        if left.iter().all(|n| n.to_string_lossy().ends_with(".log")) {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(5), "{left:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Keys typed while the runbooks are being listed go to the list, not to the shell under it.
#[cfg(target_os = "linux")]
#[test]
fn keys_typed_while_runbooks_load_stay_in_the_list() {
    let env = Env::new("rbkeys");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    std::fs::create_dir_all(env.root.join("config/kraal/runbooks")).unwrap();
    std::fs::write(env.root.join("config/kraal/runbooks/hello.toml"), "steps = [\"echo hello\"]\n").unwrap();
    env.run(&["open", "w"]);
    let mut client = env.client();
    // ctrl+b R b opens the runbooks, and the rest comes in the same breath.
    std::io::Write::write_all(client.stdin.as_mut().unwrap(), b"\x02Rbecho leaked\r").unwrap();
    std::thread::sleep(Duration::from_millis(800));
    std::io::Write::write_all(client.stdin.as_mut().unwrap(), b"\x1b").unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let screen = env.run(&["read", "-p", "shell"]);
    assert!(!screen.contains("leaked"), "typed into the shell: {screen}");
    let _ = client.kill();
    let _ = client.wait();
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
    let read_pid = |path: &Path| std::fs::read_to_string(path).ok().and_then(|t| t.trim().parse::<libc::pid_t>().ok());
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
    let read_pid = |path: &Path| std::fs::read_to_string(path).ok().and_then(|t| t.trim().parse::<libc::pid_t>().ok());
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

    // Its pane, the last one, closed during the grace period: kraal stops, and stops it first.
    let env = Env::new("tolast");
    let left = start_run(&env, "last");
    env.run(&["close", "shell"]);
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let id = panes.as_array().unwrap()[0]["id"].to_string();
    let _ = env.try_run(&["close", &id]);
    wait_until("kraal stops", 20, Box::new(|| env.server_pid().is_none()));
    wait_until("what the target left is stopped", 5, Box::new(|| !alive(left)));

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

/// A tool that acts goes by what the pane's kubeconfig says when it starts: switched to
/// production a moment before (`kubectx`), it asks first.
#[cfg(target_os = "linux")]
#[test]
fn a_tool_asks_when_the_kubeconfig_just_switched_to_production() {
    let env = Env::new("kubenow");
    let kube = env.root.join("kube.yaml");
    let config = |current: &str| {
        format!(
            "apiVersion: v1\nkind: Config\ncurrent-context: {current}\ncontexts:\n\
             - name: dev\n  context: {{cluster: c}}\n- name: prod-eks\n  context: {{cluster: c}}\n"
        )
    };
    std::fs::write(&kube, config("dev")).unwrap();
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(&bin.join("kubectl"), "#!/bin/sh\nsleep 1000\n");
    let marked = env.root.join("marked");
    let cfg = env.root.join("config/kraal/config.toml");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str(&format!("\n[tools.mark]\ncommand = \"touch {}\"\nkey = \"m\"\n", marked.display()));
    std::fs::write(&cfg, text).unwrap();
    // A pane where kubectl runs on that kubeconfig.
    env.workspace(
        "w",
        &format!(
            "layout {{\n    pane name=\"k\" command=\"env\" {{ args \"KUBECONFIG={}\" \"{}\" \"get\" \"pods\"; }}\n}}\n",
            kube.display(),
            bin.join("kubectl").display()
        ),
    );
    env.run(&["open", "w"]);
    let start = Instant::now();
    loop {
        let panes = env.run(&["panes", "--json"]);
        if panes.contains("dev") {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(20), "the pane's context is read: {panes}");
        std::thread::sleep(Duration::from_millis(100));
    }
    let mut client = env.client();
    // Switched to production, then the tool at once: before the next look at the file.
    std::fs::write(&kube, config("prod-eks")).unwrap();
    std::io::Write::write_all(client.stdin.as_mut().unwrap(), b"\x02Rm").unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    assert!(!marked.exists(), "the tool ran on production without asking");
    // It asked: yes runs it.
    std::io::Write::write_all(client.stdin.as_mut().unwrap(), b"y").unwrap();
    let start = Instant::now();
    while !marked.exists() {
        assert!(start.elapsed() < Duration::from_secs(10), "the tool runs once allowed");
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = client.kill();
    let _ = client.wait();
}

/// A pane that runs kubectl on `kube` for 2 s, then is a shell: its kubeconfig is known, and
/// what is typed there shows.
#[cfg(target_os = "linux")]
fn kube_shell_pane(env: &Env, name: &str, kube: &Path) -> String {
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(&bin.join("kubectl"), "#!/bin/sh\nsleep 2\nexport PS1='$ '\nexec sh\n");
    format!(
        "pane name=\"{name}\" command=\"env\" {{ args \"KUBECONFIG={}\" \"{}\" \"get\" \"pods\"; }}",
        kube.display(),
        bin.join("kubectl").display()
    )
}

#[cfg(target_os = "linux")]
fn kubeconfig_text(current: &str) -> String {
    format!(
        "apiVersion: v1\nkind: Config\ncurrent-context: {current}\ncontexts:\n\
         - name: dev\n  context: {{cluster: c}}\n- name: prod-x\n  context: {{cluster: c}}\n"
    )
}

/// Wait until the panes say a context: the pane's kubeconfig was read.
#[cfg(target_os = "linux")]
fn wait_for_context(env: &Env, context: &str) {
    let start = Instant::now();
    loop {
        let panes = env.run(&["panes", "--json"]);
        if panes.contains(context) {
            return;
        }
        assert!(start.elapsed() < Duration::from_secs(20), "the pane's context is read: {panes}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A tool that acts waits for the pane's kubeconfig to be read again, in a list that keeps
/// the keys typed meanwhile (Esc gives up: nothing starts), and runs where it was asked for or
/// not at all.
#[cfg(target_os = "linux")]
#[test]
fn a_tool_waits_for_its_check_and_runs_only_where_it_was_asked() {
    let env = Env::new("toolwait");
    let kube = env.root.join("kube.yaml");
    std::fs::write(&kube, kubeconfig_text("dev")).unwrap();
    let marked = env.root.join("marked");
    let cfg = env.root.join("config/kraal/config.toml");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str(&format!("\n[tools.mark]\ncommand = \"touch {}\"\nkey = \"m\"\n", marked.display()));
    std::fs::write(&cfg, text).unwrap();
    env.workspace("w", &format!("layout {{\n    {}\n}}\n", kube_shell_pane(&env, "k", &kube)));
    // Reads of the kubeconfig take 3 s while `kube.yaml.slow` exists, as on a stalled disk.
    let out = env.cmd(&["open", "w"]).env("KRAAL_TEST_SLOW_KUBECONFIG_MS", "3000").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_for_context(&env, "dev");
    env.wait_for_text("k", "$");
    std::fs::write(env.root.join("kube.yaml.slow"), "").unwrap();
    // The same content, a new modification time: the next read is a real, slow one.
    let touch = || std::fs::write(&kube, kubeconfig_text("dev")).unwrap();
    let mut client = env.client();
    let mut typed = |bytes: &[u8]| std::io::Write::write_all(client.stdin.as_mut().unwrap(), bytes).unwrap();

    // Given up while it waits: the keys stay in the list, and nothing starts.
    touch();
    typed(b"\x02Rm");
    std::thread::sleep(Duration::from_millis(400));
    typed(b"zz");
    std::thread::sleep(Duration::from_millis(300));
    typed(b"\x1b");
    std::thread::sleep(Duration::from_millis(4000));
    assert!(!marked.exists(), "a tool given up on started anyway");
    let screen = env.run(&["read", "-p", "k"]);
    assert!(!screen.contains("zz"), "keys typed while the tool waited reached the pane:\n{screen}");

    // The pane moves while the tool waits: it does not start somewhere else.
    touch();
    typed(b"\x02Rm");
    std::thread::sleep(Duration::from_millis(400));
    env.run(&["send", "-p", "k", "-e", "cd /tmp"]);
    std::thread::sleep(Duration::from_millis(4500));
    assert!(!marked.exists(), "the tool started in a place it was not asked for");

    // Otherwise it runs once the kubeconfig has been read.
    touch();
    typed(b"\x02Rm");
    let start = Instant::now();
    while !marked.exists() {
        assert!(start.elapsed() < Duration::from_secs(10), "the tool runs after the check");
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = client.kill();
    let _ = client.wait();
}

/// Keys broadcast to a pane whose kubeconfig is being read again wait for the read: once it
/// says production, they are asked about before reaching it.
#[cfg(target_os = "linux")]
#[test]
fn broadcast_waits_while_a_pane_context_is_read_again() {
    let env = Env::new("bcastread");
    let kube = env.root.join("kube.yaml");
    std::fs::write(&kube, kubeconfig_text("dev")).unwrap();
    env.workspace(
        "w",
        &format!(
            "layout {{\n    pane split_direction=\"vertical\" {{\n        pane name=\"a\"\n        {}\n    }}\n}}\n",
            kube_shell_pane(&env, "b", &kube)
        ),
    );
    // A real read of the kubeconfig takes 4.8 s while `kube.yaml.slow` exists: less than the
    // 5 s after which it would count as unknown.
    let out = env.cmd(&["open", "w"]).env("KRAAL_TEST_SLOW_KUBECONFIG_MS", "4800").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_for_context(&env, "dev");
    env.wait_for_text("b", "$");
    let mut client = env.client();
    let mut typed = |bytes: &[u8]| std::io::Write::write_all(client.stdin.as_mut().unwrap(), bytes).unwrap();
    env.run(&["focus", "a"]);
    typed(b"\x02b");
    std::thread::sleep(Duration::from_millis(500));
    // `kubectx prod-x`: the next scan (within 3.4 s) starts a read that lasts 4.8 s.
    std::fs::write(env.root.join("kube.yaml.slow"), "").unwrap();
    std::fs::write(&kube, kubeconfig_text("prod-x")).unwrap();
    std::thread::sleep(Duration::from_millis(3900));
    typed(b"echo hello-bcast");
    std::thread::sleep(Duration::from_millis(600));
    for pane in ["a", "b"] {
        let screen = env.run(&["read", "-p", pane]);
        assert!(!screen.contains("hello-bcast"), "typed while {pane}'s context was read:\n{screen}");
    }
    // The read ends, says production: the keys are asked about, not sent.
    std::thread::sleep(Duration::from_millis(5000));
    let screen = env.run(&["read", "-p", "b"]);
    assert!(!screen.contains("hello-bcast"), "keys reached a production pane unasked:\n{screen}");
    typed(b"y");
    env.wait_for_text("b", "echo hello-bcast");
    env.wait_for_text("a", "echo hello-bcast");
    let _ = client.kill();
    let _ = client.wait();
}

/// What is typed while a broadcast pane's context is read again goes as one: when the read
/// says production and the answer is no, none of it is sent, the Enter at its end included.
#[cfg(target_os = "linux")]
#[test]
fn a_no_drops_everything_typed_while_a_context_was_read() {
    let env = Env::new("heldno");
    let kube = env.root.join("kube.yaml");
    std::fs::write(&kube, kubeconfig_text("dev")).unwrap();
    env.workspace(
        "w",
        &format!(
            "layout {{\n    pane split_direction=\"vertical\" {{\n        pane name=\"a\"\n        {}\n    }}\n}}\n",
            kube_shell_pane(&env, "b", &kube)
        ),
    );
    let out = env.cmd(&["open", "w"]).env("KRAAL_TEST_SLOW_KUBECONFIG_MS", "4800").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_for_context(&env, "dev");
    env.wait_for_text("b", "$");
    let mut client = env.client();
    let mut typed = |bytes: &[u8]| std::io::Write::write_all(client.stdin.as_mut().unwrap(), bytes).unwrap();
    env.run(&["focus", "a"]);
    typed(b"\x02b");
    std::thread::sleep(Duration::from_millis(500));
    std::fs::write(env.root.join("kube.yaml.slow"), "").unwrap();
    std::fs::write(&kube, kubeconfig_text("prod-x")).unwrap();
    std::thread::sleep(Duration::from_millis(3900));
    // A whole command, Enter included, typed while the read goes on.
    typed(b" echo should-not-run\r");
    std::thread::sleep(Duration::from_millis(5600));
    // The read said production: the question is about all of it. No.
    typed(b"n");
    std::thread::sleep(Duration::from_millis(1500));
    for pane in ["a", "b"] {
        let screen = env.run(&["read", "-p", pane]);
        assert!(!screen.contains("should-not-run"), "sent to {pane} after a no:\n{screen}");
    }
    let _ = client.kill();
    let _ = client.wait();
}

/// A yes is for the panes the question named, as they were then: given while one of them has
/// its context read again, it waits for the read, and nothing goes if the read changed it.
#[cfg(target_os = "linux")]
#[test]
fn a_yes_waits_for_a_context_read_and_holds_only_if_nothing_changed() {
    let env = Env::new("heldyes");
    let kube_b = env.root.join("kube-b.yaml");
    let kube_c = env.root.join("kube-c.yaml");
    std::fs::write(&kube_b, kubeconfig_text("prod-x")).unwrap();
    std::fs::write(&kube_c, kubeconfig_text("dev")).unwrap();
    env.workspace(
        "w",
        &format!(
            "layout {{\n    pane split_direction=\"vertical\" {{\n        pane name=\"a\"\n        {}\n        {}\n    }}\n}}\n",
            kube_shell_pane(&env, "b", &kube_b),
            kube_shell_pane(&env, "c", &kube_c)
        ),
    );
    let out = env.cmd(&["open", "w"]).env("KRAAL_TEST_SLOW_KUBECONFIG_MS", "4800").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_for_context(&env, "prod-x");
    wait_for_context(&env, "dev");
    env.wait_for_text("b", "$");
    env.wait_for_text("c", "$");
    let mut client = env.client();
    let mut typed = |bytes: &[u8]| std::io::Write::write_all(client.stdin.as_mut().unwrap(), bytes).unwrap();
    env.run(&["focus", "a"]);
    typed(b"\x02b");
    std::thread::sleep(Duration::from_millis(500));
    // b is production: the paste is asked about.
    typed(b"\x1b[200~echo answered-early\x1b[201~");
    std::thread::sleep(Duration::from_millis(500));
    // Meanwhile c becomes production, and its read lasts.
    std::fs::write(env.root.join("kube-c.yaml.slow"), "").unwrap();
    std::fs::write(&kube_c, kubeconfig_text("prod-x")).unwrap();
    std::thread::sleep(Duration::from_millis(3900));
    // Yes, before c's read ends: it was not production when asked.
    typed(b"y");
    std::thread::sleep(Duration::from_millis(6000));
    for pane in ["a", "b", "c"] {
        let screen = env.run(&["read", "-p", pane]);
        assert!(!screen.contains("answered-early"), "sent to {pane} though c changed after the question:\n{screen}");
    }
    // Asked again (c is production now), answered during a read that changes nothing: it goes,
    // once the read has ended.
    typed(b"\x1b[200~echo answered-again\x1b[201~");
    std::thread::sleep(Duration::from_millis(500));
    std::fs::write(&kube_c, kubeconfig_text("prod-x")).unwrap();
    std::thread::sleep(Duration::from_millis(3900));
    typed(b"y");
    for pane in ["a", "b", "c"] {
        env.wait_for_text(pane, "echo answered-again");
    }
    let _ = client.kill();
    let _ = client.wait();
}

/// What is typed in a tab while a broadcast pane's context is read again goes to that tab's
/// panes once the read ends, even when another tab is shown by then.
#[cfg(target_os = "linux")]
#[test]
fn input_that_waited_goes_where_it_was_typed() {
    let env = Env::new("heldtab");
    let kube = env.root.join("kube.yaml");
    std::fs::write(&kube, kubeconfig_text("dev")).unwrap();
    env.workspace(
        "w",
        &format!(
            "layout {{\n    tab name=\"one\" {{\n        pane split_direction=\"vertical\" {{\n            pane name=\"a\"\n            {}\n        }}\n    }}\n    tab name=\"two\" {{\n        pane split_direction=\"vertical\" {{\n            pane name=\"c\"\n            pane name=\"d\"\n        }}\n    }}\n}}\n",
            kube_shell_pane(&env, "b", &kube)
        ),
    );
    let out = env.cmd(&["open", "w"]).env("KRAAL_TEST_SLOW_KUBECONFIG_MS", "4800").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_for_context(&env, "dev");
    env.wait_for_text("b", "$");
    let mut client = env.client();
    let mut typed = |bytes: &[u8]| std::io::Write::write_all(client.stdin.as_mut().unwrap(), bytes).unwrap();
    env.run(&["focus", "a"]);
    typed(b"\x02b");
    std::thread::sleep(Duration::from_millis(500));
    // The same context written again: a read that changes nothing, and lasts.
    std::fs::write(env.root.join("kube.yaml.slow"), "").unwrap();
    std::fs::write(&kube, kubeconfig_text("dev")).unwrap();
    std::thread::sleep(Duration::from_millis(3900));
    typed(b"echo typed-in-one");
    std::thread::sleep(Duration::from_millis(300));
    // Tab two is shown before the read ends.
    typed(b"\x022");
    std::thread::sleep(Duration::from_millis(6000));
    for pane in ["c", "d"] {
        let screen = env.run(&["read", "-p", pane]);
        assert!(!screen.contains("typed-in-one"), "reached {pane} in the tab shown later:\n{screen}");
    }
    env.wait_for_text("a", "echo typed-in-one");
    env.wait_for_text("b", "echo typed-in-one");
    let _ = client.kill();
    let _ = client.wait();
}

/// A yes given while a pane's kubeconfig is read again and the read does not answer is not
/// taken for the context known before the read (the file may say another one now): nothing goes.
#[cfg(target_os = "linux")]
#[test]
fn a_yes_during_a_read_that_does_not_answer_sends_nothing() {
    let env = Env::new("heldstall");
    let kube = env.root.join("kube.yaml");
    std::fs::write(&kube, kubeconfig_text("prod-x")).unwrap();
    env.workspace(
        "w",
        &format!(
            "layout {{\n    pane split_direction=\"vertical\" {{\n        pane name=\"a\"\n        {}\n    }}\n}}\n",
            kube_shell_pane(&env, "b", &kube)
        ),
    );
    // A read of the kubeconfig takes 20 s while `kube.yaml.slow` exists: after 5 s it does not
    // answer.
    let out = env.cmd(&["open", "w"]).env("KRAAL_TEST_SLOW_KUBECONFIG_MS", "20000").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_for_context(&env, "prod-x");
    env.wait_for_text("b", "$");
    let mut client = env.client();
    let mut typed = |bytes: &[u8]| std::io::Write::write_all(client.stdin.as_mut().unwrap(), bytes).unwrap();
    env.run(&["focus", "a"]);
    typed(b"\x02b");
    std::thread::sleep(Duration::from_millis(500));
    // b is production: the paste is asked about.
    typed(b"\x1b[200~echo stalled-yes\x1b[201~");
    std::thread::sleep(Duration::from_millis(500));
    // Meanwhile b's kubeconfig changes, and the read of it does not answer.
    std::fs::write(env.root.join("kube.yaml.slow"), "").unwrap();
    std::fs::write(&kube, kubeconfig_text("dev")).unwrap();
    std::thread::sleep(Duration::from_millis(9500));
    typed(b"y");
    std::thread::sleep(Duration::from_millis(1500));
    for pane in ["a", "b"] {
        let screen = env.run(&["read", "-p", pane]);
        assert!(!screen.contains("stalled-yes"), "sent to {pane} on a yes for what the file said before:\n{screen}");
    }
    let _ = client.kill();
    let _ = client.wait();
}

/// As for a kubeconfig: once the directory a Terraform run worked on is read again and does not
/// answer, the workspace read before no longer counts, production or not. A yes given for it
/// sends nothing.
#[cfg(target_os = "linux")]
#[test]
fn a_yes_during_a_terraform_read_that_does_not_answer_sends_nothing() {
    let env = Env::new("tfstall");
    let infra = env.root.join("infra");
    std::fs::create_dir_all(infra.join(".terraform")).unwrap();
    let environment = infra.join(".terraform/environment");
    std::fs::write(&environment, "prod-a\n").unwrap();
    // A Terraform run, seen while it runs: the pane's workspace is the one of the directory it
    // ran in, read off the event loop.
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(&bin.join("terraform"), "#!/bin/sh\nsleep 2\nexport PS1='$ '\nexec sh\n");
    env.workspace(
        "w",
        &format!(
            "layout {{\n    pane split_direction=\"vertical\" {{\n        pane name=\"a\"\n        \
             pane name=\"b\" cwd=\"{}\" command=\"{}\" {{ args \"plan\"; }}\n    }}\n}}\n",
            infra.display(),
            bin.join("terraform").display()
        ),
    );
    // A read of the environment file takes 20 s while `environment.slow` exists: after 5 s it
    // does not answer.
    let out = env.cmd(&["open", "w"]).env("KRAAL_TEST_SLOW_KUBECONFIG_MS", "20000").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_for_context(&env, "tf:prod-a");
    env.wait_for_text("b", "$");
    let mut client = env.client();
    let mut typed = |bytes: &[u8]| std::io::Write::write_all(client.stdin.as_mut().unwrap(), bytes).unwrap();
    env.run(&["focus", "a"]);
    typed(b"\x02b");
    std::thread::sleep(Duration::from_millis(500));
    // b is production: the paste is asked about.
    typed(b"\x1b[200~echo stalled-tf-yes\x1b[201~");
    std::thread::sleep(Duration::from_millis(500));
    // Meanwhile b's workspace changes, and the read of it does not answer.
    std::fs::write(infra.join(".terraform/environment.slow"), "").unwrap();
    std::fs::write(&environment, "prod-b\n").unwrap();
    std::thread::sleep(Duration::from_millis(9500));
    typed(b"y");
    std::thread::sleep(Duration::from_millis(1500));
    for pane in ["a", "b"] {
        let screen = env.run(&["read", "-p", pane]);
        assert!(!screen.contains("stalled-tf-yes"), "sent to {pane} on a yes for the workspace before:\n{screen}");
    }
    let _ = client.kill();
    let _ = client.wait();
}

/// The same with no Terraform run seen: the workspace is that of the directory the pane is in.
/// Once a read of it does not answer, the workspace read before no longer counts, and a yes
/// given for it sends nothing.
#[cfg(target_os = "linux")]
#[test]
fn a_yes_during_a_read_of_the_directory_a_pane_is_in_that_does_not_answer_sends_nothing() {
    let env = Env::new("tfdirstall");
    let infra = env.root.join("infra");
    std::fs::create_dir_all(infra.join(".terraform")).unwrap();
    let environment = infra.join(".terraform/environment");
    std::fs::write(&environment, "prod-a\n").unwrap();
    env.workspace(
        "w",
        &format!(
            "layout {{\n    pane split_direction=\"vertical\" {{\n        pane name=\"a\"\n        \
             pane name=\"b\" cwd=\"{}\" command=\"sh\"\n    }}\n}}\n",
            infra.display()
        ),
    );
    // A read of the environment file takes 20 s while `environment.slow` exists: after 5 s it
    // does not answer.
    let out = env.cmd(&["open", "w"]).env("KRAAL_TEST_SLOW_KUBECONFIG_MS", "20000").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_for_context(&env, "tf:prod-a");
    env.wait_for_text("b", "$");
    let mut client = env.client();
    let mut typed = |bytes: &[u8]| std::io::Write::write_all(client.stdin.as_mut().unwrap(), bytes).unwrap();
    env.run(&["focus", "a"]);
    typed(b"\x02b");
    std::thread::sleep(Duration::from_millis(500));
    // b is production: the paste is asked about.
    typed(b"\x1b[200~echo stalled-dir-yes\x1b[201~");
    std::thread::sleep(Duration::from_millis(500));
    // Meanwhile the directory's workspace changes, and the read of it does not answer.
    std::fs::write(infra.join(".terraform/environment.slow"), "").unwrap();
    std::fs::write(&environment, "prod-b\n").unwrap();
    std::thread::sleep(Duration::from_millis(9500));
    typed(b"y");
    std::thread::sleep(Duration::from_millis(1500));
    for pane in ["a", "b"] {
        let screen = env.run(&["read", "-p", pane]);
        assert!(!screen.contains("stalled-dir-yes"), "sent to {pane} on a yes for the workspace before:\n{screen}");
    }
    let _ = client.kill();
    let _ = client.wait();
}

/// A directory known to hold a production Terraform workspace stays known after the pane leaves
/// it: coming back, input is guarded at once, not only after the next read.
#[cfg(target_os = "linux")]
#[test]
fn a_production_directory_left_and_come_back_to_is_still_guarded() {
    let env = Env::new("tfback");
    let infra = env.root.join("infra");
    std::fs::create_dir_all(infra.join(".terraform")).unwrap();
    std::fs::write(infra.join(".terraform/environment"), "prod-a\n").unwrap();
    env.workspace("w", &format!("layout {{\n    pane name=\"b\" cwd=\"{}\" command=\"sh\"\n}}\n", infra.display()));
    env.run(&["open", "w"]);
    wait_for_context(&env, "tf:prod-a");
    env.wait_for_text("b", "$");
    let refused = |text: &str| !env.cmd(&["send", "-p", "b", "-e", text]).output().unwrap().status.success();
    assert!(refused("echo here"), "b is production");
    env.run(&["send", "-p", "b", "-e", "--production", "cd /"]);
    // A scan (every 3 s) passes while no pane is in the directory.
    std::thread::sleep(Duration::from_millis(4000));
    assert!(!refused("echo away"), "away from the directory, b is not production");
    env.run(&["send", "-p", "b", "-e", &format!("cd {}", infra.display())]);
    std::thread::sleep(Duration::from_millis(300));
    assert!(refused("echo back"), "back in the directory, b is production again at once");
}

/// The pane named `name`, as `kraal panes --json` lists it.
#[cfg(target_os = "linux")]
fn pane_json(env: &Env, name: &str) -> serde_json::Value {
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    panes.as_array().unwrap().iter().find(|p| p["name"] == name).cloned().unwrap()
}

/// Wait until the pane named `name` shows `contexts` and is production for `why` (or not).
#[cfg(target_os = "linux")]
fn wait_for_contexts(env: &Env, name: &str, contexts: &[&str], why: Option<&str>) {
    let start = Instant::now();
    loop {
        let pane = pane_json(env, name);
        if pane["contexts"] == serde_json::json!(contexts) && pane["production"] == serde_json::json!(why) {
            return;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "never {contexts:?} ({why:?}): {pane}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A shell on a server with what `kraal integration show` prints in its startup file reports
/// what it works on: its pane shows it, and is guarded like a local one, until the session ends.
/// The `ssh` here is `script`, which gives the shell a terminal of its own and relays it as ssh
/// does; the shell finds kraal by asking it, through that relay.
#[cfg(target_os = "linux")]
fn contexts_over_ssh_hold_until_the_session_ends(shell: &str) {
    let script = ["/usr/bin/script", "/bin/script"].into_iter().map(Path::new).find(|p| p.exists());
    let Some(script) = script else { return };
    if Command::new(shell).arg("-c").arg("true").output().is_err() {
        return;
    }
    let env = Env::new(&format!("sshctx-{shell}"));
    let config = env.root.join("config/kraal/config.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{text}[servers]\nauto_add = false\n")).unwrap();
    // The server's home: a kubeconfig on production, and the shell integration.
    let remote = env.root.join("remote");
    std::fs::create_dir_all(remote.join(".kube")).unwrap();
    let kube = "contexts:\n- context:\n    namespace: payments\n  name: prod-eks\n- name: dev\n  context: {}\n\
        current-context: prod-eks\n";
    std::fs::write(remote.join(".kube/config"), kube).unwrap();
    std::fs::write(remote.join("dev.yaml"), kube.replace("current-context: prod-eks", "current-context: dev")).unwrap();
    let block = env.run(&["integration", "show", shell]);
    let r = remote.display();
    let start = match shell {
        "bash" => {
            std::fs::write(remote.join(".bashrc"), format!("PS1='remote$ '\n{block}\n")).unwrap();
            format!("bash --rcfile {r}/.bashrc -i")
        }
        "zsh" => {
            std::fs::write(remote.join(".zshrc"), format!("PS1='remote$ '\n{block}\n")).unwrap();
            format!("env ZDOTDIR={r} zsh --no-globalrcs -i")
        }
        _ => {
            let fish = remote.join(".config/fish");
            std::fs::create_dir_all(fish.join("conf.d")).unwrap();
            std::fs::write(fish.join("conf.d/kraal.fish"), format!("{block}\n")).unwrap();
            let prompt = "set -g fish_greeting\nfunction fish_prompt\n    printf 'remote$ '\nend\n";
            std::fs::write(fish.join("config.fish"), prompt).unwrap();
            format!("env XDG_CONFIG_HOME={r}/.config XDG_DATA_HOME={r}/.local/share fish -i")
        }
    };
    let export = |k: &str, v: &str| match shell {
        "fish" => format!("set -gx {k} {v}"),
        _ => format!("export {k}={v}"),
    };
    let login = env.root.join("login");
    write_script(
        &login,
        &format!(
            "#!/bin/sh\nexec env -i PATH=/usr/bin:/bin HOME={r} TERM=xterm-256color \
             SSH_CONNECTION='10.0.0.1 50000 10.0.0.5 22' {start}\n"
        ),
    );
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(script, bin.join("ssh")).unwrap();
    env.workspace("w", "layout {\n    pane name=\"ops\"\n}\n");
    env.run(&["open", "w"]);
    env.wait_for_text("ops", "$");
    // `ssh -q -f -c <cipher> bastion`, as kraal reads it; `script` writes its log to ./bastion.
    let connect = format!(
        "cd {} && SHELL=/bin/sh {} -q -f -c {} bastion",
        env.root.display(),
        bin.join("ssh").display(),
        login.display()
    );
    env.run(&["send", "-p", "ops", "--enter", &connect]);
    wait_for_contexts(&env, "ops", &["⎈ prod-eks/payments"], Some("Kubernetes context prod-eks on bastion"));
    assert_eq!(pane_json(&env, "ops")["server"], "bastion");
    env.wait_for_text("ops", "remote$");
    let out = env.try_run(&["send", "-p", "ops", "--enter", "echo hi"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("is production"), "{}", String::from_utf8_lossy(&out.stderr));
    // Another kubeconfig on the server: the next prompt says so.
    env.run(&["send", "-p", "ops", "--production", "--enter", &export("KUBECONFIG", "$HOME/dev.yaml")]);
    wait_for_contexts(&env, "ops", &["⎈ dev"], None);
    // A command that runs and ends keeps it.
    env.run(&["send", "-p", "ops", "--enter", "true"]);
    env.run(&["send", "-p", "ops", "--enter", &export("AWS_PROFILE", "prd-admin")]);
    wait_for_contexts(&env, "ops", &["⎈ dev", "aws:prd-admin"], Some("AWS profile prd-admin on bastion"));
    // The session ends: what it reported goes with it.
    env.run(&["send", "-p", "ops", "--production", "--enter", "exit"]);
    wait_for_contexts(&env, "ops", &[], None);
    assert!(pane_json(&env, "ops")["server"].is_null());
    // A local shell's report is not taken: kraal reads local contexts itself.
    env.run(&["send", "-p", "ops", "--enter", "printf '\\033]7701;context;k8s=prod-x\\007'; echo printed"]);
    env.wait_for_text("ops", "printed");
    std::thread::sleep(Duration::from_millis(1000));
    wait_for_contexts(&env, "ops", &[], None);
}

#[cfg(target_os = "linux")]
#[test]
fn contexts_bash_reports_over_ssh_hold_until_the_session_ends() {
    contexts_over_ssh_hold_until_the_session_ends("bash");
}

#[cfg(target_os = "linux")]
#[test]
fn contexts_zsh_reports_over_ssh_hold_until_the_session_ends() {
    contexts_over_ssh_hold_until_the_session_ends("zsh");
}

/// fish 4 marks its prompts itself too: twice marked, a prompt keeps what was reported.
#[cfg(target_os = "linux")]
#[test]
fn contexts_fish_reports_over_ssh_hold_until_the_session_ends() {
    contexts_over_ssh_hold_until_the_session_ends("fish");
}

/// When a long paste turns out to go into production while its last pieces wait for a context
/// read, the rest of it does not go, and neither does what was typed after it: an Enter would
/// run the part of it that went.
#[cfg(target_os = "linux")]
#[test]
fn a_long_paste_cut_for_production_takes_the_enter_after_it_along() {
    let env = Env::new("heldpiece");
    let kube = env.root.join("kube.yaml");
    std::fs::write(&kube, kubeconfig_text("dev")).unwrap();
    env.workspace("w", &format!("layout {{\n    {}\n}}\n", kube_shell_pane(&env, "k", &kube)));
    let out = env.cmd(&["open", "w"]).env("KRAAL_TEST_SLOW_KUBECONFIG_MS", "4800").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_for_context(&env, "dev");
    env.wait_for_text("k", "$");
    // What reaches the pane goes to a file, byte for byte.
    let got = env.root.join("got");
    env.run(&["send", "-p", "k", "-e", &format!("stty raw -echo; cat > {}", got.display())]);
    std::thread::sleep(Duration::from_millis(500));
    let mut client = RawClient::attach(&env, "w");
    std::thread::sleep(Duration::from_millis(500));
    // More than a paste held whole (8 MiB): it goes as it comes.
    let piece = vec![b'a'; 64 << 10];
    client.send(b"\x1b[200~");
    for _ in 0..(9 << 20) / piece.len() {
        client.send(&piece);
    }
    // `kubectx prod-x`: the next scan (within 3.4 s) starts a read that lasts 4.8 s, and the
    // rest of the paste, then an Enter, wait for it.
    std::fs::write(env.root.join("kube.yaml.slow"), "").unwrap();
    std::fs::write(&kube, kubeconfig_text("prod-x")).unwrap();
    std::thread::sleep(Duration::from_millis(3900));
    for _ in 0..16 {
        client.send(&piece);
    }
    client.send(b"\x1b[201~");
    client.send(b"\r");
    // The read ends and says production.
    std::thread::sleep(Duration::from_millis(6000));
    let bytes = std::fs::read(&got).unwrap();
    assert!(bytes.len() >= 8 << 20, "the start of the paste went: {} bytes", bytes.len());
    assert!(!bytes.contains(&b'\r'), "the Enter typed after a cut paste reached the pane");
    assert!(bytes.len() < (9 << 20) + 16 * piece.len(), "the rest of the paste went into production");
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
fn account_rules_see_a_worktree_anywhere_as_its_repository() {
    let env = Env::new("acctrule");
    let repo = env.root.join("company/repo");
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
    git(&["commit", "-q", "--allow-empty", "-m", "init"]);
    // Linked worktrees far from the repository: only git can tell where they belong.
    let feature = env.root.join("elsewhere/feature");
    let other = env.root.join("elsewhere/other");
    git(&["worktree", "add", "-q", "-b", "feature", feature.to_str().unwrap()]);
    git(&["worktree", "add", "-q", "-b", "other", other.to_str().unwrap()]);
    // A stand-in for claude that says which config home it was given.
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(&bin.join("claude"), "#!/bin/sh\necho \"home=${CLAUDE_CONFIG_DIR:-default}\"\nexec sleep 100\n");
    let pro = env.root.join("pro-home");
    std::fs::create_dir_all(&pro).unwrap();
    let repo = std::fs::canonicalize(&repo).unwrap();
    std::fs::write(
        env.root.join("config/kraal/accounts.toml"),
        format!(
            "[[account]]\nname = \"pro\"\nagent = \"claude\"\nhome = {:?}\n\n[[rule]]\npath = {:?}\naccounts = [\"pro\"]\n",
            pro.display().to_string(),
            repo.display().to_string()
        ),
    )
    .unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let run = |args: &[&str], dir: &Path| {
        let out = env.cmd(args).env("PATH", &path).current_dir(dir).output().unwrap();
        assert!(out.status.success(), "kraal {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    // From a definition: the pane waits for git, then starts on the account the rule names.
    env.workspace("w", &format!("layout {{\n    pane name=\"ag\" agent=\"claude\" cwd={:?}\n}}\n", feature.display()));
    run(&["open", "w"], &env.root);
    env.wait_for_text("ag", &format!("home={}", pro.display()));
    // From the CLI inside the session, in the other worktree: the same.
    let socket = env.root.join("run/kraal/acctrule.sock");
    let out = env
        .cmd(&["agent", "claude", "--tab"])
        .env("PATH", &path)
        .env("KRAAL_SOCKET", &socket)
        .current_dir(&other)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let ids: Vec<String> = panes.as_array().unwrap().iter().map(|p| p["id"].to_string()).collect();
    let start = Instant::now();
    loop {
        let screens: Vec<String> = ids.iter().map(|id| env.run(&["read", "-p", id.as_str()])).collect();
        if screens.iter().filter(|s| s.contains(&format!("home={}", pro.display()))).count() == 2 {
            break;
        }
        assert!(
            !screens.iter().any(|s| s.contains("home=default")),
            "an agent started on an account the rule forbids: {screens:?}"
        );
        assert!(start.elapsed() < Duration::from_secs(10), "{screens:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn an_agent_waiting_for_its_account_waits_again_after_a_restart() {
    let env = Env::new("pendagent");
    let repo = env.root.join("company/repo");
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
    git(&["commit", "-q", "--allow-empty", "-m", "init"]);
    let feature = env.root.join("elsewhere/feature");
    git(&["worktree", "add", "-q", "-b", "feature", feature.to_str().unwrap()]);
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(&bin.join("claude"), "#!/bin/sh\necho \"home=${CLAUDE_CONFIG_DIR:-default}\"\nexec sleep 100\n");
    // A git that hangs on the question kraal asks while `slow` exists: the agent cannot get its
    // account before the session is stopped.
    let real_git =
        String::from_utf8(Command::new("sh").args(["-c", "command -v git"]).output().unwrap().stdout).unwrap();
    let slow = env.root.join("slow");
    write_script(
        &bin.join("git"),
        &format!(
            "#!/bin/sh\ncase \"$*\" in *--git-common-dir*) [ -e {slow:?} ] && exec sleep 8 ;; esac\nexec {} \"$@\"\n",
            real_git.trim()
        ),
    );
    std::fs::write(&slow, "").unwrap();
    let pro = env.root.join("pro-home");
    std::fs::create_dir_all(&pro).unwrap();
    let canonical = std::fs::canonicalize(&repo).unwrap();
    std::fs::write(
        env.root.join("config/kraal/accounts.toml"),
        format!(
            "[[account]]\nname = \"pro\"\nagent = \"claude\"\nhome = {:?}\n\n[[rule]]\npath = {:?}\naccounts = [\"pro\"]\n",
            pro.display().to_string(),
            canonical.display().to_string()
        ),
    )
    .unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let run = |args: &[&str]| {
        let out = env.cmd(args).env("PATH", &path).current_dir(&env.root).output().unwrap();
        assert!(out.status.success(), "kraal {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    env.workspace("w", &format!("layout {{\n    pane name=\"ag\" agent=\"claude\" cwd={:?}\n}}\n", feature.display()));
    run(&["open", "w"]);
    env.wait_for_text("ag", "waits for the account rules");
    run(&["kill"]);
    let state = std::fs::read_to_string(env.state_file()).unwrap();
    assert!(state.contains("pending_agent"), "{state}");
    // Restarted, with git answering again: the agent starts on the account the rule names.
    std::fs::remove_file(&slow).unwrap();
    run(&["open", "w"]);
    env.wait_for_text("ag", &format!("home={}", pro.display()));
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

/// A git repository at `repo` with one commit of `files`, and a function running git in it.
fn git_repo(repo: &Path, files: &[(&str, &str)]) -> impl Fn(&[&str]) {
    let dir = repo.to_path_buf();
    let git = move |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    for (path, text) in files {
        let path = repo.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }
    git(&["init", "-q", "-b", "main"]);
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "init"]);
    git
}

#[test]
fn worktrees_are_prepared_numbered_and_pruned_from_the_command_line() {
    let env = Env::new("wtcli");
    let repo = env.root.join("app");
    let git = git_repo(
        &repo,
        &[
            (".gitignore", ".env*\nsetup.log\n"),
            (
                ".kraal/worktree.toml",
                "copy = [\".env*\"]\nsetup = ['echo \"$KRAAL_WORKTREE #$KRAAL_WORKTREE_INDEX on $PORT\" > setup.log']\n",
            ),
        ],
    );
    std::fs::write(repo.join(".env"), "A=1\n").unwrap();
    std::fs::write(env.root.join("secret"), "private\n").unwrap();
    std::os::unix::fs::symlink(env.root.join("secret"), repo.join(".env.prod")).unwrap();
    let config = env.root.join("config/kraal/config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("\n[worktrees]\nport_base = 3000\n");
    std::fs::write(&config, text).unwrap();
    let wt = |args: &[&str]| env.cmd(&[&["wt"], args].concat()).current_dir(&repo).output().unwrap();
    let text =
        |out: &Output| format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let dir = env.root.join("app.worktrees");

    // Not approved yet, and nobody to ask: nothing is made.
    let out = wt(&["new", "one"]);
    assert!(!out.status.success() && text(&out).contains("--yes") && text(&out).contains("1. echo"), "{}", text(&out));
    assert!(!dir.join("one").exists());
    // Left as git makes it.
    assert!(wt(&["new", "one", "--no-setup"]).status.success());
    assert!(!dir.join("one/.env").exists());
    // Approved: the files are copied, never through a link out of the repository, and the
    // commands run with the worktree's number and port.
    let out = wt(&["new", "two", "--yes"]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(std::fs::read_to_string(dir.join("two/setup.log")).unwrap(), "two #2 on 3020\n");
    assert_eq!(std::fs::read_to_string(dir.join("two/.env")).unwrap(), "A=1\n");
    assert!(!dir.join("two/.env.prod").exists());
    assert!(text(&out).contains("not copied: .env.prod (a link out of the repository)"), "{}", text(&out));
    // That version runs again without asking.
    let out = wt(&["new", "three"]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(std::fs::read_to_string(dir.join("three/setup.log")).unwrap(), "three #3 on 3030\n");
    // Changed: asked again. A failing command fails the command; the worktree stays.
    std::fs::write(repo.join(".kraal/worktree.toml"), "setup = [\"exit 3\"]\n").unwrap();
    git(&["commit", "-q", "-am", "fail"]);
    assert!(!wt(&["new", "four"]).status.success());
    assert!(!dir.join("four").exists());
    let out = wt(&["new", "four", "--yes"]);
    assert!(!out.status.success() && text(&out).contains("the setup of four failed (exit 3)"), "{}", text(&out));
    assert!(dir.join("four").exists());
    let list = String::from_utf8(wt(&["list"]).stdout).unwrap();
    assert!(list.lines().any(|l| l.starts_with("two") && l.contains("#2")), "{list}");

    // Merged and clean ones go; one with a change or a commit of its own stays, and says why.
    std::fs::write(dir.join("three/notes.txt"), "draft\n").unwrap();
    let in_two = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir.join("two"))
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    };
    std::fs::write(dir.join("two/feature.txt"), "x\n").unwrap();
    in_two(&["add", "feature.txt"]);
    in_two(&["commit", "-q", "-m", "feature"]);
    let out = wt(&["prune", "--dry-run"]);
    let said = text(&out);
    assert!(said.contains("would remove") && said.contains("kraal/one") && said.contains("kraal/four"), "{said}");
    assert!(said.contains("kept three: it has 1 uncommitted change"), "{said}");
    assert!(said.contains("kept two: it has 1 unmerged commit"), "{said}");
    assert!(dir.join("one").exists(), "a dry run removes nothing");
    let out = wt(&["prune"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(!dir.join("one").exists() && !dir.join("four").exists());
    assert!(dir.join("two/feature.txt").exists() && dir.join("three/notes.txt").exists());
    let list = String::from_utf8(wt(&["list"]).stdout).unwrap();
    assert!(!list.contains("one") && list.contains("two") && list.contains("three"), "{list}");
}

/// From the agent picker: the worktree is made once its repository's setup is approved, the
/// setup runs in the agent's pane, and the agent starts there with the worktree's number. A
/// setup that fails leaves the agent waiting for Enter; a no makes nothing. Merged worktrees
/// are then removed in one go, their panes closed.
#[cfg(target_os = "linux")]
#[test]
fn an_agent_worktree_is_prepared_in_its_pane_once_trusted() {
    let env = Env::new("wtsetup");
    let repo = env.root.join("app");
    let git = git_repo(
        &repo,
        &[
            (".gitignore", ".env\n"),
            (
                ".kraal/worktree.toml",
                "copy = [\".env\"]\nsetup = ['echo \"preparing $KRAAL_WORKTREE\"', \"test -f .env\"]\n",
            ),
        ],
    );
    std::fs::write(repo.join(".env"), "A=1\n").unwrap();
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(
        &bin.join("claude"),
        "#!/bin/sh\necho \"agent in $KRAAL_WORKTREE #$KRAAL_WORKTREE_INDEX port $PORT\"\nexec sleep 100\n",
    );
    let home = env.root.join("alpha-home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        env.root.join("config/kraal/accounts.toml"),
        format!(
            "[[account]]\nname = \"alpha\"\nagent = \"claude\"\nhome = {:?}\n\n[[rule]]\nworkspace = \"w\"\naccounts = [\"alpha\"]\n",
            home.display().to_string()
        ),
    )
    .unwrap();
    let config = env.root.join("config/kraal/config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("\n[worktrees]\nport_base = 3000\n");
    std::fs::write(&config, text).unwrap();
    env.workspace("w", &format!("layout {{\n    pane name=\"shell\" cwd={:?}\n}}\n", repo.display().to_string()));
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mut client = RawClient::attach(&env, "w");
    // The agent picker offers worktrees once git has seen the repository.
    client.wait_for_screen("⎇ main");
    let new_worktree = |client: &mut RawClient, name: &str| {
        env.run(&["focus", "shell"]);
        client.screen.lock().unwrap().clear();
        client.send(b"\x02a");
        client.wait_for_screen("in a new worktree");
        client.send(b"claude in a new worktree");
        std::thread::sleep(Duration::from_millis(200));
        client.send(b"\r");
        client.wait_for_screen("Name of the worktree");
        client.send(name.as_bytes());
        client.send(b"\r");
        client.wait_for_screen("Trust the worktree setup");
    };
    let dir = env.root.join("app.worktrees");
    // The pane comes once git has made the worktree.
    let wait_for = |pane: &str, text: &str| -> String {
        let start = Instant::now();
        loop {
            let out = env.try_run(&["read", "-p", pane]);
            let screen = String::from_utf8_lossy(&out.stdout).into_owned();
            if out.status.success() && screen.contains(text) {
                return screen;
            }
            assert!(start.elapsed() < Duration::from_secs(15), "timed out waiting for {text:?} in {pane}:\n{screen}");
            std::thread::sleep(Duration::from_millis(50));
        }
    };

    new_worktree(&mut client, "feat");
    assert!(!dir.join("feat").exists(), "nothing is made before the answer");
    client.send(b"y");
    let screen = wait_for("claude feat", "agent in feat #1 port 3010");
    assert!(screen.contains("preparing feat") && screen.contains("copied from the main checkout: .env"), "{screen}");

    std::fs::write(repo.join(".kraal/worktree.toml"), "setup = [\"echo about to fail\", \"exit 3\"]\n").unwrap();
    git(&["commit", "-q", "-am", "fail"]);
    new_worktree(&mut client, "bad");
    client.send(b"y");
    let screen = wait_for("claude bad", "Enter starts this anyway");
    assert!(screen.contains("about to fail") && !screen.contains("agent in"), "{screen}");
    env.run(&["focus", "claude bad"]);
    client.send(b"\r");
    wait_for("claude bad", "agent in bad #2 port 3020");

    std::fs::write(repo.join(".kraal/worktree.toml"), "setup = [\"echo changed\"]\n").unwrap();
    git(&["commit", "-q", "-am", "changed"]);
    new_worktree(&mut client, "no");
    client.screen.lock().unwrap().clear();
    client.send(b"n");
    client.wait_for_screen("was not created");
    assert!(!dir.join("no").exists());

    // Both worktrees have nothing to lose: they go, with their agents' panes.
    env.run(&["focus", "shell"]);
    client.screen.lock().unwrap().clear();
    client.send(b"\x02Rg");
    client.wait_for_screen("Remove merged worktrees");
    client.send(b"\r");
    client.wait_for_screen("Closes these panes");
    client.send(b"y");
    let start = Instant::now();
    while dir.join("feat").exists() || dir.join("bad").exists() {
        assert!(start.elapsed() < Duration::from_secs(15), "the merged worktrees are still there");
        std::thread::sleep(Duration::from_millis(100));
    }
    let panes = env.run(&["panes"]);
    assert!(!panes.contains("claude feat") && !panes.contains("claude bad"), "{panes}");

    // From the command line inside the session: prepared there, and the agent opened in the
    // session gets the worktree's number (the first, free again).
    let socket = env.root.join("run/kraal/wtsetup.sock");
    let out = env
        .cmd(&["wt", "new", "cli", "--agent", "claude", "--yes"])
        .env("PATH", &path)
        .env("KRAAL_SOCKET", &socket)
        .current_dir(&repo)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stdout).contains("changed"), "the setup ran there");
    wait_for("claude", "agent in cli #1 port 3010");
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

#[cfg(unix)]
#[test]
fn tunnels_are_up_when_ssh_listens_and_survive_a_restart() {
    let env = Env::new("tunnels");
    // A fake ssh: with "ok" in the destination, says it listens and makes its listener (as
    // `ssh -v` does); with "taken", says it listens but cannot bind (another program has the
    // port); else says nothing. Then it stays, as a tunnel does. Its pid goes in a file.
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let pids = env.root.join("ssh-pids");
    write_script(
        &bin.join("ssh"),
        &format!(
            "#!/bin/sh\nprev=; for a; do [ \"$prev\" = -L ] && fwd=$a; prev=$a; dest=$a; done\n\
             port=$(echo \"$fwd\" | cut -d: -f2)\necho $$ >> {}\n\
             case $dest in *ok*|*taken*) echo \"debug1: Local forwarding listening on 127.0.0.1 port $port.\" >&2;; esac\n\
             case $dest in *ok*) echo \"debug1: channel 0: new port-listener [port listener] (inactive timeout: 0)\" >&2;;\n\
             *taken*) echo \"bind [127.0.0.1]:$port: Address already in use\" >&2;; esac\n\
             exec sleep 600\n",
            pids.display()
        ),
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let run = |args: &[&str]| {
        let out = env.cmd(args).env("PATH", &path).output().unwrap();
        assert!(out.status.success(), "kraal {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let free = || std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port();
    let state_of = |port: u16| {
        run(&["tunnel", "list"])
            .lines()
            .find(|l| l.ends_with(&format!("localhost:{port}")))
            .map(|l| l.split_whitespace().next().unwrap_or("").to_string())
    };
    let wait_state = |port: u16, want: &str| {
        let start = Instant::now();
        while state_of(port).as_deref() != Some(want) {
            assert!(start.elapsed() < Duration::from_secs(10), "{port}: {:?}", run(&["tunnel", "list"]));
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    run(&["open", "w"]);

    let (up, quiet) = (free(), free());
    run(&["tunnel", "add", "ok@a.lan", "5432", "--local", &up.to_string()]);
    wait_state(up, "up");
    // Another program listening on the port of a tunnel whose ssh is still connecting does not
    // make the tunnel up.
    run(&["tunnel", "add", "quiet@b.lan", "5433", "--local", &quiet.to_string()]);
    let squatter = std::net::TcpListener::bind(("127.0.0.1", quiet)).unwrap();
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(state_of(quiet).as_deref(), Some("connecting"));
    drop(squatter);
    // ssh announcing its listener is not enough: the bind may fail after it.
    let taken = free();
    run(&["tunnel", "add", "taken@e.lan", "5434", "--local", &taken.to_string()]);
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(state_of(taken).as_deref(), Some("connecting"));
    // Two tunnels without a local port never get the same one, even before either listens.
    let wanted = free();
    run(&["tunnel", "add", "quiet@c.lan", &wanted.to_string()]);
    run(&["tunnel", "add", "quiet@d.lan", &wanted.to_string()]);
    let list = run(&["tunnel", "list"]);
    assert_eq!(list.matches(&format!("localhost:{wanted}")).count(), 1, "{list}");

    // Closing a tunnel stops its ssh.
    let before: Vec<i32> =
        std::fs::read_to_string(&pids).unwrap().lines().filter_map(|l| l.trim().parse().ok()).collect();
    run(&["tunnel", "rm", &quiet.to_string()]);
    let alive = |pid: i32| unsafe { libc::kill(pid, 0) } == 0;
    let start = Instant::now();
    while before.iter().filter(|p| alive(**p)).count() > 4 {
        assert!(start.elapsed() < Duration::from_secs(5), "the closed tunnel's ssh still runs");
        std::thread::sleep(Duration::from_millis(20));
    }

    // A tunnel through a connection kraal knows by no name comes back through that connection.
    run(&["kill"]);
    run(&["open", "w"]);
    wait_state(up, "up");
    let list = run(&["tunnel", "list"]);
    assert!(list.contains(&format!("ok@a.lan:5432 → localhost:{up}")), "{list}");
}

#[cfg(unix)]
#[test]
fn an_ssh_pane_keeps_the_account_it_was_opened_with() {
    let env = Env::new("account");
    // A fake ssh: logs its arguments; runs a remote script here, and ends at once otherwise (so
    // kraal never sees an ssh process in the pane, only what it started).
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let log = env.root.join("ssh.log");
    write_script(
        &bin.join("ssh"),
        &format!(
            "#!/bin/sh\necho \"$*\" >> {}\nfor a; do cmd=$a; done\ncase $cmd in *' '*) exec sh -c \"$cmd\";; esac\nexit 0\n",
            log.display()
        ),
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    // The inventory's `web` is root's.
    std::fs::write(
        env.root.join("config/kraal/servers.toml"),
        "[[server]]\nname = \"web\"\nhost = \"web.lan\"\nuser = \"root\"\n",
    )
    .unwrap();
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    let run = |args: &[&str], pane: Option<&str>| {
        let mut c = env.cmd(args);
        c.env("PATH", &path);
        if let Some(p) = pane {
            c.env("KRAAL_PANE", p).env("KRAAL_SESSION", &env.session);
        }
        let out = c.output().unwrap();
        assert!(out.status.success(), "kraal {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    run(&["open", "w"], None);
    run(&["ssh", "deploy@web.lan", "--tab"], None);
    let panes: serde_json::Value = serde_json::from_str(&run(&["panes", "--json"], None)).unwrap();
    let pane =
        panes.as_array().unwrap().iter().find(|p| p["server"] == "web").unwrap_or_else(|| panic!("{panes}"))["id"]
            .to_string();
    // A runbook on that pane (whose ssh already ended) goes as deploy, not as the inventory's root.
    let file = env.root.join("config/kraal/runbooks/where.toml");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "on = \"pane\"\nsteps = [\"echo here\"]\n").unwrap();
    let out = run(&["runbook", "run", "where", "--yes", "--wait"], Some(&pane));
    assert!(out.contains("finished: 1 ok"), "{out}");
    let log = std::fs::read_to_string(&log).unwrap();
    let remote = log.lines().filter(|l| l.contains("sh -c")).collect::<Vec<_>>();
    assert!(!remote.is_empty() && remote.iter().all(|l| l.contains("deploy@web.lan")), "{log}");
}

#[test]
fn a_long_paste_into_production_is_asked_about_and_sent_whole() {
    let env = Env::new("prodpaste");
    let tf = env.root.join("infra");
    std::fs::create_dir_all(tf.join(".terraform")).unwrap();
    std::fs::write(tf.join(".terraform/environment"), "prod\n").unwrap();
    let got = env.root.join("pasted");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    // A program that takes 2 MB as they come (no line editing), then says it has them all.
    let script = format!("stty raw -echo; head -c 2000000 > {}; echo; echo GOT-ALL; sleep 60", got.display());
    env.run(&["run", "--cwd", &tf.display().to_string(), "--name", "ops", "--", "sh", "-c", &script]);
    wait_production(&env, "ops");
    env.run(&["focus", "ops"]);
    let mut client = RawClient::attach(&env, "w");
    // 20000 lines, more than the pieces a paste is passed on in.
    let data = ("x".repeat(99) + "\n").repeat(20000);
    client.paste(data.as_bytes());
    // One question about all of it, and nothing sent before the answer.
    client.wait_for_screen("Paste 20000 lines into production");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(std::fs::metadata(&got).map(|m| m.len()).unwrap_or(0), 0);
    client.send(b"y");
    env.wait_for_text("ops", "GOT-ALL");
    assert_eq!(std::fs::metadata(&got).unwrap().len(), 2_000_000);
}

#[test]
fn a_long_paste_keeps_going_where_it_started_and_never_into_production() {
    let env = Env::new("streampaste");
    let tf = env.root.join("infra");
    std::fs::create_dir_all(tf.join(".terraform")).unwrap();
    std::fs::write(tf.join(".terraform/environment"), "prod\n").unwrap();
    let (got, prod_got) = (env.root.join("pasted"), env.root.join("prod-pasted"));
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let take = |file: &Path| format!("stty raw -echo; cat > {}", file.display());
    env.run(&["run", "--cwd", &tf.display().to_string(), "--name", "ops", "--", "sh", "-c", &take(&prod_got)]);
    wait_production(&env, "ops");
    env.run(&["run", "--name", "sink", "--", "sh", "-c", &take(&got)]);
    env.run(&["focus", "sink"]);
    let mut client = RawClient::attach(&env, "w");
    // More than kraal holds back (8 MiB): it goes out as it comes.
    let line = "x".repeat(99) + "\n";
    client.send(b"\x1b[200~");
    for chunk in line.repeat(90_000).as_bytes().chunks(64 << 10) {
        client.send(chunk);
    }
    let size = |f: &Path| std::fs::metadata(f).map(|m| m.len()).unwrap_or(0);
    let start = Instant::now();
    while size(&got) == 0 {
        assert!(start.elapsed() < Duration::from_secs(10), "the paste never started going out");
        std::thread::sleep(Duration::from_millis(50));
    }
    // The focus moves to a production pane while the rest comes: the rest still goes where the
    // paste started, never there.
    env.run(&["focus", "ops"]);
    for chunk in line.repeat(10_000).as_bytes().chunks(64 << 10) {
        client.send(chunk);
    }
    client.send(b"\x1b[201~");
    let start = Instant::now();
    while size(&got) < 10_000_000 {
        assert!(start.elapsed() < Duration::from_secs(20), "only {} bytes arrived", size(&got));
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(size(&got), 10_000_000);
    assert_eq!(size(&prod_got), 0, "nothing of the paste reached production");
}

#[test]
fn copy_mode_releases_the_button_the_program_saw_pressed() {
    let env = Env::new("copyrelease");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    // A program that asks for SGR mouse reports and shows what it gets.
    env.run(&["run", "--name", "app", "--", "sh", "-c", "stty raw -echo; printf '\\033[?1000h\\033[?1006h'; cat -v"]);
    env.run(&["focus", "app"]);
    std::thread::sleep(Duration::from_millis(300));
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_screen("app");
    // The right button goes down over the program, copy mode starts (ctrl+b v), the wheel
    // turns: the program is told the right button is up, and nothing more.
    client.send(b"\x1b[<2;110;20M");
    env.wait_for_text("app", "<2;");
    client.send(b"\x02v");
    std::thread::sleep(Duration::from_millis(200));
    client.send(b"\x1b[<64;110;20M");
    let screen = env.wait_for_text("app", "m");
    let reports: Vec<&str> = screen.split("^[[").filter(|r| r.starts_with('<')).collect();
    assert!(reports.iter().any(|r| r.starts_with("<2;") && r.trim_end().ends_with('m')), "{screen}");
    assert!(!reports.iter().any(|r| r.starts_with("<0;")), "{screen}");
}

#[test]
fn a_pane_is_told_when_the_terminal_loses_and_regains_the_focus() {
    let env = Env::new("focusreports");
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    // A program that asks for focus reports and shows what it gets.
    env.run(&["run", "--name", "app", "--", "sh", "-c", "stty raw -echo; printf '\\033[?1004h'; cat -v"]);
    std::thread::sleep(Duration::from_millis(300));
    let mut client = RawClient::attach(&env, "w");
    env.wait_for_text("app", "^[[I");
    // Out and back in, in one read: both are told, in order.
    client.send(b"\x1b[O\x1b[I");
    env.wait_for_text("app", "^[[I^[[O^[[I");
    // The same when the focus moves away and back by commands.
    env.run(&["focus", "main"]);
    env.run(&["focus", "app"]);
    env.wait_for_text("app", "^[[I^[[O^[[I^[[O^[[I");
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

#[test]
fn an_account_switch_never_resends_a_prompt_by_itself_and_one_that_fails_can_be_restarted() {
    let env = Env::new("switchacct");
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    // A stand-in for claude that says which config home and arguments it was given.
    write_script(
        &bin.join("claude"),
        "#!/bin/sh\necho \"home=${CLAUDE_CONFIG_DIR:-default} args=$*\"\nexec sleep 100\n",
    );
    let (alpha, beta) = (env.root.join("alpha-home"), env.root.join("beta-home"));
    std::fs::create_dir_all(&alpha).unwrap();
    std::fs::create_dir_all(beta.join("projects")).unwrap();
    std::fs::write(
        env.root.join("config/kraal/accounts.toml"),
        format!(
            "[[account]]\nname = \"alpha\"\nagent = \"claude\"\nhome = {:?}\n\n[[account]]\nname = \"beta\"\nagent = \"claude\"\nhome = {:?}\n",
            alpha.display().to_string(),
            beta.display().to_string()
        ),
    )
    .unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let socket = env.root.join("run/kraal/switchacct.sock");
    let agent = |args: &[&str]| -> String {
        let mut all = vec!["agent", "claude", "--account", "alpha", "--tab", "--"];
        all.extend_from_slice(args);
        let out = env.cmd(&all).env("PATH", &path).env("KRAAL_SOCKET", &socket).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let p = panes.as_array().unwrap().iter().find(|p| p["command"].to_string().contains(args[0])).unwrap();
        p["id"].to_string()
    };
    let info = |id: &str| -> serde_json::Value {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let id: u64 = id.parse().unwrap();
        panes.as_array().unwrap().iter().find(|p| p["id"] == id).unwrap().clone()
    };
    let until = |what: &str, ok: &dyn Fn() -> bool| {
        let start = Instant::now();
        while !ok() {
            assert!(start.elapsed() < Duration::from_secs(10), "never: {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let on_alpha = format!("home={}", alpha.display());
    let on_beta = format!("home={}", beta.display());
    let mut client = RawClient::attach(&env, "w");

    // 1. An option kraal does not know, then a word: its value, or a prompt. Switching stops the
    // agent and prepares it on the other account, but shows the command: Enter runs it.
    let odd = agent(&["--brand-new-flag", "Deploy now"]);
    env.wait_for_text(&odd, &on_alpha);
    env.run(&["focus", &odd]);
    client.send(b"\x02Ac");
    client.wait_for_screen("beta");
    client.send(b"beta");
    std::thread::sleep(Duration::from_millis(200));
    client.send(b"\r");
    until("the switched agent waits", &|| info(&odd)["suspended"] == true);
    let screen = env.wait_for_text(&odd, "cannot tell");
    assert!(!screen.contains(&on_beta), "it ran by itself: {screen}");
    client.send(b"\r");
    env.wait_for_text(&odd, &on_beta);
    let screen = env.run(&["read", "-p", &odd]).replace('\n', "");
    assert!(screen.contains("--brand-new-flag Deploy now"), "everything after the unknown option stays: {screen}");

    // 2. A switch whose conversation cannot follow (beta cannot write it) leaves the pane ended
    // on its account, and Enter starts it again there, resuming its conversation.
    let plain = agent(&["--verbose"]);
    env.wait_for_text(&plain, &on_alpha);
    let command: Vec<String> =
        info(&plain)["command"].as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_string()).collect();
    let session = command.iter().skip_while(|a| *a != "--session-id").nth(1).expect("a session id").clone();
    std::fs::create_dir_all(alpha.join("projects/-work")).unwrap();
    std::fs::write(alpha.join(format!("projects/-work/{session}.jsonl")), "{}\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(beta.join("projects"), std::fs::Permissions::from_mode(0o555)).unwrap();
    env.run(&["focus", &plain]);
    client.send(b"\x02Ac");
    client.wait_for_screen("beta");
    client.send(b"beta");
    std::thread::sleep(Duration::from_millis(200));
    client.send(b"\r");
    until("the failed switch leaves the agent ended", &|| !info(&plain)["exit_code"].is_null());
    assert_eq!(info(&plain)["suspended"], false);
    client.send(b"\r");
    // Lines may wrap in the pane: compare without line ends.
    let resumed = format!("{on_alpha} args=--resume {session} --verbose");
    until("it resumes on alpha", &|| env.run(&["read", "-p", &plain]).replace('\n', "").contains(&resumed));
    assert!(!env.run(&["read", "-p", &plain]).replace('\n', "").contains(&on_beta));
    std::fs::set_permissions(beta.join("projects"), std::fs::Permissions::from_mode(0o755)).unwrap();
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

/// Drive the agent of pane `pane` in `env`'s session through Claude's hooks.
fn claude_hook(env: &Env, pane: &str, payload: &str) {
    use std::io::Write;
    let socket = env.root.join("run/kraal").join(format!("{}.sock", env.session));
    let mut c = env.cmd(&["hook", "claude"]);
    c.env("KRAAL_PANE", pane).env("KRAAL_SOCKET", &socket).stdin(std::process::Stdio::piped());
    let mut child = c.spawn().unwrap();
    child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
    assert!(child.wait_with_output().unwrap().status.success());
}

#[test]
fn agents_follow_prints_the_list_each_time_it_changes() {
    let env = Env::new("follow");
    // Before the session runs it has no agents, and the list waits for it.
    let (mut follow, lines) = env.follow(&["agents", "--follow"]);
    let next = || -> serde_json::Value {
        let line = lines.recv_timeout(Duration::from_secs(10)).expect("a list in time");
        serde_json::from_str(&line).unwrap_or_else(|e| panic!("{e}: {line}"))
    };
    assert_eq!(next(), serde_json::json!([]));
    env.workspace("w", "layout {\n    pane name=\"api\"\n}\n");
    env.run(&["open", "w"]);
    claude_hook(&env, "api", r#"{"hook_event_name":"SessionStart","session_id":"s-1"}"#);
    claude_hook(&env, "api", r#"{"hook_event_name":"UserPromptSubmit","session_id":"s-1"}"#);
    let mut list = next();
    while list[0]["state"] != "working" {
        list = next();
    }
    assert_eq!((list[0]["name"].as_str(), list[0]["agent"].as_str()), (Some("api"), Some("claude")), "{list}");
    claude_hook(&env, "api", r#"{"hook_event_name":"Notification","message":"Claude needs your permission"}"#);
    while list[0]["state"] != "blocked" {
        list = next();
    }
    // The session ends: no agents again.
    env.run(&["kill"]);
    while list != serde_json::json!([]) {
        list = next();
    }
    let _ = follow.kill();
    let _ = follow.wait();
    // It ends once what reads it is gone, even when it has nothing new to say.
    let mut alone = env.cmd(&["agents", "--follow"]).stdout(std::process::Stdio::piped()).spawn().unwrap();
    let mut first = [0u8; 3];
    std::io::Read::read_exact(alone.stdout.as_mut().unwrap(), &mut first).unwrap();
    assert_eq!(&first, b"[]\n");
    drop(alone.stdout.take());
    let start = Instant::now();
    while alone.try_wait().unwrap().is_none() {
        assert!(start.elapsed() < Duration::from_secs(10), "still running with nobody to read it");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(unix)]
#[test]
fn agents_of_a_remote_session_are_listed_and_reached() {
    let local = Env::new("rlocal");
    let remote = Env::new("rremote");
    // The server's kraal: an agent in a tab that is not the one shown.
    remote.workspace("w", "layout {\n    pane name=\"api\"\n}\n");
    remote.run(&["open", "w"]);
    remote.run(&["run", "--tab", "-n", "other", "--", "sh"]);
    claude_hook(&remote, "api", r#"{"hook_event_name":"SessionStart","session_id":"s-1"}"#);
    claude_hook(&remote, "api", r#"{"hook_event_name":"UserPromptSubmit","session_id":"s-1"}"#);
    let remote_api = || -> serde_json::Value {
        let panes: serde_json::Value = serde_json::from_str(&remote.run(&["panes", "--json"])).unwrap();
        panes.as_array().unwrap().iter().find(|p| p["name"] == "api").unwrap().clone()
    };
    let api = remote_api()["id"].to_string();
    assert_eq!(remote_api()["focused"], false);

    // A fake ssh: runs the command "on the server", that is here, as the server's kraal (with its
    // own directories); "down.lan" does not answer. Every call is logged.
    let bin = local.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_kraal"), bin.join("kraal")).unwrap();
    let log = local.root.join("ssh.log");
    write_script(
        &bin.join("ssh"),
        &format!(
            "#!/bin/sh\necho \"$*\" >> {log}\nfor a; do cmd=$a; done\n\
             case \"$*\" in *down.lan*) echo 'ssh: connect to host down.lan port 22: Connection refused' >&2; exit 255;; esac\n\
             unset KRAAL KRAAL_SOCKET KRAAL_SESSION KRAAL_PANE\n\
             export XDG_RUNTIME_DIR={r}/run XDG_STATE_HOME={r}/state XDG_CONFIG_HOME={r}/config\n\
             exec sh -c \"$cmd\"\n",
            log = log.display(),
            r = remote.root.display()
        ),
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let inventory = local.root.join("config/kraal/servers.toml");
    std::fs::write(
        &inventory,
        "[[server]]\nname = \"gpu\"\nhost = \"gpu.lan\"\nagents = \"rremote\"\n\n\
         [[server]]\nname = \"down\"\nhost = \"down.lan\"\nagents = \"main\"\n\n\
         [[server]]\nname = \"gpu2\"\nhost = \"gpu2.lan\"\n",
    )
    .unwrap();
    local.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    let out = local.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let local_panes = || -> Vec<serde_json::Value> {
        let panes: serde_json::Value = serde_json::from_str(&local.run(&["panes", "--json"])).unwrap();
        panes.as_array().unwrap().clone()
    };
    let ssh_log = || std::fs::read_to_string(&log).unwrap_or_default();

    // The sidebar lists the agent after its server's name. The server that does not answer is
    // said once, and tried again later, not at once.
    let mut client = RawClient::attach(&local, "w");
    client.wait_for_screen("gpu › ");
    client.wait_for_screen("working · claude · w");
    client.wait_for_screen("the agents of down cannot be listed");
    std::thread::sleep(Duration::from_secs(3));
    let tries = ssh_log().matches("down.lan").count();
    assert!(tries <= 2, "a server that is down is not hammered: {tries} tries in about 3 s");
    assert!(local.run(&["agents"]).trim().is_empty(), "kraal agents lists this session's own agents only");

    // Blocked there: announced here.
    let (mut events, notified) = local.follow(&["events", "notification"]);
    claude_hook(&remote, "api", r#"{"hook_event_name":"Notification","message":"Claude needs your permission"}"#);
    let line = notified.recv_timeout(Duration::from_secs(10)).expect("a notification");
    let v: serde_json::Value = serde_json::from_str(&line).unwrap();
    let said = (v["title"].as_str(), v["body"].as_str());
    assert_eq!(said, (Some("claude · api on gpu"), Some("needs your input")), "{line}");
    let _ = events.kill();
    let _ = events.wait();
    client.wait_for_screen("blocked · claude · w");

    // alt+a: the next agent needing you is there; a tab attaches to that session, its pane shown.
    client.send(b"\x1ba");
    let attach = |p: &serde_json::Value| {
        let cmd: Vec<&str> = p["command"].as_array().unwrap().iter().filter_map(|a| a.as_str()).collect();
        cmd.get(1..).is_some_and(|a| a == ["-s", "rremote", "attach", "--host", "gpu", "--pane", api.as_str()])
    };
    let start = Instant::now();
    while !local_panes().iter().any(attach) {
        assert!(start.elapsed() < Duration::from_secs(10), "no tab attached: {:?}", local_panes());
        std::thread::sleep(Duration::from_millis(50));
    }
    let start = Instant::now();
    while remote_api()["focused"] != true {
        assert!(start.elapsed() < Duration::from_secs(10), "the agent's pane is not shown there: {}", remote_api());
        std::thread::sleep(Duration::from_millis(50));
    }
    // Again: the tab already attached is used, and that session asked to show the pane.
    client.send(b"\x1ba");
    let start = Instant::now();
    while !ssh_log().contains(&format!("-s rremote focus {api}")) {
        assert!(start.elapsed() < Duration::from_secs(10), "{}", ssh_log());
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(local_panes().len(), 2, "{:?}", local_panes());

    // Attaching to a server makes kraal list that session's agents from then on.
    let kraal = env!("CARGO_BIN_EXE_kraal");
    local.run(&[
        "run",
        "-n",
        "gpu2",
        "--",
        "env",
        "-u",
        "KRAAL_PANE",
        kraal,
        "-s",
        "rremote",
        "attach",
        "--host",
        "gpu2",
    ]);
    let start = Instant::now();
    while std::fs::read_to_string(&inventory).unwrap().matches("agents = \"rremote\"").count() < 2 {
        assert!(start.elapsed() < Duration::from_secs(10), "{}", std::fs::read_to_string(&inventory).unwrap());
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(std::fs::read_to_string(&inventory).unwrap().contains("agents = \"main\""), "the others stay as written");
    drop(client);
}

/// A release as GitHub serves it, made from this build: `kraal-<target>.tar.gz` and its
/// checksum, in `dir`.
fn fake_release(dir: &Path) -> String {
    fake_release_of(dir, None)
}

/// A release, made from this build; with `version`, its kraal says it is that version and is
/// this build otherwise.
fn fake_release_of(dir: &Path, version: Option<&str>) -> String {
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        _ => "unknown-linux-musl",
    };
    let target = format!("{}-{os}", std::env::consts::ARCH);
    let inner = dir.join(format!("kraal-{target}"));
    std::fs::create_dir_all(&inner).unwrap();
    match version {
        Some(v) => write_script(
            &inner.join("kraal"),
            &format!(
                "#!/bin/sh\n[ \"$1\" = --version ] && {{ echo 'kraal {v}'; exit 0; }}\nexec '{}' \"$@\"\n",
                env!("CARGO_BIN_EXE_kraal")
            ),
        ),
        None => {
            std::fs::copy(env!("CARGO_BIN_EXE_kraal"), inner.join("kraal")).unwrap();
        }
    }
    let asset = format!("kraal-{target}.tar.gz");
    let tar = Command::new("tar")
        .arg("-czf")
        .arg(dir.join(&asset))
        .arg("-C")
        .arg(dir)
        .arg(format!("kraal-{target}"))
        .status()
        .unwrap();
    assert!(tar.success());
    let sum = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "cd '{}' && (sha256sum {asset} 2>/dev/null || shasum -a 256 {asset}) > {asset}.sha256",
            dir.display()
        ))
        .status()
        .unwrap();
    assert!(sum.success());
    asset
}

/// `kraal update` puts a release that matches its checksum in place of itself, and leaves
/// itself alone when the checksum is wrong.
#[test]
fn update_installs_a_checked_release_in_place() {
    let env = Env::new("update");
    let release = env.root.join("release");
    let asset = fake_release(&release);
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let kraal = bin.join("kraal");
    std::fs::copy(env!("CARGO_BIN_EXE_kraal"), &kraal).unwrap();
    let url = format!("file://{}", release.display());
    let update =
        |args: &[&str]| env.isolated(&kraal).arg("update").args(args).env("KRAAL_DOWNLOAD_URL", &url).output().unwrap();

    let same = update(&[]);
    assert!(same.status.success(), "{}", String::from_utf8_lossy(&same.stderr));
    assert!(String::from_utf8_lossy(&same.stdout).contains("already installed"));

    let before = std::fs::metadata(&kraal).unwrap().modified().unwrap();
    std::thread::sleep(Duration::from_millis(20));
    let forced = update(&["--force"]);
    assert!(forced.status.success(), "{}", String::from_utf8_lossy(&forced.stderr));
    assert_ne!(std::fs::metadata(&kraal).unwrap().modified().unwrap(), before, "replaced");
    assert!(Command::new(&kraal).arg("--version").status().unwrap().success());
    let left: Vec<_> = std::fs::read_dir(&bin).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(left, ["kraal"], "nothing left beside it");

    std::fs::write(release.join(format!("{asset}.sha256")), format!("{}  {asset}\n", "0".repeat(64))).unwrap();
    let before = std::fs::metadata(&kraal).unwrap().modified().unwrap();
    let wrong = update(&["--force"]);
    assert!(!wrong.status.success());
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("does not match its checksum"));
    assert_eq!(std::fs::metadata(&kraal).unwrap().modified().unwrap(), before, "left alone");
}

/// After an update, `kraal update` names the sessions still on an older kraal: one that can
/// restart live, where nothing stops, or one that cannot, with what a restart stops in it. It
/// asks on a terminal (no by default when programs would stop), and has the new kraal restart
/// them when told to.
#[test]
fn update_offers_to_restart_the_sessions_left_on_an_older_kraal() {
    let env = Env::new("updrestart");
    env.workspace(
        "w",
        "layout {\n    pane name=\"job\" command=\"sh\" { args \"-c\" \"echo started; exec sleep 4243\"; }\n}\n",
    );
    env.run(&["open", "w"]);
    env.wait_for_text("job", "started");
    let release = env.root.join("release");
    fake_release_of(&release, Some("9.9.9"));
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let kraal = bin.join("kraal");
    // Each update replaces this copy with the release's kraal: put this build back first.
    let install = || {
        let _ = std::fs::remove_file(&kraal);
        std::fs::copy(env!("CARGO_BIN_EXE_kraal"), &kraal).unwrap();
    };
    let url = format!("file://{}", release.display());
    let update = |args: &[&str]| {
        let mut c = env.isolated(&kraal);
        c.arg("update").args(args).env("KRAAL_DOWNLOAD_URL", &url);
        c
    };
    let version = env!("CARGO_PKG_VERSION");
    let pid = env.server_pid().expect("the server runs");
    // While a `kraal wait` waits, the session cannot restart live (nor anywhere but on Linux).
    let mut wait = env.cmd(&["wait", "-p", "job", "--until", "exit", "--timeout", "120"]).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(300));

    // Without a terminal to ask on, and without --restart: said, not done.
    install();
    let out = update(&[]).output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{said}{}", String::from_utf8_lossy(&out.stderr));
    assert!(said.contains(&format!("Session updrestart still runs kraal {version}.")), "{said}");
    assert!(said.contains("It cannot restart live: "), "{said}");
    assert!(said.contains("These programs stop:") && said.contains("\"job\" in w)"), "{said}");
    assert!(said.contains(&format!("Session updrestart keeps kraal {version} until: kraal -s updrestart restart")));
    assert_eq!(env.server_pid(), Some(pid));

    // On a terminal: asked, and Enter says no, since a program would stop.
    #[cfg(target_os = "linux")]
    {
        install();
        let screen = in_terminal(update(&[]), b"\n");
        assert!(screen.contains("Restart it on kraal 9.9.9 now? [y/N]"), "{screen}");
        assert!(screen.contains("keeps kraal"), "{screen}");
        assert_eq!(env.server_pid(), Some(pid));
        // --no-restart answers beforehand.
        install();
        let screen = in_terminal(update(&["--no-restart"]), b"");
        assert!(screen.contains("keeps kraal") && !screen.contains("[y/N]"), "{screen}");
        assert_eq!(env.server_pid(), Some(pid));
    }

    // --restart: the kraal just installed restarts it.
    install();
    let out = update(&["--restart"]).output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{said}{}", String::from_utf8_lossy(&out.stderr));
    assert!(said.contains("restarted session updrestart"), "{said}");
    let new = env.server_pid().expect("a server runs again");
    assert_ne!(new, pid);
    env.wait_for_text_since_restore("job", "restored session");
    let _ = wait.wait();

    // Live: nothing stops, and nothing is asked about stopping.
    #[cfg(target_os = "linux")]
    {
        env.run(&["run", "--name", "sleeper", "--", "sleep", "4246"]);
        install();
        let out = update(&["--restart"]).output().unwrap();
        let said = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{said}{}", String::from_utf8_lossy(&out.stderr));
        assert!(
            said.contains("Session updrestart still runs kraal") && said.contains("it restarts live: nothing stops")
        );
        assert!(!said.contains("These programs stop:"), "{said}");
        assert!(said.contains("restarted session updrestart live"), "{said}");
        assert_eq!(env.server_pid(), Some(new), "the same server");
        let running = std::fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .any(|e| std::fs::read(e.path().join("cmdline")).is_ok_and(|c| c == b"sleep\x004246\x00"));
        assert!(running, "what ran in the pane still runs");
    }
}

/// The server says what it runs to a client that attaches, and says once, to a client that
/// has a newer kraal installed, that a restart brings it. `kraal ls` shows each session's.
#[test]
fn a_client_with_a_newer_kraal_installed_is_told_the_session_runs_an_older_one() {
    let env = Env::new("notice");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    let version = env!("CARGO_PKG_VERSION");
    let newer = RawClient::attach_installed(&env, "w", Some("99.0.0"));
    newer.wait_for_message(&format!(r#"{{"type":"attached","version":"{version}"}}"#));
    newer.wait_for_screen(&format!("kraal 99.0.0 installed; this session runs {version}: ctrl+b W r restarts it"));
    let same = RawClient::attach_installed(&env, "w", Some(version));
    same.wait_for_message("\"attached\"");
    let older = RawClient::attach_installed(&env, "w", Some("0.0.1"));
    older.wait_for_message("\"attached\"");
    std::thread::sleep(Duration::from_millis(500));
    for client in [&same, &older] {
        assert!(!String::from_utf8_lossy(&client.screen.lock().unwrap()).contains(" installed; "));
    }
    let ls = env.run(&["ls"]);
    assert!(ls.lines().any(|l| l.starts_with("notice ") && l.ends_with(&format!("running  {version}"))), "{ls}");
    let keys = env.run(&["config", "keys"]);
    assert!(keys.lines().any(|l| l.trim_start().starts_with("restart ") && l.ends_with("ctrl+b W r")), "{keys}");
}

/// The restart in the menu asks first: live when it can be, where nothing stops, else saying
/// what stops. Then the session comes back, and attached clients are told to come back to the
/// workspace they showed.
#[test]
fn the_menu_restarts_the_session_after_asking() {
    let env = Env::new("menurestart");
    env.workspace(
        "w",
        "layout {\n    pane name=\"job\" command=\"sh\" { args \"-c\" \"echo started; exec sleep 4244\"; }\n}\n",
    );
    env.run(&["open", "w"]);
    env.wait_for_text("job", "started");
    let pid = env.server_pid().expect("the server runs");
    // Live: nothing stops, and the session is the same process after.
    #[cfg(target_os = "linux")]
    {
        let mut client = RawClient::attach_following(&env, "w");
        client.wait_for_message("\"attached\"");
        client.send(b"\x02Wr");
        client.wait_for_screen("Restart session menurestart live on the kraal installed now?");
        client.send(b"n");
        std::thread::sleep(Duration::from_millis(500));
        client.screen.lock().unwrap().clear();
        client.send(b"\x02Wr");
        client.wait_for_screen("Restart session menurestart live");
        client.send(b"y");
        client.wait_for_message(r#""live":true"#);
        let screen = env.wait_for_text("job", "started");
        assert!(!screen.contains("restored"), "{screen}");
        assert_eq!(env.server_pid(), Some(pid), "the same server");
    }
    // While a `kraal wait` waits, a restart cannot be live: what stops is said.
    let mut wait = env.cmd(&["wait", "-p", "job", "--until", "exit", "--timeout", "60"]).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_message("\"attached\"");
    client.send(b"\x02Wr");
    client.wait_for_screen("Restart session menurestart on the kraal installed now?");
    client.wait_for_screen("No live restart now (one stops nothing): `kraal wait`");
    client.wait_for_screen("These programs stop:");
    client.wait_for_screen("\"job\" in w)");
    client.send(b"n");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(env.server_pid(), Some(pid), "no is no");
    client.screen.lock().unwrap().clear();
    client.send(b"\x02Wr");
    client.wait_for_screen("Restart session menurestart");
    client.send(b"y");
    client.wait_for_message(r#""reattach":"w""#);
    let start = Instant::now();
    while env.server_pid().is_none_or(|p| p == pid) {
        assert!(start.elapsed() < Duration::from_secs(20), "the session did not come back");
        std::thread::sleep(Duration::from_millis(50));
    }
    env.wait_for_text_since_restore("job", "restored session");
    let _ = wait.wait();
}

/// `kraal restart` restarts live when it can: nothing stops or is listed, and a client attached
/// in a terminal stays. Otherwise (`--cold`, or when a live restart cannot happen) it says what
/// would stop before asking; a client waits for the next server and comes back to its workspace;
/// an idle shell comes back; and a restart run from a pane of the session itself outlives that
/// pane.
#[cfg(target_os = "linux")]
#[test]
fn restart_keeps_the_session_and_brings_its_clients_back() {
    let env = Env::new("restart");
    env.workspace(
        "w",
        "layout {\n    pane split_direction=\"vertical\" {\n        pane name=\"idle\"\n        pane name=\"busy\"\n    }\n}\n",
    );
    env.run(&["open", "w"]);
    env.run(&["send", "-p", "idle", "--enter", "echo before-restart"]);
    env.wait_for_text("idle", "before-restart");
    env.run(&["send", "-p", "busy", "--enter", "sleep 4245"]);
    let sleeping = || {
        std::fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .any(|e| std::fs::read(e.path().join("cmdline")).is_ok_and(|c| c == b"sleep\x004245\x00"))
    };
    let start = Instant::now();
    while !sleeping() {
        assert!(start.elapsed() < Duration::from_secs(10), "sleep never started");
        std::thread::sleep(Duration::from_millis(50));
    }
    // A client in a 30-row terminal: once it is attached, the panes are shorter than that.
    let mut client = env.client();
    let attached = || {
        let start = Instant::now();
        loop {
            let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
            if panes.as_array().unwrap().iter().all(|p| p["rows"].as_u64().unwrap() < 30) {
                return;
            }
            assert!(start.elapsed() < Duration::from_secs(20), "no client attached: {panes}");
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    attached();
    let pid = env.server_pid().expect("the server runs");

    // Live: on a terminal, asked (Enter is yes); nothing would stop, so nothing is listed.
    let screen = in_terminal(env.cmd(&["restart"]), b"n\n");
    assert!(screen.contains("it restarts live on kraal") && screen.contains("Restart session restart? [Y/n]"));
    assert!(!screen.contains("These programs stop:") && screen.contains("cancelled"), "{screen}");
    assert_eq!(env.server_pid(), Some(pid));
    // Without a terminal: nothing to ask about.
    let out = env.run(&["restart"]);
    assert!(out.contains("restarted session restart live: kraal"), "{out}");
    assert_eq!(env.server_pid(), Some(pid));
    assert!(sleeping(), "the busy pane's program runs on");
    assert!(client.try_wait().unwrap().is_none(), "the client left");
    attached();
    // From a pane of the session, the restart is done there.
    let line = format!("'{}' restart --yes", env!("CARGO_BIN_EXE_kraal"));
    env.run(&["send", "-p", "idle", "--enter", &line]);
    env.wait_for_text("idle", "restarted session restart live");
    assert_eq!(env.server_pid(), Some(pid));

    // Not live: without a terminal to ask on, what stops is said, and nothing happens.
    let out = env.try_run(&["restart", "--cold"]);
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "{said}");
    let listed = said.split_once("These programs stop:\n").map(|(_, rest)| rest).unwrap_or_default();
    assert!(listed.starts_with("  sleep 4245  (pane ") && listed.contains(" \"busy\" in w)"), "{said}");
    assert!(!said.contains("idle"), "a shell at its prompt runs nothing: {said}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("add --yes"));
    assert_eq!(env.server_pid(), Some(pid));
    // On a terminal: the list, then the question, whose Enter is no.
    let screen = in_terminal(env.cmd(&["restart", "--cold"]), b"\n");
    let (listed, asked) = (screen.find("sleep 4245"), screen.find("Restart session restart? [y/N]"));
    assert!(listed.is_some() && asked.is_some() && listed < asked, "{screen}");
    assert!(screen.contains("cancelled"), "{screen}");
    assert_eq!(env.server_pid(), Some(pid));

    // While a `kraal wait` waits, a restart cannot be live: --live fails, and otherwise it is
    // not live.
    let mut wait = env.cmd(&["wait", "-p", "busy", "--until", "exit", "--timeout", "60"]).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let out = env.try_run(&["restart", "--live", "--yes"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("cannot restart live: `kraal wait`"), "{err}");
    assert_eq!(env.server_pid(), Some(pid));
    let out = env.run(&["restart", "--yes"]);
    assert!(out.contains("cannot restart live: `kraal wait`") && out.contains("These programs stop:"), "{out}");
    assert!(out.contains("restarted session restart: kraal"), "{out}");
    let _ = wait.wait();
    let second = env.server_pid().expect("a server runs again");
    assert_ne!(second, pid);
    // The idle shell is back, with what it showed, and takes commands.
    let start = Instant::now();
    while !env.try_run(&["read", "-p", "idle", "--lines", "200"]).stdout.windows(14).any(|w| w == b"before-restart") {
        assert!(start.elapsed() < Duration::from_secs(10), "the idle pane did not come back");
        std::thread::sleep(Duration::from_millis(50));
    }
    env.run(&["send", "-p", "idle", "--enter", "echo after-restart"]);
    env.wait_for_text_since_restore("idle", "after-restart");
    // The client did not leave: it attached to the next server.
    assert!(client.try_wait().unwrap().is_none(), "the client left");
    attached();

    // From a pane of the session: the restart closes that pane, and happens all the same.
    let line = format!("'{}' restart --yes --cold", env!("CARGO_BIN_EXE_kraal"));
    env.run(&["send", "-p", "idle", "--enter", &line]);
    let start = Instant::now();
    while env.server_pid().is_none_or(|p| p == second) {
        assert!(start.elapsed() < Duration::from_secs(20), "the session did not restart");
        std::thread::sleep(Duration::from_millis(50));
    }
    let start = Instant::now();
    while !env.try_run(&["read", "-p", "idle", "--lines", "200"]).stdout.windows(13).any(|w| w == b"after-restart") {
        assert!(start.elapsed() < Duration::from_secs(10), "the idle pane did not come back");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(client.try_wait().unwrap().is_none(), "the client left");
    attached();
    let _ = client.kill();
    let _ = client.wait();
}

/// Run `cmd` in a terminal (`script`, from util-linux), typing `input` into it; what the
/// terminal showed.
#[cfg(target_os = "linux")]
fn in_terminal(cmd: Command, input: &[u8]) -> String {
    use std::io::{Read, Write};
    let line = std::iter::once(cmd.get_program())
        .chain(cmd.get_args())
        .map(|a| format!("'{}'", a.to_string_lossy()))
        .collect::<Vec<_>>()
        .join(" ");
    let mut c = Command::new("script");
    c.args(["-q", "-f", "-c", &line, "/dev/null"]);
    for (k, v) in cmd.get_envs() {
        match v {
            Some(v) => c.env(k, v),
            None => c.env_remove(k),
        };
    }
    c.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null());
    let mut child = c.spawn().expect("script (util-linux)");
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(input).unwrap();
    let mut out = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut shown = Vec::new();
        let _ = out.read_to_end(&mut shown);
        shown
    });
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        assert!(start.elapsed() < Duration::from_secs(30), "{line} is still running");
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(stdin);
    String::from_utf8_lossy(&reader.join().unwrap()).into_owned()
}

/// install.sh installs a release that matches its checksum, and nothing when it does not.
#[test]
fn install_script_installs_a_checked_release() {
    let env = Env::new("installsh");
    let release = env.root.join("release");
    let asset = fake_release(&release);
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/install.sh");
    let dir = env.root.join("bin");
    let install = || {
        Command::new("sh")
            .arg(script)
            .env("KRAAL_DOWNLOAD_URL", format!("file://{}", release.display()))
            .env("KRAAL_INSTALL_DIR", &dir)
            .output()
            .unwrap()
    };
    let done = install();
    assert!(done.status.success(), "{}", String::from_utf8_lossy(&done.stderr));
    assert!(String::from_utf8_lossy(&done.stderr).contains("Installed kraal"));
    assert!(Command::new(dir.join("kraal")).arg("--version").status().unwrap().success());

    std::fs::remove_file(dir.join("kraal")).unwrap();
    std::fs::write(release.join(format!("{asset}.sha256")), format!("{}  {asset}\n", "0".repeat(64))).unwrap();
    let wrong = install();
    assert!(!wrong.status.success());
    assert!(String::from_utf8_lossy(&wrong.stderr).contains("does not match its checksum"));
    assert!(!dir.join("kraal").exists(), "nothing installed");
}

/// A throwaway sshd on 127.0.0.1:2305 with its own host and user keys, and an `ssh` that uses
/// only them (never ~/.ssh): a server reached for real. `None` where no sshd can run here.
#[cfg(target_os = "linux")]
struct Sshd {
    child: std::process::Child,
    /// Holds the port for this test, against other test runs on this machine.
    _lock: std::fs::File,
    /// Where that `ssh` is, to put first in PATH.
    bin: PathBuf,
}

#[cfg(target_os = "linux")]
impl Sshd {
    fn start(dir: &Path) -> Option<Sshd> {
        use std::os::fd::AsRawFd;
        let sshd = ["/usr/sbin/sshd", "/usr/bin/sshd"].into_iter().map(PathBuf::from).find(|p| p.exists())?;
        let ssh =
            ["/usr/bin/ssh", "/bin/ssh", "/usr/local/bin/ssh"].into_iter().map(PathBuf::from).find(|p| p.exists())?;
        std::fs::create_dir_all(dir).unwrap();
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open("/tmp/kraal-test-sshd-2305.lock")
            .unwrap();
        // SAFETY: flock on a descriptor we own; released when it is closed.
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) };
        for key in ["host_key", "user_key"] {
            let made =
                Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(dir.join(key)).status();
            if !made.is_ok_and(|s| s.success()) {
                return None;
            }
        }
        std::fs::copy(dir.join("user_key.pub"), dir.join("authorized_keys")).unwrap();
        let user = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout).unwrap();
        let d = dir.display();
        std::fs::write(
            dir.join("sshd_config"),
            format!(
                "Port 2305\nListenAddress 127.0.0.1\nHostKey {d}/host_key\nPidFile {d}/sshd.pid\n\
                 AuthorizedKeysFile {d}/authorized_keys\nStrictModes no\nUsePAM no\nPasswordAuthentication no\n\
                 KbdInteractiveAuthentication no\nPubkeyAuthentication yes\nAllowUsers {}\n",
                user.trim()
            ),
        )
        .unwrap();
        let host_key = std::fs::read_to_string(dir.join("host_key.pub")).unwrap();
        std::fs::write(dir.join("known_hosts"), format!("[127.0.0.1]:2305 {host_key}")).unwrap();
        std::fs::write(
            dir.join("ssh_config"),
            format!(
                "Host *\n  UserKnownHostsFile {d}/known_hosts\n  GlobalKnownHostsFile /dev/null\n  \
                 StrictHostKeyChecking yes\n  IdentityFile {d}/user_key\n  IdentitiesOnly yes\n  \
                 IdentityAgent none\n  LogLevel ERROR\n"
            ),
        )
        .unwrap();
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_script(&bin.join("ssh"), &format!("#!/bin/sh\nexec '{}' -F '{d}/ssh_config' \"$@\"\n", ssh.display()));
        let log = dir.join("sshd.log");
        let child =
            Command::new(&sshd).arg("-D").arg("-f").arg(dir.join("sshd_config")).arg("-E").arg(&log).spawn().ok()?;
        let mut sshd = Sshd { child, _lock: lock, bin };
        let start = Instant::now();
        loop {
            let said = std::fs::read_to_string(&log).unwrap_or_default();
            if said.contains("Server listening") {
                return Some(sshd);
            }
            if sshd.child.try_wait().ok().flatten().is_some() || start.elapsed() > Duration::from_secs(10) {
                eprintln!("sshd did not start: {said}");
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for Sshd {
    fn drop(&mut self) {
        // SAFETY: the pid of the child this test started, not reaped yet.
        unsafe { libc::kill(self.child.id() as i32, libc::SIGTERM) };
        let _ = self.child.wait();
    }
}

/// A file of a server opens in the local editor, each save goes back to the server (its mode
/// kept), a change made on the server meanwhile is never overwritten unasked, and each answer
/// does what it says. The server is a real sshd; the editor is a script told what to do.
#[cfg(target_os = "linux")]
#[test]
fn a_server_file_is_edited_in_the_local_editor() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new("redit");
    let Some(sshd) = Sshd::start(&env.root.join("sshd")) else {
        eprintln!("skipped: no sshd can run here");
        return;
    };
    let user = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout).unwrap();
    let entry = |name: &str| {
        format!("[[server]]\nname = \"{name}\"\nhost = \"127.0.0.1\"\nport = 2305\nuser = \"{}\"\n", user.trim())
    };
    std::fs::write(env.root.join("config/kraal/servers.toml"), format!("{}\n{}", entry("box"), entry("box-prod")))
        .unwrap();
    let downloads = env.root.join("downloads");
    let cfg = env.root.join("config/kraal/config.toml");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str(&format!("\n[servers]\nmultiplex = false\ndownload_dir = \"{}\"\n", downloads.display()));
    std::fs::write(&cfg, text).unwrap();
    // The editor says which file it edits, runs each step it is given (`$f` is the file), and
    // ends when told.
    let root = env.root.display().to_string();
    let editor = env.root.join("editor");
    write_script(
        &editor,
        &format!(
            "#!/bin/sh\nfor f; do :; done\nprintf '%s' \"$f\" > '{root}/editing.tmp' && mv '{root}/editing.tmp' '{root}/editing'\n\
             n=0\nwhile :; do\n  if [ -e '{root}/step'$n ]; then . '{root}/step'$n; n=$((n + 1)); fi\n  \
             if [ -e '{root}/quit' ]; then rm -f '{root}/quit' '{root}/editing' '{root}/step'*; exit 0; fi\n  sleep 0.05\ndone\n"
        ),
    );
    let path = format!("{}:{}", sshd.bin.display(), std::env::var("PATH").unwrap_or_default());
    let kraal = |args: &[&str]| env.cmd(args).env("PATH", &path).env("EDITOR", &editor).output().unwrap();
    let wait_until = |what: &str, ok: &dyn Fn() -> bool| {
        let start = Instant::now();
        while !ok() {
            assert!(start.elapsed() < Duration::from_secs(15), "timed out waiting: {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let read = |p: &Path| std::fs::read_to_string(p).unwrap_or_default();
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;
    let step = |n: u32, script: &str| {
        let tmp = env.root.join("step.tmp");
        std::fs::write(&tmp, script).unwrap();
        std::fs::rename(&tmp, env.root.join(format!("step{n}"))).unwrap();
    };
    // Open `target`: the editor's copy.
    let open = |target: &str, extra: &[&str]| -> PathBuf {
        let mut args = vec!["edit"];
        args.extend_from_slice(extra);
        args.push(target);
        let out = kraal(&args);
        assert!(out.status.success(), "kraal edit {target}: {}", String::from_utf8_lossy(&out.stderr));
        let editing = env.root.join("editing");
        wait_until("the editor starts", &|| editing.exists());
        PathBuf::from(read(&editing))
    };
    // End the editor: its private copy goes.
    let quit = |local: &Path| {
        std::fs::write(env.root.join("quit"), "").unwrap();
        let dir = local.parent().unwrap().to_path_buf();
        wait_until("the private copy is removed", &|| !dir.exists());
    };

    env.workspace("w", "layout {\n    pane name=\"sh\"\n}\n");
    let out = kraal(&["open", "w"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mut client = RawClient::attach(&env, "w");
    std::thread::sleep(Duration::from_millis(500));
    let srv = env.root.join("srv").join("it's a \"dir\" $(touch pwned)");
    std::fs::create_dir_all(&srv).unwrap();

    // Each save goes to the server, through a new file renamed in place with the old one's mode.
    let notes = srv.join("notes 'q' $HOME;.conf");
    std::fs::write(&notes, "hello\n").unwrap();
    std::fs::set_permissions(&notes, std::fs::Permissions::from_mode(0o640)).unwrap();
    let local = open(&format!("box:{}", notes.display()), &[]);
    assert!(local.starts_with(env.root.join("run")), "the copy is in the private runtime directory: {local:?}");
    assert_eq!(read(&local), "hello\n");
    assert_eq!(mode(&local), 0o600);
    assert_eq!(mode(local.parent().unwrap()), 0o700);
    step(0, "printf 'added\\n' >> \"$f\"");
    wait_until("the first save reaches the server", &|| read(&notes) == "hello\nadded\n");
    assert_eq!(mode(&notes), 0o640, "the mode is kept");
    client.wait_for_screen("saved to box");
    step(1, "printf 'more\\n' >> \"$f\"");
    wait_until("the second save reaches the server", &|| read(&notes) == "hello\nadded\nmore\n");
    quit(&local);
    let names: Vec<String> =
        std::fs::read_dir(&srv).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(names, vec!["notes 'q' $HOME;.conf".to_string()], "nothing else is left there");
    assert!(!env.root.join("srv/pwned").exists());

    // Changed on the server meanwhile: nothing is sent; kept as a local file, later saves go there.
    let file = srv.join("conflict.txt");
    std::fs::write(&file, "v1\n").unwrap();
    let local = open(&format!("box:{}", file.display()), &[]);
    std::fs::write(&file, "theirs\n").unwrap();
    client.screen.lock().unwrap().clear();
    step(0, "printf 'mine\\n' > \"$f\"");
    client.wait_for_screen("Keep your version as a local file");
    assert_eq!(read(&file), "theirs\n");
    client.send(b"\r");
    let kept = downloads.join("conflict.txt");
    wait_until("the copy is kept", &|| read(&kept) == "mine\n");
    assert_eq!(mode(&kept), 0o600);
    step(1, "printf 'mine2\\n' > \"$f\"");
    wait_until("the next save goes to the kept file", &|| read(&kept) == "mine2\n");
    quit(&local);
    assert_eq!(read(&file), "theirs\n", "the server's change stays");

    // Replaced, once asked.
    let file = srv.join("over.txt");
    std::fs::write(&file, "v1\n").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o604)).unwrap();
    let local = open(&format!("box:{}", file.display()), &[]);
    std::fs::write(&file, "theirs\n").unwrap();
    client.screen.lock().unwrap().clear();
    step(0, "printf 'mine\\n' > \"$f\"");
    client.wait_for_screen("Replace it with your version");
    // Typing does not answer it.
    client.send(b"replace");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(read(&file), "theirs\n");
    for key in [&b"\x1b[B"[..], b"\x1b[B", b"\r"] {
        client.send(key);
        std::thread::sleep(Duration::from_millis(100));
    }
    wait_until("the server's version is replaced", &|| read(&file) == "mine\n");
    assert_eq!(mode(&file), 0o604);
    quit(&local);

    // The server's version kept: the editor's copy becomes it, and nothing is sent.
    let file = srv.join("theirs.txt");
    std::fs::write(&file, "v1\n").unwrap();
    let local = open(&format!("box:{}", file.display()), &[]);
    std::fs::write(&file, "theirs\n").unwrap();
    client.screen.lock().unwrap().clear();
    step(0, "printf 'mine\\n' > \"$f\"");
    client.wait_for_screen("Keep the version of box");
    client.send(b"\x1b[B");
    std::thread::sleep(Duration::from_millis(100));
    client.send(b"\r");
    wait_until("the editor's copy is the server's version", &|| read(&local) == "theirs\n");
    quit(&local);
    assert_eq!(read(&file), "theirs\n");
    assert!(!downloads.join("theirs.txt").exists());

    // Production asks first: from the command line, --production says yes.
    let file = srv.join("prod.txt");
    std::fs::write(&file, "v1\n").unwrap();
    let target = format!("box-prod:{}", file.display());
    let refused = kraal(&["edit", &target]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("production"));
    let local = open(&target, &["--production", "--line", "1"]);
    step(0, "printf 'v2\\n' > \"$f\"");
    wait_until("a production file is saved once allowed", &|| read(&file) == "v2\n");
    quit(&local);

    // From the file manager: `e` on a server's file, asked first on a production server.
    let dir = env.root.join("fm");
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("fm.txt");
    std::fs::write(&file, "v1\n").unwrap();
    client.screen.lock().unwrap().clear();
    let out = kraal(&["files", &env.root.display().to_string(), &format!("box-prod:{}", dir.display())]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    client.wait_for_screen("fm.txt");
    for key in [&b"\t"[..], b"\x1b[B", b"e"] {
        client.send(key);
        std::thread::sleep(Duration::from_millis(150));
    }
    client.wait_for_screen("Careful");
    assert!(!env.root.join("editing").exists(), "nothing opens before the answer");
    client.send(b"y");
    let editing = env.root.join("editing");
    wait_until("the editor starts", &|| editing.exists());
    let local = PathBuf::from(read(&editing));
    step(0, "printf 'v2\\n' > \"$f\"");
    wait_until("a save from the file manager's editor", &|| read(&file) == "v2\n");
    quit(&local);

    // What cannot be edited is said.
    for (target, why) in [
        (format!("box:{}", srv.display()), "directory"),
        (format!("box:{}", env.root.join("nope").display()), "no such file"),
        (env.root.join("editor").display().to_string(), "not on a server"),
    ] {
        let out = kraal(&["edit", &target]);
        assert!(!out.status.success());
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(why), "{target}: {err}");
    }
    drop(sshd);
}

/// The parent of process `pid`.
#[cfg(target_os = "linux")]
fn parent_of(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?.1.split_whitespace().nth(1)?.parse().ok()
}

/// A pane that prints its pid, then `tick N` ten times a second.
#[cfg(target_os = "linux")]
const COUNTER: &str = "echo pid $$; i=0; while :; do i=$((i+1)); echo tick $i; sleep 0.1; done";

/// The pid a [`COUNTER`] pane printed.
#[cfg(target_os = "linux")]
fn counter_pid(env: &Env, pane: &str) -> i32 {
    let screen = env.wait_for_text(pane, "tick 2");
    screen.lines().find_map(|l| l.trim().strip_prefix("pid ")?.parse().ok()).expect("its pid")
}

/// Wait until a [`COUNTER`] pane has counted 3 more.
#[cfg(target_os = "linux")]
fn still_counting(env: &Env, pane: &str) {
    let last = |s: &str| s.lines().filter_map(|l| l.trim().strip_prefix("tick ")?.parse::<u64>().ok()).max();
    let now = last(&env.run(&["read", "-p", pane])).unwrap_or(0);
    env.wait_for_text(pane, &format!("tick {}", now + 3));
}

/// Hand-off files left in the state directory.
#[cfg(target_os = "linux")]
fn handoff_files(env: &Env) -> Vec<PathBuf> {
    let entries = std::fs::read_dir(env.root.join("state/kraal")).into_iter().flatten().flatten();
    entries
        .map(|e| e.path())
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("handoff-")))
        .collect()
}

#[cfg(target_os = "linux")]
#[test]
fn a_live_restart_keeps_programs_running_and_clients_attached() {
    let env = Env::new("live");
    std::fs::create_dir_all(env.root.join("state/kraal")).unwrap();
    std::fs::write(env.root.join("state/kraal/welcomed"), "").unwrap();
    // A hook says when a workspace opens.
    let opened = env.root.join("opened");
    let cfg = env.root.join("config/kraal/config.toml");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str(&format!("\n[hooks]\n\"workspace.opened\" = \"echo $KRAAL_WORKSPACE >> '{}'\"\n", opened.display()));
    std::fs::write(&cfg, text).unwrap();
    env.workspace("w", "layout {\n    pane name=\"sh\"\n}\n");
    env.run(&["open", "w"]);
    let start = Instant::now();
    while std::fs::read_to_string(&opened).unwrap_or_default() != "w\n" {
        assert!(start.elapsed() < Duration::from_secs(10), "the hook did not run");
        std::thread::sleep(Duration::from_millis(50));
    }
    let shown = env.root.join("client-screen");
    let mut client = env.client_showing(&["attach", "w"], &shown);
    // A full-screen program that draws itself again when its terminal changes size, and leaves
    // the alternate screen once told.
    let full = env.root.join("full");
    let leave = env.root.join("leave");
    write_script(
        &full,
        &format!(
            "#!/bin/sh\nn=0\ndraw() {{ n=$((n+1)); printf '\\033[H\\033[2Jfull screen, drawn %s times\\r\\n' $n; }}\n\
             echo before full screen\nprintf '\\033[?1049h'\ndraw\ntrap draw WINCH\n\
             while [ ! -e '{}' ]; do sleep 0.1; done\nprintf '\\033[?1049l'\necho back\nexec sleep 600\n",
            leave.display()
        ),
    );
    env.run(&["run", "--name", "counter", "--", "sh", "-c", COUNTER]);
    env.run(&["run", "--name", "full", "--", &full.display().to_string()]);
    env.run(&["run", "--name", "done", "--", "sh", "-c", "echo finished"]);
    let pid = counter_pid(&env, "counter");
    let server = parent_of(pid).expect("the server");
    // A narrow pane wraps the line.
    let drawn = || {
        let screen = env.run(&["read", "-p", "full"]);
        let joined: String = screen.lines().collect();
        let n = joined.split_once("drawn ").and_then(|(_, rest)| rest.split_once(" times")?.0.parse().ok());
        (n.unwrap_or(0u32), screen)
    };
    env.wait_for_text("full", "full screen, drawn");
    let start = Instant::now();
    while pane_json(&env, "done")["exit_code"] != 0 {
        assert!(start.elapsed() < Duration::from_secs(10), "{}", pane_json(&env, "done"));
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(300));
    let (times, _) = drawn();

    let out = env.run(&["restart", "--live"]);
    assert!(out.contains("restarted session live live"), "{out}");
    // The same server process, and the same program in the pane, counting on.
    assert_eq!(parent_of(pid), Some(server));
    still_counting(&env, "counter");
    // The full-screen program shows, and is made to draw itself again.
    let start = Instant::now();
    while drawn().0 <= times {
        assert!(start.elapsed() < Duration::from_secs(10), "not drawn again: {}", drawn().1);
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!drawn().1.contains("before full screen"), "{}", drawn().1);
    // What it covers comes back when it leaves the alternate screen.
    std::fs::write(&leave, "").unwrap();
    env.wait_for_text("full", "back");
    let all: String = env.run(&["read", "-p", "full", "--lines", "100"]).lines().collect();
    assert!(all.contains("before full screenback"), "{all}");
    // A finished command still shows how it ended.
    assert_eq!(pane_json(&env, "done")["exit_code"], 0);
    // The client came back by itself: what a pane prints now reaches its terminal.
    env.run(&["send", "-p", "sh", "--enter", "echo AFTER-$((40+2))"]);
    let start = Instant::now();
    while !String::from_utf8_lossy(&std::fs::read(&shown).unwrap_or_default()).contains("AFTER-42") {
        assert!(start.elapsed() < Duration::from_secs(10), "the client did not come back");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(client.try_wait().unwrap().is_none(), "the client is attached");
    assert!(handoff_files(&env).is_empty(), "{:?}", handoff_files(&env));
    // Nothing opened again.
    assert_eq!(std::fs::read_to_string(&opened).unwrap(), "w\n");
    let _ = client.kill();
    let _ = client.wait();
}

#[cfg(target_os = "linux")]
#[test]
fn a_refused_live_restart_leaves_the_server_as_it_was() {
    let env = Env::new("refused");
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    // The server runs a copy of kraal, replaced below as `kraal update` does it.
    let kraal = bin.join("kraal");
    let install = |from: &Path| {
        let staged = bin.join(".kraal.new");
        assert!(Command::new("cp").arg(from).arg(&staged).status().unwrap().success());
        std::fs::rename(&staged, &kraal).unwrap();
    };
    install(Path::new(env!("CARGO_BIN_EXE_kraal")));
    // The file manager's copies run rsync: this one copies nothing, and ends when told.
    let stop = env.root.join("stop");
    write_script(
        &bin.join("rsync"),
        &format!(
            "#!/bin/sh\n[ \"$1\" = --version ] && {{ echo 'rsync  version 3.2.7  protocol version 31'; exit 0; }}\n\
             while [ ! -e '{}' ]; do sleep 0.1; done\n",
            stop.display()
        ),
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    env.workspace("w", "layout {\n    pane name=\"sh\"\n}\n");
    let out = env.cmd_with(&kraal, &["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    env.run(&["run", "--name", "counter", "--", "sh", "-c", COUNTER]);
    let pid = counter_pid(&env, "counter");
    let server = parent_of(pid).expect("the server");
    let refused = |why: &str| {
        let out = env.try_run(&["restart", "--live"]);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && err.contains(why), "{why}: {err}");
        // The same server, the same program counting on, nothing left behind.
        assert_eq!(parent_of(pid), Some(server));
        still_counting(&env, "counter");
        assert!(handoff_files(&env).is_empty(), "{:?}", handoff_files(&env));
    };

    // A transfer runs.
    let (from, to) = (env.root.join("from"), env.root.join("to"));
    std::fs::create_dir_all(&from).unwrap();
    std::fs::create_dir_all(&to).unwrap();
    std::fs::write(from.join("payload.txt"), "data").unwrap();
    env.run(&["files", &from.display().to_string(), &to.display().to_string()]);
    let mut client = RawClient::attach_following(&env, "w");
    client.wait_for_screen("payload.txt");
    for key in [&b"\x1b[B"[..], b"c"] {
        client.send(key);
        std::thread::sleep(Duration::from_millis(150));
    }
    let copy = || {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        panes.as_array().unwrap().iter().find(|p| p["name"].as_str().is_some_and(|n| n.contains("copy"))).cloned()
    };
    let start = Instant::now();
    while copy().is_none() {
        assert!(start.elapsed() < Duration::from_secs(10), "the copy did not start");
        std::thread::sleep(Duration::from_millis(50));
    }
    refused("still running");
    std::fs::write(&stop, "").unwrap();
    let start = Instant::now();
    while copy().is_some_and(|c| c["exit_code"].is_null()) {
        assert!(start.elapsed() < Duration::from_secs(10), "the copy did not end");
        std::thread::sleep(Duration::from_millis(50));
    }

    // A `kraal wait` waits.
    let mut wait = env.cmd(&["wait", "-p", "sh", "--until", "exit", "--timeout", "60"]).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    refused("kraal wait");
    let _ = wait.kill();
    let _ = wait.wait();

    // The kraal installed where the server runs from cannot take over: the menu, which restarts
    // on that one (`kraal restart` restarts on its own), offers a restart that is not live.
    write_script(&env.root.join("not-kraal"), "#!/bin/sh\nexit 0\n");
    install(&env.root.join("not-kraal"));
    client.screen.lock().unwrap().clear();
    client.send(b"\x02Wr");
    client.wait_for_screen("No live restart now (one stops nothing): the kraal installed cannot take over");
    client.send(b"n");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(parent_of(pid), Some(server));
    still_counting(&env, "counter");

    // Once all that is over, from the kraal renamed over the one the server runs (Linux names
    // that one `<path> (deleted)`), from the menu, then from the command line.
    install(Path::new(env!("CARGO_BIN_EXE_kraal")));
    client.screen.lock().unwrap().clear();
    client.send(b"\x02Wr");
    client.wait_for_screen("Restart session refused live on the kraal installed now?");
    client.send(b"y");
    client.wait_for_message(r#""live":true"#);
    still_counting(&env, "counter");
    assert_eq!(parent_of(pid), Some(server));
    let out = env.run(&["restart", "--live"]);
    assert!(out.contains("restarted session refused live"), "{out}");
    assert_eq!(parent_of(pid), Some(server));
    still_counting(&env, "counter");
    assert!(handoff_files(&env).is_empty(), "{:?}", handoff_files(&env));
}

#[cfg(target_os = "linux")]
#[test]
fn tunnels_and_waiting_commands_are_carried_over_a_live_restart() {
    let env = Env::new("livetunnel");
    // A fake ssh that says it listens (as `ssh -v` does), then stays; its pids go in a file.
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let pids = env.root.join("ssh-pids");
    write_script(
        &bin.join("ssh"),
        &format!(
            "#!/bin/sh\nprev=; for a; do [ \"$prev\" = -L ] && fwd=$a; prev=$a; done\n\
             port=$(echo \"$fwd\" | cut -d: -f2)\necho $$ >> {}\n\
             echo \"debug1: Local forwarding listening on 127.0.0.1 port $port.\" >&2\n\
             echo \"debug1: channel 0: new port-listener [port listener] (inactive timeout: 0)\" >&2\n\
             exec sleep 600\n",
            pids.display()
        ),
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let run = |args: &[&str]| {
        let out = env.cmd(args).env("PATH", &path).output().unwrap();
        assert!(out.status.success(), "kraal {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    let port = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap().local_addr().unwrap().port();
    let tunnel_up =
        || run(&["tunnel", "list"]).lines().any(|l| l.starts_with("up") && l.ends_with(&format!("localhost:{port}")));
    let wait_up = || {
        let start = Instant::now();
        while !tunnel_up() {
            assert!(start.elapsed() < Duration::from_secs(10), "{}", run(&["tunnel", "list"]));
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let ssh_pids = || -> Vec<i32> {
        std::fs::read_to_string(&pids).unwrap_or_default().lines().filter_map(|l| l.trim().parse().ok()).collect()
    };
    env.workspace(
        "w",
        "layout {\n    pane split_direction=\"vertical\" {\n        pane name=\"main\"\n        \
         pane name=\"deploy\" command=\"sh\" { args \"-c\" \"echo DEPLOYED; exec sleep 1000\"; }\n    }\n}\n",
    );
    run(&["open", "w"]);
    env.wait_for_text("deploy", "DEPLOYED");
    run(&["tunnel", "add", "ok@a.lan", "5432", "--local", &port.to_string()]);
    wait_up();
    // After a restart, the command waits for Enter.
    run(&["kill"]);
    run(&["open", "w"]);
    wait_up();
    env.wait_for_text_since_restore("deploy", "restored session");
    let ssh = *ssh_pids().last().unwrap();

    let out = run(&["restart", "--live"]);
    assert!(out.contains("restarted"), "{out}");
    // The tunnel keeps its ssh: up, and no other one started.
    assert!(tunnel_up(), "{}", run(&["tunnel", "list"]));
    std::thread::sleep(Duration::from_millis(1500));
    assert!(tunnel_up(), "{}", run(&["tunnel", "list"]));
    assert_eq!(*ssh_pids().last().unwrap(), ssh);
    // SAFETY: signal 0 only checks that the process exists.
    let alive = |pid: i32| unsafe { libc::kill(pid, 0) } == 0;
    assert!(alive(ssh));
    // The command still waits for Enter, and Enter runs it.
    assert_eq!(pane_json(&env, "deploy")["suspended"], true);
    let mut client = RawClient::attach(&env, "w");
    run(&["focus", "deploy"]);
    std::thread::sleep(Duration::from_millis(300));
    client.send(b"\r");
    env.wait_for_text_since_restore("deploy", "DEPLOYED");
    // Closing the tunnel stops the ssh it kept.
    run(&["tunnel", "rm", &port.to_string()]);
    let start = Instant::now();
    while alive(ssh) {
        assert!(start.elapsed() < Duration::from_secs(5), "the tunnel's ssh still runs");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Whether process `pid` is a zombie: ended, and not reaped.
#[cfg(target_os = "linux")]
fn zombie(pid: i32) -> bool {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    stat.rsplit_once(')').is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
}

/// Add `text` to this test's config.
#[cfg(target_os = "linux")]
fn configure(env: &Env, text: &str) {
    let cfg = env.root.join("config/kraal/config.toml");
    let mut all = std::fs::read_to_string(&cfg).unwrap();
    all.push_str(text);
    std::fs::write(&cfg, all).unwrap();
}

/// A layout nested deeper than JSON is read by default (a split in a split, 40 times) restarts
/// live, and comes back whole after a restart that is not live.
#[cfg(target_os = "linux")]
#[test]
fn a_deeply_nested_layout_restarts_live_and_cold() {
    let env = Env::new("deep");
    env.workspace("w", "layout {\n    pane name=\"p0\"\n}\n");
    env.run(&["open", "w"]);
    env.run(&["run", "--name", "counter", "--", "sh", "-c", COUNTER]);
    let pid = counter_pid(&env, "counter");
    let server = parent_of(pid).expect("the server");
    let mut last = "counter".to_string();
    for i in 1..=40 {
        let way = if i % 2 == 1 { "--right" } else { "--down" };
        let name = format!("p{i}");
        env.run(&["run", way, "-t", &last, "--name", &name, "--", "sleep", "600"]);
        last = name;
    }
    let count =
        || serde_json::from_str::<serde_json::Value>(&env.run(&["panes", "--json"])).unwrap().as_array().unwrap().len();
    assert_eq!(count(), 42);
    let out = env.run(&["restart", "--live"]);
    assert!(out.contains("restarted session deep live"), "{out}");
    assert_eq!(parent_of(pid), Some(server));
    still_counting(&env, "counter");
    assert_eq!(count(), 42);
    env.run(&["restart", "--cold", "--yes"]);
    assert_eq!(count(), 42, "the layout came back");
}

/// The binary a live restart runs is the very file it tried, even when another one (here, one
/// that could not take over) is renamed over its path meanwhile.
#[cfg(target_os = "linux")]
#[test]
fn a_live_restart_runs_the_binary_it_tried() {
    let env = Env::new("tried");
    // A hook keeps the restart waiting a moment after it has tried the binary.
    configure(&env, "\n[hooks]\nnotification = \"sleep 3\"\n");
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let kraal = bin.join("kraal");
    assert!(Command::new("cp").arg(env!("CARGO_BIN_EXE_kraal")).arg(&kraal).status().unwrap().success());
    env.workspace("w", "layout {\n    pane name=\"sh\"\n}\n");
    env.run(&["open", "w"]);
    env.run(&["run", "--name", "counter", "--", "sh", "-c", COUNTER]);
    let pid = counter_pid(&env, "counter");
    let server = parent_of(pid).expect("the server");
    env.run(&["notify", "a hook runs"]);
    // Run from that copy, `kraal restart` restarts on it.
    let restart = env.cmd_with(&kraal, &["restart", "--live"]).stdout(std::process::Stdio::piped()).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(800));
    write_script(&bin.join("old"), "#!/bin/sh\nexit 1\n");
    std::fs::rename(bin.join("old"), &kraal).unwrap();
    let out = restart.wait_with_output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success() && said.contains("restarted session tried live"), "{said}");
    assert_eq!(parent_of(pid), Some(server));
    still_counting(&env, "counter");
}

/// Input a program has not read yet would be lost in the exec: the restart waits for it, then
/// is refused, and the program keeps it.
#[cfg(target_os = "linux")]
#[test]
fn input_a_program_has_not_read_holds_a_live_restart_back() {
    let env = Env::new("unread");
    env.workspace("w", "layout {\n    pane name=\"sh\"\n}\n");
    env.run(&["open", "w"]);
    env.run(&["run", "--name", "counter", "--", "sh", "-c", COUNTER]);
    let pid = counter_pid(&env, "counter");
    let server = parent_of(pid).expect("the server");
    env.run(&["run", "--name", "deaf", "--", "sh", "-c", "stty raw -echo; exec sleep 600"]);
    let chunk = "x".repeat(60_000);
    for _ in 0..4 {
        env.run(&["send", "-p", "deaf", &chunk]);
    }
    let out = env.try_run(&["restart", "--live"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("has input it has not read yet"), "{err}");
    assert_eq!(parent_of(pid), Some(server));
    still_counting(&env, "counter");
}

/// A command one of the server's threads runs (git, for the panes' branches) is waited for, and
/// none starts until the exec: none is left behind as a zombie nothing reaps.
#[cfg(target_os = "linux")]
#[test]
fn commands_the_server_runs_are_not_left_behind() {
    let env = Env::new("helpers");
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let (pids, running) = (env.root.join("git-pids"), env.root.join("git-running"));
    write_script(
        &bin.join("git"),
        &format!(
            "#!/bin/sh\necho $$ >> '{}'\ntouch '{}'\nsleep 1.5\nrm -f '{}'\n",
            pids.display(),
            running.display(),
            running.display()
        ),
    );
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    env.workspace("w", "layout {\n    pane name=\"sh\"\n}\n");
    let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let start = Instant::now();
    while !running.exists() {
        assert!(start.elapsed() < Duration::from_secs(15), "git never ran");
        std::thread::sleep(Duration::from_millis(10));
    }
    let out = env.run(&["restart", "--live"]);
    assert!(out.contains("restarted session helpers live"), "{out}");
    std::thread::sleep(Duration::from_millis(2500));
    let ran: Vec<i32> = std::fs::read_to_string(&pids).unwrap().lines().filter_map(|l| l.trim().parse().ok()).collect();
    let left: Vec<i32> = ran.iter().copied().filter(|p| zombie(*p)).collect();
    std::thread::sleep(Duration::from_millis(300));
    let left: Vec<i32> = left.into_iter().filter(|p| zombie(*p)).collect();
    assert!(left.is_empty(), "left behind: {left:?}");
}

/// A command connected when a live restart starts, which sends its request a moment later, is
/// answered.
#[cfg(target_os = "linux")]
#[test]
fn a_command_sent_while_a_live_restart_starts_is_answered() {
    use std::io::Read;
    let env = Env::new("asked");
    env.workspace("w", "layout {\n    pane name=\"sh\"\n}\n");
    env.run(&["open", "w"]);
    let socket = env.root.join("run/kraal/asked.sock");
    let mut stream = std::os::unix::net::UnixStream::connect(&socket).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let restart = env.cmd(&["restart", "--live"]).stdout(std::process::Stdio::piped()).spawn().unwrap();
    std::thread::sleep(Duration::from_millis(600));
    raw_frame(&mut stream, 0, br#"{"type":"request","version":2,"request":{"cmd":"ping"}}"#);
    stream.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    let mut header = [0u8; 5];
    stream.read_exact(&mut header).expect("an answer");
    let mut payload = vec![0u8; u32::from_le_bytes(header[..4].try_into().unwrap()) as usize];
    stream.read_exact(&mut payload).unwrap();
    assert!(String::from_utf8_lossy(&payload).contains("pong"), "{}", String::from_utf8_lossy(&payload));
    let out = restart.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
}

/// A program that has written half an escape sequence when a live restart starts (a query whose
/// end comes later) gets its answer: the restart waits until the sequence is whole.
#[cfg(target_os = "linux")]
#[test]
fn a_sequence_cut_in_two_is_never_handed_over() {
    let env = Env::new("halfway");
    env.workspace("w", "layout {\n    pane name=\"sh\"\n}\n");
    env.run(&["open", "w"]);
    let started = env.root.join("started");
    let query = env.root.join("query");
    write_script(
        &query,
        &format!(
            "#!/bin/sh\nstty raw -echo\nprintf '\\033['\ntouch '{}'\nsleep 2\nprintf '6n'\n\
             r=$(timeout --foreground 5 dd bs=1 count=6 2>/dev/null)\nstty sane\n\
             if [ -n \"$r\" ]; then echo GOT-REPLY; else echo NO-REPLY; fi\nexec sleep 600\n",
            started.display()
        ),
    );
    env.run(&["run", "--name", "query", "--", &query.display().to_string()]);
    let start = Instant::now();
    while !started.exists() {
        assert!(start.elapsed() < Duration::from_secs(10), "the program did not start");
        std::thread::sleep(Duration::from_millis(10));
    }
    let out = env.run(&["restart", "--live"]);
    assert!(out.contains("restarted session halfway live"), "{out}");
    let screen = env.wait_for_text("query", "REPLY");
    assert!(screen.contains("GOT-REPLY"), "{screen}");
}

/// A client from before live restarts (one that does not say it follows them) is told to attach
/// again, rather than to wait for a server that does not go away; one that follows comes back.
#[cfg(target_os = "linux")]
#[test]
fn clients_that_do_not_follow_a_live_restart_are_told_to_attach_again() {
    let env = Env::new("olderclient");
    env.workspace("w", "layout {\n    pane name=\"sh\"\n}\n");
    env.run(&["open", "w"]);
    let older = RawClient::attach(&env, "w");
    let following = RawClient::attach_following(&env, "w");
    older.wait_for_message("\"attached\"");
    following.wait_for_message("\"attached\"");
    let out = env.run(&["restart", "--live"]);
    assert!(out.contains("restarted session olderclient live"), "{out}");
    older.wait_for_message("kraal restarted live: run kraal to attach again");
    assert!(!older.messages.lock().unwrap().iter().any(|m| m.contains("reattach")), "{:?}", older.messages);
    following.wait_for_message(r#""reattach":"w","live":true"#);
    following.wait_for_message(r#""type":"hold","on":true"#);
}

/// The menu's restart that is not live runs the kraal installed now, which may be one from
/// before live restarts: it knows no `--cold`, and its restart happens all the same.
#[cfg(target_os = "linux")]
#[test]
fn the_menu_restarts_with_a_kraal_from_before_live_restarts() {
    let env = Env::new("prelive");
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let kraal = bin.join("kraal");
    assert!(Command::new("cp").arg(env!("CARGO_BIN_EXE_kraal")).arg(&kraal).status().unwrap().success());
    env.workspace("w", "layout {\n    pane name=\"sh\"\n}\n");
    let out = env.cmd_with(&kraal, &["open", "w"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    // Installed now: a kraal that knows neither `--cold` nor live restarts.
    write_script(
        &bin.join("old"),
        &format!(
            "#!/bin/sh\nfor a; do case $a in --cold|--handoff-check) echo \"error: unexpected argument '$a' found\" >&2; exit 2;; esac; done\n\
             exec '{}' \"$@\"\n",
            env!("CARGO_BIN_EXE_kraal")
        ),
    );
    std::fs::rename(bin.join("old"), &kraal).unwrap();
    let mut client = RawClient::attach_following(&env, "w");
    client.wait_for_message("\"attached\"");
    client.send(b"\x02Wr");
    client.wait_for_screen("No live restart now (one stops nothing): the kraal installed cannot take over");
    client.send(b"y");
    client.wait_for_message(r#""type":"exit""#);
}

/// A closed pane whose program ignored the hangup is still reaped once it ends, after a live
/// restart too: no zombie is left.
#[cfg(target_os = "linux")]
#[test]
fn a_closed_panes_program_is_reaped_after_a_live_restart() {
    let env = Env::new("orphan");
    env.workspace("w", "layout {\n    pane name=\"sh\"\n}\n");
    env.run(&["open", "w"]);
    let (pidfile, stop) = (env.root.join("pid"), env.root.join("stop"));
    let script = format!(
        "trap '' HUP; echo $$ > '{}'; while [ ! -e '{}' ]; do sleep 0.1; done",
        pidfile.display(),
        stop.display()
    );
    env.run(&["run", "--name", "stubborn", "--", "sh", "-c", &script]);
    let start = Instant::now();
    let pid: i32 = loop {
        if let Some(p) = std::fs::read_to_string(&pidfile).ok().and_then(|s| s.trim().parse().ok()) {
            break p;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "it did not start");
        std::thread::sleep(Duration::from_millis(20));
    };
    env.run(&["close", "stubborn"]);
    std::thread::sleep(Duration::from_millis(500));
    // SAFETY: signal 0 only checks that the process exists.
    assert!(unsafe { libc::kill(pid, 0) } == 0 && !zombie(pid), "it ignores the hangup");
    let out = env.run(&["restart", "--live"]);
    assert!(out.contains("restarted session orphan live"), "{out}");
    std::fs::write(&stop, "").unwrap();
    let start = Instant::now();
    // SAFETY: as above.
    while unsafe { libc::kill(pid, 0) } == 0 {
        assert!(start.elapsed() < Duration::from_secs(10), "left as a zombie: {}", zombie(pid));
        std::thread::sleep(Duration::from_millis(50));
    }
}
