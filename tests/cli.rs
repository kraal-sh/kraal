//! End-to-end tests: a real server driven through the CLI, in isolated directories.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use common::{Env, claude_hook, git_repo, wait_until, write_script};

/// How long a test waits for what it expects to show before it fails. Far longer than it takes:
/// a machine busy with other work (a CI runner, the suite run beside other suites) is slower by
/// tens of seconds where programs are started and talk to each other, and a wait that ends when
/// its condition holds costs nothing when it does.
const PATIENCE: Duration = Duration::from_secs(60);

impl Env {
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
            let out = self.try_run(&["read", "-p", pane]);
            let screen = if out.status.success() {
                String::from_utf8(out.stdout).unwrap()
            } else {
                // A pane that a command has just asked for may not exist yet.
                let err = String::from_utf8_lossy(&out.stderr);
                assert!(err.contains("no pane named"), "kraal read -p {pane:?} failed: {err}");
                String::new()
            };
            let seen = part(&screen);
            // A narrow pane wraps long lines: the text may run over two rows.
            if seen.contains(text) || seen.lines().collect::<String>().contains(text) {
                return seen.to_string();
            }
            assert!(start.elapsed() < PATIENCE, "timed out waiting for {text:?} in:\n{screen}");
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
        let attach = self.cmd(&["attach"]);
        let line = std::iter::once(attach.get_program())
            .chain(attach.get_args())
            .map(|a| format!("'{}'", a.to_string_lossy()))
            .collect::<Vec<_>>()
            .join(" ");
        let mut c = Command::new("script");
        // The terminal `script` makes has no size until it is given one.
        c.args(["-q", "-f", "-c", &format!("stty cols 100 rows 30; exec {line}"), "/dev/null"]);
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
        use std::io::Read;
        let socket = env.root.join("run/kraal").join(format!("{}.sock", env.session));
        let mut stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
        let installed = installed.map(|v| format!(r#","installed":"{v}""#)).unwrap_or_default();
        let hello = format!(
            r#"{{"type":"attach","version":2,"cols":120,"rows":40,"workspace":"{workspace}","remote":false{installed}}}"#
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
            assert!(start.elapsed() < PATIENCE, "{text:?} never sent: {:?}", self.messages.lock().unwrap());
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
            assert!(start.elapsed() < PATIENCE, "{text:?} never shown");
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
        assert!(start.elapsed() < PATIENCE, "never seen as production: {pane}");
        std::thread::sleep(Duration::from_millis(100));
    }
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

/// How `child` ended and what it printed, once it has: still running after `limit`, it is
/// killed and the test fails.
fn finish(mut child: std::process::Child, limit: Duration) -> Output {
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if start.elapsed() > limit {
            let _ = child.kill();
            let _ = child.wait();
            panic!("still running after {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

/// A process this test stopped (SIGSTOP), continued when the test ends, however it ends.
struct Stopped(libc::pid_t);

impl Stopped {
    fn stop(pid: libc::pid_t) -> Stopped {
        // SAFETY: plain syscall to a process this test started.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGSTOP) }, 0);
        Stopped(pid)
    }
}

impl Drop for Stopped {
    fn drop(&mut self) {
        // SAFETY: plain syscall to a process this test started.
        unsafe { libc::kill(self.0, libc::SIGCONT) };
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

/// `kraal save` run in a pane saves that pane's workspace, as every command run in a pane acts
/// where it runs: not the workspace a terminal shows, nor the first one. `--workspace` names
/// another, and outside a pane the one on screen is saved.
#[test]
fn save_in_a_pane_saves_the_workspace_of_that_pane() {
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

    assert!(in_pane(&["save"]).trim().ends_with("/v.kdl"));
    // Saving over a definition keeps the one before beside it.
    assert_eq!(names_in(&dir), ["v.kdl", "v.kdl.bak", "w.kdl"]);
    assert!(in_pane(&["save", "copy"]).trim().ends_with("/copy.kdl"));
    assert!(saved("copy.kdl").contains("name=\"second\"") && !saved("copy.kdl").contains("first"));
    assert!(in_pane(&["save", "named", "--workspace", "w"]).trim().ends_with("/named.kdl"));
    assert!(saved("named.kdl").contains("name=\"first\""), "{}", saved("named.kdl"));
    // Outside a pane: the workspace on screen.
    assert!(env.run(&["save"]).trim().ends_with("/w.kdl"));
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

/// The rows naming the definitions the sidebar lists as not running, once the screen settles.
fn not_running(client: &RawClient) -> Vec<String> {
    client.settle();
    client.rows().windows(2).filter(|w| w[1].contains("not running")).map(|w| w[0].clone()).collect()
}

/// The case reported: a workspace opened from `perso.kdl` and renamed with a prefix in the menu
/// takes its file along, and is listed once, in the sidebar and by `kraal workspaces`, after a
/// restart too. Saving its layout writes that file.
#[test]
fn a_renamed_workspace_takes_its_file_along() {
    let env = Env::new("wsrename");
    let dir = env.root.join("config/kraal/workspaces");
    env.workspace("perso", "layout {\n    pane name=\"shell\"\n}\n");
    env.workspace("work", "layout {\n    pane name=\"other\"\n}\n");
    env.run(&["open", "perso"]);
    let mut client = RawClient::attach(&env, "perso");
    client.wait_for_message("\"attached\"");
    client.wait_for_row("not running");
    client.send(b"\x02We");
    client.wait_for_row("Rename workspace");
    // Home, then the prefix.
    client.send(b"\x1b[Hinfra - \r");
    client.wait_for_row("renamed perso.kdl to infra - perso.kdl");
    assert_eq!(names_in(&dir), ["infra - perso.kdl", "work.kdl"]);
    let shown = not_running(&client);
    assert!(shown.len() == 1 && shown[0].contains("work"), "{shown:?}");
    assert_eq!(listed_workspaces(&env), ["infra - perso: infra - perso.kdl (open)", "work: work.kdl"]);
    let text = env.run(&["workspaces"]);
    assert!(!text.lines().any(|l| l.starts_with("perso ")), "{text}");
    // Saved from the menu under the name it proposes: to its file.
    client.send(b"\x02Ws");
    client.wait_for_row("Save workspace layout as");
    client.send(b"\r");
    client.wait_for_row("saved ");
    assert_eq!(names_in(&dir), ["infra - perso.kdl", "infra - perso.kdl.bak", "work.kdl"]);
    // Restored after a restart, still linked to its file.
    env.run(&["kill"]);
    env.run(&["open", "infra - perso"]);
    assert_eq!(listed_workspaces(&env), ["infra - perso: infra - perso.kdl (open)", "work: work.kdl"]);
    assert_eq!(env.run(&["panes"]).lines().count(), 1);
}

/// A rename never replaces a file: a name whose file exists, or that cannot name a file, is
/// refused with why, and nothing changes. A workspace without a file takes any name.
#[test]
fn a_workspace_rename_never_replaces_a_file() {
    let env = Env::new("wsrenameexists");
    let dir = env.root.join("config/kraal/workspaces");
    env.workspace("a", "layout {\n    pane name=\"one\"\n}\n");
    env.workspace("b", "layout {\n    pane name=\"two\"\n}\n");
    env.run(&["open", "a"]);
    let mut client = RawClient::attach(&env, "a");
    client.wait_for_message("\"attached\"");
    client.send(b"\x02We");
    client.wait_for_row("Rename workspace");
    client.send(b"\x7fb\r");
    client.wait_for_row("b.kdl exists: nothing was renamed.");
    client.send(b"\x7fx/y\r");
    client.wait_for_row("a workspace file name is one plain name");
    client.send(b"\x1b");
    client.wait_for_rows("the prompt closed", |rows| !rows.iter().any(|r| r.contains("Rename workspace")));
    assert_eq!(names_in(&dir), ["a.kdl", "b.kdl"]);
    assert_eq!(std::fs::read_to_string(dir.join("b.kdl")).unwrap(), "layout {\n    pane name=\"two\"\n}\n");
    client.send(b"\x02Wn");
    client.wait_for_row("New workspace");
    client.send(b"scratch\r");
    client.send(b"\x02We");
    client.wait_for_row("Rename workspace");
    client.send(b"/x\r");
    wait_until(Duration::from_secs(10), "never renamed", || {
        listed_workspaces(&env) == ["a: a.kdl (open)", "scratch/x:  (open)", "b: b.kdl"]
    });
    assert_eq!(names_in(&dir), ["a.kdl", "b.kdl"]);
}

/// A session where a workspace was renamed without its file, by an older kraal, keeps it linked
/// to that file: listed once, opened by the file's name, saved to it. Renamed again in the menu,
/// the file follows.
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
    let shown = not_running(&client);
    assert!(shown.len() == 1 && shown[0].contains("work"), "{shown:?}");
    let saved = env.run(&["save", "--workspace", "infra - perso"]);
    assert!(saved.trim_end().ends_with("/perso.kdl"), "{saved}");
    assert_eq!(names_in(&dir), ["perso.kdl", "perso.kdl.bak", "work.kdl"]);
    client.send(b"\x02We");
    client.wait_for_row("Rename workspace");
    client.send(b"\r");
    client.wait_for_row("renamed perso.kdl to infra - perso.kdl");
    assert_eq!(names_in(&dir), ["infra - perso.kdl", "perso.kdl.bak", "work.kdl"]);
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
    env.run(&["save", "--workspace", "perso"]);
    assert_eq!(names_in(&dir), ["moved.kdl"]);
}

/// Panes stacked from the keys share one place and keep one size; the one the focus reaches
/// opens (a key, a click on its row); a pane comes out again; one key goes through the layouts;
/// stacks and the layout chosen survive a restart, and a saved workspace keeps its stacks.
#[test]
fn stacks_and_layouts_from_the_keys() {
    let env = Env::new("stacks");
    let config = env.root.join("config/kraal/config.toml");
    let text = std::fs::read_to_string(&config).unwrap();
    std::fs::write(&config, format!("{text}[sidebar]\nvisible = false\n")).unwrap();
    env.workspace(
        "w",
        "layout {\n    pane split_direction=\"vertical\" {\n        pane name=\"alpha\"\n        pane name=\"bravo\"\n        pane name=\"charlie\"\n    }\n}\n",
    );
    env.run(&["open", "w"]);
    let panes = || -> Vec<serde_json::Value> {
        let v: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        v.as_array().unwrap().iter().filter(|p| p["workspace"] == "w").cloned().collect()
    };
    let size = |name: &str| -> (u64, u64) {
        let p = panes().into_iter().find(|p| p["name"] == name).unwrap();
        (p["cols"].as_u64().unwrap(), p["rows"].as_u64().unwrap())
    };
    let focused = || -> String {
        let p = panes().into_iter().find(|p| p["focused"] == true);
        p.and_then(|p| p["name"].as_str().map(String::from)).unwrap_or_default()
    };
    let until = |what: &str, done: &dyn Fn() -> bool| {
        let start = Instant::now();
        while !done() {
            assert!(start.elapsed() < Duration::from_secs(10), "{what}: {}", env.run(&["panes", "--json"]));
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_message("\"attached\"");
    // 120×40: the panes take rows 1 to 38, between the bars, a third of the width each.
    until("three columns", &|| size("alpha") == (37, 36));
    env.run(&["focus", "bravo"]);

    // Bravo goes right, onto charlie: bravo open in two thirds of the width, charlie folded to
    // the row under it, of the same size.
    client.send(b"\x02Ps\x1b[C");
    until("stacked", &|| size("bravo") == (78, 35) && size("charlie") == (78, 35));
    assert_eq!(focused(), "bravo");
    // Down goes to the next pane of the stack, which opens.
    client.send(b"\x1b[1;3B");
    until("charlie focused", &|| focused() == "charlie");
    // Bravo is folded to the top row of the stack now: a click on it opens it.
    client.send(b"\x1b[<0;61;2M\x1b[<0;61;2m");
    until("bravo clicked", &|| focused() == "bravo");
    assert_eq!(size("charlie"), (78, 35), "opening a pane resizes nothing");
    // Out of the stack again: above charlie, each half of the height, frame on frame.
    client.send(b"\x02Pu");
    until("unstacked", &|| size("bravo") == (78, 17) && size("charlie") == (78, 17));
    assert_eq!(focused(), "bravo");

    // The layouts, in turn: even columns, even rows, the main pane left, the others stacked.
    client.send(b"\x02Pl");
    until("columns", &|| size("alpha") == (37, 36) && size("charlie") == (38, 36));
    client.send(b"\x02Pl");
    until("rows", &|| size("alpha").0 == 118 && size("charlie").0 == 118);
    client.send(b"\x02Pl");
    until("main left", &|| size("alpha") == (69, 36) && size("bravo").0 == 46);
    client.send(b"\x02Pl");
    until("main left, stacked", &|| size("bravo") == (46, 35) && size("charlie") == (46, 35));
    assert_eq!(focused(), "bravo");

    // A restart brings the stack and the layout back.
    drop(client);
    env.run(&["kill"]);
    let state = std::fs::read_to_string(env.state_file()).unwrap();
    assert!(state.contains("\"stacked\": true") && state.contains("\"preset\": \"main-left-stacked\""), "{state}");
    env.run(&["open", "w"]);
    until("restored", &|| panes().len() == 3);
    until("stacked again", &|| size("bravo") == (46, 35) && size("charlie") == (46, 35) && size("alpha") == (69, 36));
    // The layout is still the tab's: a pane that comes joins the stack.
    env.run(&["run", "-t", "alpha", "--name", "delta", "--", "sh"]);
    until("delta stacked", &|| panes().len() == 4 && size("delta") == (46, 34) && size("bravo") == (46, 34));

    // Saved, the stack is `stacked=true`, with the pane open `expanded=true`, and opens as one.
    env.run(&["save", "copy", "--workspace", "w"]);
    let saved = std::fs::read_to_string(env.root.join("config/kraal/workspaces/copy.kdl")).unwrap();
    assert!(saved.contains("pane stacked=true") && saved.contains("expanded=true"), "{saved}");
    env.run(&["open", "copy"]);
    let copies = || -> Vec<(u64, u64)> {
        let v: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let mut sizes: Vec<(u64, u64)> = v
            .as_array()
            .unwrap()
            .iter()
            .filter(|p| p["workspace"] == "copy")
            .map(|p| (p["cols"].as_u64().unwrap(), p["rows"].as_u64().unwrap()))
            .collect();
        sizes.sort();
        sizes
    };
    until("the copy opens stacked", &|| copies() == [(46, 34), (46, 34), (46, 34), (69, 36)]);
}

#[test]
fn a_stack_opens_on_its_expanded_pane_without_a_focus_given() {
    let env = Env::new("expanded");
    let stack = "pane stacked=true {\n        pane name=\"a\"\n        pane name=\"b\" expanded=true\n    }";
    env.workspace("w", &format!("layout {{\n    {stack}\n}}\n"));
    env.run(&["open", "w"]);
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let focused: Vec<&str> =
        panes.as_array().unwrap().iter().filter(|p| p["focused"] == true).filter_map(|p| p["name"].as_str()).collect();
    assert_eq!(focused, ["b"]);
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

/// A session has one server: another one started for it while it runs refuses to, even when
/// the first one's socket is gone (a cleaner of /tmp may remove it), so that two servers never
/// write the same session.
#[test]
fn a_session_never_has_two_servers() {
    let env = Env::new("oneserver");
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    env.run(&["open", "w"]);
    let first = env.server_pid().expect("the server runs");
    let socket = env.root.join("run/kraal/oneserver.sock");
    let aside = env.root.join("run/kraal/aside.sock");
    std::fs::rename(&socket, &aside).unwrap();
    let second = env.cmd(&["server"]).stderr(std::process::Stdio::piped()).spawn().unwrap();
    let out = finish(second, Duration::from_secs(10));
    std::fs::rename(&aside, &socket).unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("holds session \"oneserver\""), "{err}");
    assert_eq!(env.server_pid(), Some(first));
    env.run(&["panes"]);
}

/// `kraal forget` deletes nothing of a session whose server runs, even one whose socket is gone:
/// a server holds its session, and `forget` must hold it too.
#[test]
fn forget_leaves_a_running_session_alone() {
    let env = Env::new("forgetheld");
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    env.run(&["open", "w"]);
    wait_until(Duration::from_secs(10), "the layout was never saved", || env.state_file().exists());
    let socket = env.root.join("run/kraal/forgetheld.sock");
    let aside = env.root.join("run/kraal/aside.sock");
    std::fs::rename(&socket, &aside).unwrap();
    let out = env.try_run(&["forget"]);
    std::fs::rename(&aside, &socket).unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("is running"), "{err}");
    assert!(env.state_file().exists(), "the layout was deleted");
}

/// A workspace that cannot be restored (its shell is gone) stays saved as it was, scrollback
/// included, while the server saves the others; it comes back once it can.
#[test]
fn a_workspace_that_cannot_be_restored_stays_saved() {
    let env = Env::new("unrestored");
    env.workspace("shells", "layout {\n    pane name=\"shell\"\n}\n");
    env.workspace("jobs", "layout {\n    pane name=\"job\" command=\"sh\" { args \"-c\" \"exec sleep 4246\"; }\n}\n");
    env.run(&["open", "shells"]);
    env.run(&["open", "jobs"]);
    env.run(&["send", "-p", "shell", "--enter", "echo kept-$((4*4))"]);
    env.wait_for_text("shell", "kept-16");
    env.run(&["kill"]);
    let names = || -> Vec<String> {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        panes.as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap_or_default().to_string()).collect()
    };
    // Without its shell, that workspace cannot come back; the other one does, and is saved.
    let shell = env.root.join("sh");
    let away = env.root.join("sh.away");
    std::fs::rename(&shell, &away).unwrap();
    env.run(&["open", "jobs"]);
    assert_eq!(names(), ["job"]);
    env.run(&["kill"]);
    let state = std::fs::read_to_string(env.state_file()).unwrap();
    assert!(state.contains("\"shells\""), "the workspace was dropped: {state}");
    std::fs::rename(&away, &shell).unwrap();
    env.run(&["open", "jobs"]);
    let mut back = names();
    back.sort();
    assert_eq!(back, ["job", "shell"]);
    env.wait_for_text("shell", "kept-16");
}

/// A restart stops the session's programs only once the layout it brings back is saved: when it
/// cannot be, the session keeps running as it is, and the restart says why.
#[test]
fn a_restart_that_cannot_save_the_session_leaves_it_running() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new("unsavedrestart");
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    env.run(&["open", "w"]);
    wait_until(Duration::from_secs(10), "the layout was never saved", || env.state_file().exists());
    let pid = env.server_pid().expect("the server runs");
    let sessions = env.root.join("state/kraal/sessions");
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o500)).unwrap();
    // A pane opened since the last save: the layout on disk is not the session's any more.
    env.run(&["run", "--name", "two", "--target", "one", "--", "sh"]);
    let out = env.try_run(&["restart", "--yes"]);
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o700)).unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("not restarting: the layout could not be saved"), "{err}");
    assert_eq!(env.server_pid(), Some(pid), "the session stopped");
    // Once it can be saved, the restart brings back the pane opened since.
    let said = env.run(&["restart", "--yes"]);
    assert!(said.contains("restarted session unsavedrestart"), "{said}");
    assert_ne!(env.server_pid(), Some(pid));
    assert!(env.run(&["panes"]).contains("two"));
}

/// `kraal kill` and `kraal restart` give up on a server that does not answer (stopped, here) in
/// a bounded time, instead of waiting for it for good.
#[test]
fn stopping_a_server_that_does_not_answer_gives_up_in_time() {
    let env = Env::new("frozen");
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    env.run(&["open", "w"]);
    let stopped = Stopped::stop(env.server_pid().expect("the server runs"));
    let quiet = |args: &[&str]| {
        let mut c = env.cmd(args);
        c.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped());
        c.spawn().unwrap()
    };
    let (kill, restart) = (quiet(&["kill"]), quiet(&["restart", "--yes"]));
    let outs = [finish(kill, Duration::from_secs(60)), finish(restart, Duration::from_secs(60))];
    drop(stopped);
    for out in outs {
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && err.contains("is still running after"), "{err}");
    }
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

/// A name given where a workspace's goes that names no workspace but reads as a server
/// (`user@host`, or a server kraal knows) is answered with the command that was meant, and
/// kraal attaches nowhere; any other name gets the plain answer. No session is started only to
/// say that a name is unknown.
#[test]
fn a_server_given_where_a_workspace_goes_is_told_how_to_attach_to_it() {
    let env = Env::new("hostname");
    std::fs::write(env.root.join("config/kraal/servers.toml"), "[[server]]\nname = \"db\"\nhost = \"db.lan\"\n")
        .unwrap();
    let said = |name: &str| {
        let out = env.try_run(&["open", name]);
        assert!(!out.status.success(), "{name}");
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    let meant = |name: &str| format!("; to attach to kraal on that server: kraal attach --host {name}");
    let open_says = |when: &str| {
        for name in ["fpa@anvil", "db"] {
            let said = said(name);
            assert!(said.contains(&format!("no workspace named {name:?} in ")), "{when}: {said}");
            assert!(said.trim_end().ends_with(&meant(name)), "{when}: {said}");
        }
        let plain = said("nothing");
        assert!(plain.contains("no workspace named \"nothing\" in ") && !plain.contains("--host"), "{when}: {plain}");
    };

    open_says("without a session");
    assert_eq!(env.server_pid(), None, "a session was started only to say that a name is unknown");

    // A session running: its server answers the same, and a terminal is not attached to it.
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    open_says("with a session");
    let refused = RawClient::attach(&env, "fpa@anvil");
    refused.wait_for_message(&meant("fpa@anvil"));
    assert!(!refused.messages.lock().unwrap().iter().any(|m| m.contains("\"attached\"")));
    // Any other unknown name attaches all the same, with the plain message.
    let client = RawClient::attach(&env, "nothing");
    client.wait_for_message("\"attached\"");
    let rows = client.wait_for_row("no workspace named \"nothing\"");
    assert!(!rows.iter().any(|r| r.contains("--host")), "{rows:?}");
    // A workspace named like a server is a workspace.
    env.workspace("db", "layout {\n    pane name=\"data\"\n}\n");
    env.run(&["open", "db"]);
    let named = RawClient::attach(&env, "db");
    named.wait_for_message("\"attached\"");
    named.wait_for_row("data");
}

/// `kraal attach fpa@anvil` and `kraal fpa@anvil`, typed for `kraal attach --host fpa@anvil`:
/// the terminal gets the answer and is not attached to the local session, running or not.
#[cfg(target_os = "linux")]
#[test]
fn attaching_to_a_server_given_as_a_workspace_says_what_was_meant() {
    let env = Env::new("hostattach");
    let forms: [&[&str]; 2] = [&["attach", "fpa@anvil"], &["fpa@anvil"]];
    let check = |when: &str| {
        for args in forms {
            let shown = in_terminal(env.cmd(args), b"");
            // Said in the terminal as it is: the screen is not taken and given back.
            assert!(shown.starts_with("kraal: no workspace named \"fpa@anvil\" in "), "{when}, {args:?}: {shown:?}");
            assert!(shown.contains("to attach to kraal on that server: kraal attach --host fpa@anvil"), "{shown}");
        }
    };
    check("without a session");
    assert_eq!(env.server_pid(), None, "a session was started only to say that a name is unknown");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    check("with a session");
}

/// Outside a pane, a command that acts on a pane and is given none says how it takes one: the
/// option for those that take an option, the argument for the one that takes an argument.
#[test]
fn a_command_given_no_pane_says_how_it_takes_one() {
    let env = Env::new("whichpane");
    let said = |args: &[&str]| {
        let out = env.try_run(args);
        assert!(!out.status.success(), "{args:?}");
        String::from_utf8_lossy(&out.stderr).into_owned()
    };
    let with_option: [&[&str]; 6] =
        [&["send", "hi"], &["keys", "enter"], &["read"], &["wait"], &["rename", "x"], &["buffer", "paste"]];
    for args in with_option {
        assert!(said(args).contains("which pane? pass --pane"), "{args:?}: {}", said(args));
        // What the message names is taken: the command gets as far as looking for the session.
        let named = said(&[args, &["--pane", "1"][..]].concat());
        assert!(named.contains("no kraal server"), "{args:?}: {named}");
    }
    let close = said(&["close"]);
    assert!(close.contains("which pane? name it after `kraal close`") && !close.contains("--pane"), "{close}");
    assert!(said(&["close", "1"]).contains("no kraal server"));
}

/// `kraal servers add` for an address already saved leaves its entry as it is, and says what it
/// did: it never says "saved" for options it dropped. `--agents` is the one thing it sets there.
#[test]
fn adding_a_saved_server_again_never_claims_a_save() {
    let env = Env::new("readd");
    let file = env.root.join("config/kraal/servers.toml");
    let add = |args: &[&str]| env.try_run(&[&["servers", "add", "admin@db.lan"], args].concat());
    let said =
        |out: &Output| format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let saved = || std::fs::read_to_string(&file).unwrap();

    let out = add(&["--name", "db", "--group", "data"]);
    assert!(out.status.success() && said(&out).starts_with("saved db in "), "{}", said(&out));
    let first = saved();
    // An option that would change the entry is refused, by its name, and nothing is written.
    let options: [&[&str]; 7] = [
        &["--env", "prod"],
        &["--group", "web"],
        &["--jump", "bastion"],
        &["--identity", "~/.ssh/db"],
        &["--note", "primary"],
        &["--name", "other"],
        &["--env", "prod", "--agents", "main"],
    ];
    for option in options {
        let out = add(option);
        let said = said(&out);
        assert!(!out.status.success(), "{option:?}: {said}");
        assert!(said.contains("admin@db.lan is already saved as db") && said.contains(option[0]), "{option:?}: {said}");
        assert!(said.contains("`kraal servers edit`") && !said.contains("saved db in"), "{option:?}: {said}");
        assert_eq!(saved(), first, "{option:?}");
    }
    // What the entry already has changes nothing, and is not refused.
    for same in [&[][..], &["--name", "db", "--group", "data"]] {
        let out = add(same);
        assert!(out.status.success() && said(&out).starts_with("db is already saved in "), "{}", said(&out));
        assert_eq!(saved(), first);
    }
    // The session whose agents are listed is set on a saved server too, and said.
    let out = add(&["--agents", "main"]);
    assert!(out.status.success(), "{}", said(&out));
    assert!(said(&out).starts_with("db is already saved in ") && said(&out).contains("session main"), "{}", said(&out));
    assert_eq!(saved(), format!("{first}agents = \"main\"\n"));
    // With another port it is another address: a new entry, with what it was given.
    let out = env.try_run(&["servers", "add", "admin@db.lan:2222", "--env", "prod"]);
    assert!(out.status.success() && said(&out).starts_with("saved db-admin in "), "{}", said(&out));
    assert_eq!(saved().matches("env = \"prod\"").count(), 1);
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

/// A program found but that cannot run (its interpreter is missing) is known only once the pane
/// started it: the pane then waits for Enter all the same, saying why, and the others run.
#[test]
fn a_program_that_cannot_run_after_all_waits_for_enter() {
    let env = Env::new("cantrun");
    let broken = env.root.join("broken");
    write_script(&broken, "#!/no/such/interpreter\n");
    env.workspace(
        "w",
        &format!(
            "layout {{\n    pane split_direction=\"vertical\" {{\n        pane name=\"ok\"\n        pane name=\"bad\" command={:?}\n    }}\n}}\n",
            broken.display().to_string()
        ),
    );
    env.run(&["open", "w"]);
    env.wait_for_text("bad", "cannot start");
    env.wait_for_text("bad", "No such file or directory");
    let start = Instant::now();
    loop {
        let v: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let bad = v.as_array().unwrap().iter().find(|p| p["name"] == "bad").cloned().expect("the pane is kept");
        if bad["suspended"] == true {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(5), "{bad}");
        std::thread::sleep(Duration::from_millis(50));
    }
    env.run(&["send", "-p", "ok", "-e", "echo still-here"]);
    env.wait_for_text("ok", "still-here");
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

/// A pane whose scrollback could not be written is saved again once it can be, though it
/// printed nothing since; `kraal kill` says when it still cannot be.
#[test]
fn scrollback_that_could_not_be_written_is_saved_later() {
    let env = Env::new("scrollfail");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    // A file where the session's scrollback directory goes: nothing can be written there.
    let dir = env.root.join("state/kraal/scrollback/scrollfail");
    std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
    std::fs::write(&dir, "in the way").unwrap();
    env.run(&["open", "w"]);
    env.run(&["send", "-p", "shell", "--enter", "echo once-$((5*5))"]);
    env.wait_for_text("shell", "once-25");
    // Long enough for a save of everything it printed to be tried, and to fail.
    std::thread::sleep(Duration::from_secs(11));
    assert!(std::fs::read_to_string(env.state_file()).unwrap().contains(".ans\""), "the state names its file");
    std::fs::remove_file(&dir).unwrap();
    env.run(&["kill"]);
    env.run(&["open", "w"]);
    let screen = env.run(&["read", "-p", "shell"]);
    assert!(screen.contains("once-25") && screen.contains("kraal: restored from"), "{screen}");
    // Still impossible when the server stops: said.
    std::fs::rename(&dir, env.root.join("aside")).unwrap();
    std::fs::write(&dir, "in the way").unwrap();
    env.run(&["send", "-p", "shell", "--enter", "echo twice-$((6*6))"]);
    env.wait_for_text_since_restore("shell", "twice-36");
    let out = env.try_run(&["kill"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success() && err.contains("the scrollback of a pane could not be saved"), "{err}");
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

/// `kraal config check` run in `dir`: whether it passed, and what it said on stderr.
fn config_check(env: &Env, dir: &Path) -> (bool, String) {
    let out = env.cmd(&["config", "check"]).current_dir(dir).output().unwrap();
    (out.status.success(), String::from_utf8(out.stderr).unwrap())
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
            let text = format!("{base}future_setting = 1\n[keys]\npreset = \"{preset}\"\n{more}");
            std::fs::write(&path, text).unwrap();
        };
        write("");
        assert_eq!(config_check(&env, &env.root), (true, "warning: future_setting: unknown key\n".into()));

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
            "warning: bar.kyes: unknown key (did you mean keys?)\nwarning: future_setting: unknown key\n"
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

/// Each `ssh.started` has its `ssh.ended`, as a reader keeping count of the connections needs:
/// when the connection drops (the pane stays, Enter connects again) and when the pane closes.
#[test]
fn every_ssh_that_starts_ends() {
    let env = Env::new("sshends");
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    // A connection to `drop` drops after a moment; the others stay.
    let ssh = bin.join("ssh");
    write_script(
        &ssh,
        "#!/bin/sh\necho \"connected $*\"\n[ \"$1\" = admin@drop ] && sleep 1 && exit 255\nexec sleep 1000\n",
    );
    let ssh = ssh.display().to_string();
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let (mut events, lines) = env.follow(&["events", "--since", "0", "ssh.*"]);
    let mut seen: Vec<String> = Vec::new();
    let mut wait = |what: &str| {
        while !seen.iter().any(|s| s == what) {
            let line = lines.recv_timeout(Duration::from_secs(10)).unwrap_or_else(|_| panic!("no {what}: {seen:?}"));
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            seen.push(format!("{} {}", v["type"].as_str().unwrap(), v["name"].as_str().unwrap()));
        }
    };
    env.run(&["run", "--name", "dropped", "--", &ssh, "admin@drop"]);
    wait("ssh.started dropped");
    wait("ssh.ended dropped");
    env.wait_for_text("dropped", "connected admin@drop");
    env.run(&["run", "--name", "closed", "--", &ssh, "admin@db"]);
    wait("ssh.started closed");
    env.run(&["close", "closed"]);
    wait("ssh.ended closed");
    let _ = events.kill();
    let _ = events.wait();
}

/// A directory a quiet pane is in that becomes a Terraform one (`terraform init` run from
/// elsewhere) is guarded as soon as it is: the change is told, not looked for now and then.
#[test]
fn a_directory_that_becomes_a_terraform_one_is_guarded() {
    let env = Env::new("tfnew");
    let tf = env.root.join("infra");
    std::fs::create_dir_all(&tf).unwrap();
    env.workspace("w", &format!("layout {{\n    pane name=\"t\" cwd={:?}\n}}\n", tf.display().to_string()));
    env.run(&["open", "w"]);
    env.wait_for_text("t", "$");
    let production = || {
        let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
        let t = panes.as_array().unwrap().iter().find(|p| p["name"] == "t").cloned().unwrap();
        t["production"].as_str().map(String::from)
    };
    // Known not to be a Terraform directory, and quiet.
    std::thread::sleep(Duration::from_secs(4));
    assert_eq!(production(), None);
    std::fs::create_dir_all(tf.join(".terraform")).unwrap();
    std::fs::write(tf.join(".terraform/environment"), "prod\n").unwrap();
    let start = Instant::now();
    while production().is_none() {
        assert!(start.elapsed() < PATIENCE, "never seen as production");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(production().as_deref(), Some("Terraform workspace prod"));
    let out = env.try_run(&["send", "-p", "t", "echo", "hi"]);
    assert!(!out.status.success(), "sent to production");
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
        assert!(start.elapsed() < PATIENCE, "never seen as production: {ops}");
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
    // Killed while it wrote its run ledger, it would leave the temporary file of that write.
    std::fs::write(runs.join(format!("..active-runbooks.json.{pid}.38.tmp")), "[]").unwrap();
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

/// The session's answer to `request`, sent through its socket as written: what a kraal of
/// another release, or any other program, can send.
fn raw_request(env: &Env, request: serde_json::Value) -> serde_json::Value {
    use std::io::Read;
    let socket = env.root.join("run/kraal").join(format!("{}.sock", env.session));
    let mut stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let message = serde_json::json!({ "type": "request", "version": 2, "request": request }).to_string();
    raw_frame(&mut stream, 0, message.as_bytes());
    let mut header = [0u8; 5];
    stream.read_exact(&mut header).unwrap();
    let mut payload = vec![0u8; u32::from_le_bytes(header[..4].try_into().unwrap()) as usize];
    stream.read_exact(&mut payload).unwrap();
    serde_json::from_slice::<serde_json::Value>(&payload).unwrap()["result"].take()
}

/// A repository's runbook is a file someone else wrote: nothing of it runs before this version
/// of it is approved, and approving is an act of its own. Listing it, showing it, planning it
/// and `--yes` (which answers the plan) approve nothing; `kraal runbook trust` does, for the
/// file as it is.
#[test]
fn a_repository_runbook_runs_only_once_approved() {
    let env = Env::new("rbtrust");
    let repo = env.root.join("app");
    let (ran, listed) = (env.root.join("ran"), env.root.join("listed"));
    let file = repo.join(".kraal/runbooks/deploy.toml");
    let version = |word: &str| {
        format!(
            "description = \"deploy\"\nsteps = [\"echo {word} > {}\"]\n[[param]]\nname = \"V\"\ndefault = \"1\"\nchoices_command = \"touch {}\"\n",
            ran.display(),
            listed.display()
        )
    };
    let _ = git_repo(&repo, &[(".kraal/runbooks/deploy.toml", &version("one"))]);
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let kraal = |args: &[&str]| {
        let mut c = env.cmd(&[&["runbook"], args].concat());
        c.current_dir(&repo).stdin(std::process::Stdio::null()).output().unwrap()
    };
    let text =
        |out: &Output| format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let store = env.root.join("state/kraal/trusted-runbooks.json");
    // What a program other than this kraal's command line can do: ask for a plan, then run it.
    let plan = || {
        let request =
            serde_json::json!({ "cmd": "runbook_plan", "path": file, "target": null, "params": [], "pane": null });
        let plan = raw_request(&env, request);
        plan["Ok"]["data"]["token"].as_str().unwrap_or_else(|| panic!("no plan: {plan}")).to_string()
    };
    let run_plan = |token: &str| raw_request(&env, serde_json::json!({ "cmd": "runbook_run", "token": token }));
    let refused = |answer: &serde_json::Value| answer["Err"].as_str().is_some_and(|e| e.contains("is not approved"));

    // Reading it runs nothing and approves nothing.
    for args in [&["list"][..], &["list", "--json"], &["show", "deploy"], &["run", "deploy", "--dry-run"]] {
        let out = kraal(args);
        assert!(out.status.success() && text(&out).contains("deploy"), "{args:?}: {}", text(&out));
        assert!(!ran.exists() && !listed.exists() && !store.exists(), "{args:?}");
    }
    // `--yes` answers the plan, and without a terminal nobody answers: neither approves.
    for args in [&["run", "deploy", "--yes", "--wait"][..], &["run", "deploy"]] {
        let out = kraal(args);
        let said = text(&out);
        assert!(!out.status.success(), "{args:?}: {said}");
        assert!(said.contains("is not approved") && said.contains("`kraal runbook trust deploy`"), "{args:?}: {said}");
        assert!(!ran.exists() && !listed.exists() && !store.exists(), "{args:?}");
    }
    let answer = run_plan(&plan());
    assert!(refused(&answer), "{answer}");
    assert!(!ran.exists() && !store.exists());

    // Approved as it is now: it runs without a question, and so does a plan made before.
    let before = plan();
    let out = kraal(&["trust", "deploy"]);
    assert!(out.status.success() && text(&out).contains("is approved as it is now"), "{}", text(&out));
    let out = kraal(&["run", "deploy", "--yes", "--wait"]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(std::fs::read_to_string(&ran).unwrap(), "one\n");
    assert!(!listed.exists(), "the command line asks for no choice");
    std::fs::remove_file(&ran).unwrap();
    let answer = run_plan(&before);
    assert!(answer["Ok"].is_object(), "{answer}");
    wait_until(Duration::from_secs(10), "the plan made before the approval never ran", || {
        std::fs::read_to_string(&ran).is_ok_and(|t| t == "one\n")
    });

    // Changed since: that version was never approved.
    std::fs::write(&file, version("two")).unwrap();
    let out = kraal(&["run", "deploy", "--yes", "--wait"]);
    assert!(!out.status.success() && text(&out).contains("`kraal runbook trust deploy`"), "{}", text(&out));
    let answer = run_plan(&plan());
    assert!(refused(&answer), "{answer}");
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(std::fs::read_to_string(&ran).unwrap(), "one\n");
    let out = kraal(&["trust", "deploy"]);
    assert!(out.status.success(), "{}", text(&out));
    let out = kraal(&["run", "deploy", "--yes", "--wait"]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(std::fs::read_to_string(&ran).unwrap(), "two\n");

    // What these commands print of a repository's files, a terminal acts on none of it: an
    // escape sequence in a description, in a step, or in a file that cannot be read.
    let odd = "description = \"clear \\u001b[2J\"\nsteps = [\"echo \\u001b[2K\"]\n";
    std::fs::write(repo.join(".kraal/runbooks/odd.toml"), odd).unwrap();
    std::fs::write(repo.join(".kraal/runbooks/bad.toml"), "steps = \x1b[2J\n").unwrap();
    let said = text(&kraal(&["list"]));
    assert!(said.contains("clear \\u{1b}[2J") && said.contains("steps = \\u{1b}[2J"), "{said:?}");
    assert!(!said.contains('\x1b'), "{said:?}");
    let said = text(&kraal(&["run", "odd", "--dry-run"]));
    assert!(said.contains("1. echo \\u{1b}[2K") && !said.contains('\x1b'), "{said:?}");
    assert!(!text(&kraal(&["show", "odd"])).contains('\x1b'));

    // The user's own runbooks need no approval.
    std::fs::create_dir_all(env.root.join("config/kraal/runbooks")).unwrap();
    std::fs::write(env.root.join("config/kraal/runbooks/mine.toml"), "steps = [\"true\"]\n").unwrap();
    let out = kraal(&["trust", "mine"]);
    assert!(out.status.success() && text(&out).contains("needs no approval"), "{}", text(&out));
    let out = kraal(&["run", "mine", "--yes", "--wait"]);
    assert!(out.status.success(), "{}", text(&out));
}

/// An approval is of a file in its repository, however the way there is spelled: through a link
/// to the checkout or not (`/tmp` is one on macOS), on the command line or in a plan another
/// program asks for, and in a store written by 0.3.0 under the spelling used then. A link that
/// comes to lead to another repository carries no approval there.
#[test]
fn a_runbook_approval_does_not_depend_on_how_its_repository_is_reached() {
    let env = Env::new("rblink");
    let (ran, link) = (env.root.join("ran"), env.root.join("link"));
    let runbook = format!("steps = [\"echo ran >> {}\"]\n", ran.display());
    let repository = |dir: &str| {
        let repo = env.root.join(dir).join("app");
        let _ = git_repo(&repo, &[(".kraal/runbooks/deploy.toml", &runbook)]);
        repo
    };
    let (first, second) = (repository("first"), repository("second"));
    std::os::unix::fs::symlink(env.root.join("first"), &link).unwrap();
    // The file where its repository really is, and as a path through the link spells it.
    let file = |repo: &Path| repo.join(".kraal/runbooks/deploy.toml").display().to_string();
    let (real, spelled) = (file(&first.canonicalize().unwrap()), file(&link.join("app")));
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let kraal = |dir: &Path, args: &[&str]| {
        let mut c = env.cmd(&[&["runbook"], args].concat());
        let out = c.current_dir(dir).stdin(std::process::Stdio::null()).output().unwrap();
        (
            out.status.success(),
            format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)),
        )
    };
    // A plan of the file at `path`, asked for and run as another program would.
    let run_plan = |path: &str| {
        let request =
            serde_json::json!({ "cmd": "runbook_plan", "path": path, "target": null, "params": [], "pane": null });
        let plan = raw_request(&env, request);
        let token = plan["Ok"]["data"]["token"].as_str().unwrap_or_else(|| panic!("no plan: {plan}")).to_string();
        raw_request(&env, serde_json::json!({ "cmd": "runbook_run", "token": token }))
    };
    let store = env.root.join("state/kraal/trusted-runbooks.json");
    let approved = || -> Vec<String> {
        let store: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&store).unwrap()).unwrap();
        store.as_object().unwrap().keys().cloned().collect()
    };

    // Approved in the checkout: approved through the link too, and recorded once.
    let (ok, said) = kraal(&first, &["trust", "deploy"]);
    assert!(ok, "{said}");
    assert_eq!(approved(), std::slice::from_ref(&real));
    let answer = run_plan(&spelled);
    assert!(answer["Ok"].is_object(), "{answer}");
    let (ok, said) = kraal(&link.join("app"), &["run", "deploy", "--yes", "--wait"]);
    assert!(ok, "{said}");

    // A store written by 0.3.0 under the spelling through the link still approves the file; the
    // next approval leaves the file one entry, under its real place.
    let mut written = serde_json::Map::new();
    written.insert(spelled.clone(), serde_json::Value::String(runbook.clone()));
    std::fs::write(&store, serde_json::Value::Object(written).to_string()).unwrap();
    let (ok, said) = kraal(&first, &["run", "deploy", "--yes", "--wait"]);
    assert!(ok, "{said}");
    let answer = run_plan(&real);
    assert!(answer["Ok"].is_object(), "{answer}");
    let (ok, said) = kraal(&first, &["trust", "deploy"]);
    assert!(ok, "{said}");
    assert_eq!(approved(), std::slice::from_ref(&real));

    // The link comes to lead to another repository, with the same file: nothing is approved
    // there.
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(second.parent().unwrap(), &link).unwrap();
    let answer = run_plan(&spelled);
    assert!(answer["Err"].as_str().is_some_and(|e| e.contains("is not approved")), "{answer}");
    let (ok, said) = kraal(&link.join("app"), &["run", "deploy", "--yes", "--wait"]);
    assert!(!ok && said.contains("is not approved"), "{said}");
}

/// In a terminal, `kraal runbook run` asks before a repository runbook nobody approved: the
/// question shows the file, with what a terminal would hide written out, names the approval,
/// and comes before the plan's.
#[cfg(target_os = "linux")]
#[test]
fn a_repository_runbook_is_approved_by_a_question_of_its_own() {
    let env = Env::new("rbask");
    let repo = env.root.join("app");
    let ran = env.root.join("ran");
    // A zero-width space, which a terminal shows nothing of.
    let runbook = format!("steps = [\"echo ran > {} #\u{200b}\"]\n", ran.display());
    let _ = git_repo(&repo, &[(".kraal/runbooks/deploy.toml", &runbook)]);
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let store = env.root.join("state/kraal/trusted-runbooks.json");
    let run = |input: &[u8]| {
        let mut c = env.cmd(&["runbook", "run", "deploy", "--wait"]);
        c.current_dir(&repo);
        in_terminal(c, input)
    };

    let shown = run(b"n\n");
    assert!(shown.contains("Trust this version?") && shown.contains("#\\u{200b}"), "{shown}");
    assert!(!shown.contains("Run it?"), "{shown}");
    assert!(!ran.exists() && !store.exists());
    // Approved, then its plan declined: nothing runs, and it stays approved.
    let shown = run(b"y\nn\n");
    assert!(shown.contains("Trust this version?") && shown.contains("Run it?"), "{shown}");
    assert!(!ran.exists() && store.exists());
    let shown = run(b"y\n");
    assert!(!shown.contains("Trust this version?") && shown.contains("finished: 1 ok"), "{shown}");
    assert!(ran.exists());
}

/// From the list of runbooks, a repository runbook asks to be trusted before anything of it
/// runs, its `choices_command` included; a no leaves it as it was, and a change asks again. A
/// Markdown one asks before its first block is typed.
#[test]
fn the_runbook_list_asks_before_a_repository_runbook_does_anything() {
    let env = Env::new("rbpick");
    let repo = env.root.join("app");
    let (ran, listed) = (env.root.join("ran"), env.root.join("listed"));
    let file = repo.join(".kraal/runbooks/deploy.toml");
    let version = |word: &str| {
        format!(
            "steps = [\"echo {word} $V > {}\"]\n[[param]]\nname = \"V\"\nchoices_command = \"touch {}; echo first\"\n",
            ran.display(),
            listed.display()
        )
    };
    let _ = git_repo(
        &repo,
        &[
            (".kraal/runbooks/deploy.toml", &version("one")),
            (".kraal/runbooks/notes.md", "# Notes\n\n```sh\necho from-\"notes\"\n```\n"),
        ],
    );
    env.workspace("w", &format!("layout {{\n    pane name=\"shell\" cwd={:?}\n}}\n", repo.display().to_string()));
    env.run(&["open", "w"]);
    let store = env.root.join("state/kraal/trusted-runbooks.json");
    let mut client = RawClient::attach(&env, "w");
    // The list has the repository's runbooks once git has seen it.
    client.wait_for_screen("⎇ main");
    let pick = |client: &mut RawClient, name: &str| {
        env.run(&["focus", "shell"]);
        client.send(b"\x02Rb");
        client.wait_for_row("Runbooks");
        client.send(name.as_bytes());
        client.wait_for_row("repository app");
        client.send(b"\r");
    };
    let gone = |client: &RawClient, text: &str| {
        client.wait_for_rows(&format!("without {text:?}"), |rows| !rows.iter().any(|r| r.contains(text)));
    };

    pick(&mut client, "deploy");
    client.wait_for_row("Trust the runbook deploy");
    std::thread::sleep(Duration::from_millis(300));
    assert!(!listed.exists() && !ran.exists() && !store.exists(), "something ran before the answer");
    client.send(b"n");
    gone(&client, "Trust the runbook deploy");
    std::thread::sleep(Duration::from_millis(300));
    assert!(!listed.exists() && !ran.exists() && !store.exists(), "a no approved or ran something");

    // Approved: its choices are listed, then its plan is shown.
    pick(&mut client, "deploy");
    client.wait_for_row("Trust the runbook deploy");
    client.send(b"y");
    gone(&client, "Trust the runbook deploy");
    client.wait_for_row("first");
    assert!(listed.exists() && !ran.exists());
    client.send(b"\r");
    client.wait_for_row("Run deploy on this machine?");
    client.send(b"y");
    wait_until(Duration::from_secs(10), "the approved runbook never ran", || {
        std::fs::read_to_string(&ran).is_ok_and(|t| t == "one first\n")
    });

    // Changed since: asked again, before its choices are listed.
    std::fs::remove_file(&listed).unwrap();
    std::fs::write(&file, version("two")).unwrap();
    pick(&mut client, "deploy");
    client.wait_for_row("Trust the runbook deploy");
    std::thread::sleep(Duration::from_millis(300));
    assert!(!listed.exists());
    client.send(b"n");
    gone(&client, "Trust the runbook deploy");

    // A document: its first block waits for the answer; approved from the command line, the
    // block is typed without a question.
    pick(&mut client, "notes");
    client.wait_for_row("1. Notes");
    client.send(b"\r");
    client.wait_for_row("From the repository");
    client.send(b"n");
    gone(&client, "From the repository");
    assert!(!env.run(&["read", "-p", "shell"]).contains("from-notes"));
    let out = env.cmd(&["runbook", "trust", "notes"]).current_dir(&repo).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    env.run(&["focus", "shell"]);
    client.send(b"\x02Rb");
    client.wait_for_row("1. Notes");
    client.send(b"\r");
    env.wait_for_text("shell", "from-notes");
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
        assert!(start.elapsed() < PATIENCE, "the pane's context is read: {panes}");
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
        assert!(start.elapsed() < PATIENCE, "the pane's context is read: {panes}");
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
    // A read of the environment file takes ten minutes while `environment.slow` exists: longer
    // than this test waits for anything.
    let out = env.cmd(&["open", "w"]).env("KRAAL_TEST_SLOW_KUBECONFIG_MS", "600000").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    wait_for_context(&env, "tf:prod-a");
    env.wait_for_text("b", "$");
    let refused = |text: &str| !env.cmd(&["send", "-p", "b", "-e", text]).output().unwrap().status.success();
    assert!(refused("echo here"), "b is production");
    env.run(&["send", "-p", "b", "-e", "--production", "cd /"]);
    wait_for_contexts(&env, "b", &[], None);
    // A scan (every 3 s) passes while no pane is in the directory.
    std::thread::sleep(Duration::from_millis(4000));
    assert!(!refused("echo away"), "away from the directory, b is not production");
    // The directory cannot be read again in time from here on: what is said of it once the pane
    // is back is what was known, however long the pane takes to get there.
    std::fs::write(infra.join(".terraform/environment.slow"), "").unwrap();
    env.run(&["send", "-p", "b", "-e", &format!("cd {}", infra.display())]);
    wait_for_contexts(&env, "b", &["tf:prod-a"], Some("Terraform workspace prod-a"));
    assert!(refused("echo back"), "back in the directory, b is production again");
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
        assert!(start.elapsed() < PATIENCE, "never {contexts:?} ({why:?}): {pane}");
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

/// What git says of a pane's directory follows what is done in its repository from elsewhere,
/// promptly, while the pane prints nothing: a checkout, a commit.
#[test]
fn git_in_frames_follows_changes_made_elsewhere() {
    let env = Env::new("gitwatch");
    let repo = env.root.join("repo");
    let git = git_repo(&repo, &[("a.txt", "1")]);
    // An upstream, for the commits ahead of it.
    git(&["config", "remote.origin.url", "/nowhere"]);
    git(&["config", "remote.origin.fetch", "+refs/heads/*:refs/remotes/origin/*"]);
    git(&["update-ref", "refs/remotes/origin/main", "HEAD"]);
    env.workspace("w", &format!("layout {{\n    pane name=\"a\" cwd={:?}\n}}\n", repo.display().to_string()));
    env.run(&["open", "w"]);
    let client = RawClient::attach(&env, "w");
    client.wait_for_screen("main");
    env.wait_for_text("a", "$");
    // Long enough for the server to be idle.
    std::thread::sleep(Duration::from_secs(4));
    let start = Instant::now();
    git(&["checkout", "-q", "-b", "qqbranch", "--track", "origin/main"]);
    client.wait_for_screen("qqbranch");
    assert!(start.elapsed() < Duration::from_secs(2), "a checkout showed after {:?}", start.elapsed());
    // A commit replaces the branch's ref, not the index.
    std::thread::sleep(Duration::from_secs(2));
    let start = Instant::now();
    git(&["commit", "-q", "--allow-empty", "-m", "two"]);
    client.wait_for_screen("↑1");
    assert!(start.elapsed() < Duration::from_secs(2), "a commit showed after {:?}", start.elapsed());
}

/// An idle server sleeps: no thread of it wakes up again and again while nothing happens.
#[cfg(target_os = "linux")]
#[test]
fn an_idle_server_sleeps() {
    let env = Env::new("sleeps");
    // In a directory where nothing happens (the home directory may be busy: its changes are
    // looked at).
    let quiet = env.root.join("quiet");
    std::fs::create_dir_all(&quiet).unwrap();
    env.workspace(
        "w",
        &format!(
            "layout {{\n    pane name=\"a\" cwd={0:?}\n    pane name=\"b\" cwd={0:?}\n}}\n",
            quiet.display().to_string()
        ),
    );
    env.run(&["open", "w"]);
    let client = RawClient::attach(&env, "w");
    env.wait_for_text("a", "$");
    env.wait_for_text("b", "$");
    let pid = env.server_pid().expect("the server's pid");
    // The times each of its threads went to sleep (not those another program took the processor
    // from them while they ran: that depends on the machine's load).
    let sleeps = || -> std::collections::HashMap<std::ffi::OsString, u64> {
        let tasks = std::fs::read_dir(format!("/proc/{pid}/task")).unwrap();
        tasks
            .flatten()
            .filter_map(|t| {
                let status = std::fs::read_to_string(t.path().join("status")).ok()?;
                let line = status.lines().find(|l| l.starts_with("voluntary_ctxt_switches"))?;
                Some((t.file_name(), line.split_whitespace().last()?.parse().ok()?))
            })
            .collect()
    };
    // Past the requests above, and what they started.
    std::thread::sleep(Duration::from_secs(4));
    let start = Instant::now();
    let woke: u64 = loop {
        let before = sleeps();
        std::thread::sleep(Duration::from_secs(6));
        let after = sleeps();
        // A thread that ended meanwhile was still finishing what those requests started, as on
        // a loaded machine: the server was not idle yet, and is looked at again.
        if before.keys().all(|thread| after.contains_key(thread)) {
            break after.iter().map(|(thread, now)| now - before.get(thread).copied().unwrap_or(0)).sum();
        }
        assert!(start.elapsed() < Duration::from_secs(60), "the server's threads keep ending: {after:?}");
    };
    // At most its periodic work, and what that starts: not the readers of the panes, nor
    // anything else.
    assert!(woke <= 6, "{woke} wake-ups in 6 s");
    // Still there when something happens.
    env.run(&["send", "-p", "a", "-e", "echo still-awake"]);
    env.wait_for_text("a", "still-awake");
    drop(client);
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
    client.send(b"n");
    client.wait_for_row("the worktree no was not created");
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
    // The local port is what names a tunnel, and the list shows it: of two tunnels to the same
    // port of two servers, each is closed by its own. A tunnel has no other name.
    let to_wanted = |list: &str| -> Vec<(String, u16)> {
        let lines = list.lines().filter(|l| l.contains(&format!(":{wanted} → localhost:")));
        lines
            .map(|l| {
                let (server, local) = l.split_once(" → localhost:").unwrap();
                (server.split_whitespace().last().unwrap().to_string(), local.trim().parse().unwrap())
            })
            .collect()
    };
    let both = to_wanted(&run(&["tunnel", "list"]));
    assert_eq!(both.len(), 2, "{both:?}");
    let out = env.cmd(&["tunnel", "rm", "#1"]).env("PATH", &path).output().unwrap();
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success() && said.contains("not a local port: #1") && said.contains("kraal tunnel list"),
        "{said}"
    );
    run(&["tunnel", "rm", &both[1].1.to_string()]);
    assert_eq!(to_wanted(&run(&["tunnel", "list"])), [both[0].clone()]);

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
fn a_paste_held_until_its_end_goes_where_it_started() {
    let env = Env::new("heldpaste");
    let (got, other_got) = (env.root.join("pasted"), env.root.join("other-pasted"));
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.run(&["open", "w"]);
    let take = |file: &Path| format!("stty raw -echo; cat > {}", file.display());
    env.run(&["run", "--name", "sink", "--", "sh", "-c", &take(&got)]);
    env.run(&["run", "--name", "other", "--", "sh", "-c", &take(&other_got)]);
    env.run(&["focus", "sink"]);
    let mut client = RawClient::attach(&env, "w");
    // Less than kraal holds back (8 MiB): it goes whole once it ends.
    let line = "x".repeat(99) + "\n";
    client.send(b"\x1b[200~");
    for chunk in line.repeat(10_000).as_bytes().chunks(64 << 10) {
        client.send(chunk);
    }
    std::thread::sleep(Duration::from_millis(300));
    // The focus moves before its end: all of it still goes where it started.
    env.run(&["focus", "other"]);
    client.send(b"end\x1b[201~");
    let size = |f: &Path| std::fs::metadata(f).map(|m| m.len()).unwrap_or(0);
    let start = Instant::now();
    while size(&got) < 1_000_003 && size(&other_got) == 0 {
        assert!(start.elapsed() < Duration::from_secs(10), "only {} bytes arrived", size(&got));
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!((size(&got), size(&other_got)), (1_000_003, 0));
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

/// A terminal of `cols × rows` after `bytes`.
fn emulate(
    bytes: &[u8],
    cols: usize,
    rows: usize,
) -> alacritty_terminal::Term<alacritty_terminal::event::VoidListener> {
    use alacritty_terminal::term::{Config, Term, test::TermSize};
    let mut term = Term::new(Config::default(), &TermSize::new(cols, rows), alacritty_terminal::event::VoidListener);
    let mut parser: alacritty_terminal::vte::ansi::Processor = alacritty_terminal::vte::ansi::Processor::new();
    parser.advance(&mut term, bytes);
    term
}

/// The text of each row a terminal of `cols × rows` shows after `bytes`.
fn text_after(bytes: &[u8], cols: usize, rows: usize) -> Vec<String> {
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::cell::Flags;
    let term = emulate(bytes, cols, rows);
    let grid = term.grid();
    (0..rows)
        .map(|y| {
            let row = &grid[Line(y as i32)];
            (0..cols)
                .map(|x| &row[Column(x)])
                .filter(|c| !c.flags.contains(Flags::WIDE_CHAR_SPACER))
                .map(|c| c.c)
                .collect()
        })
        .collect()
}

/// What a terminal of `cols × rows` shows after `bytes`: each row's cells (text, colours,
/// attributes, link) and the cursor.
fn terminal_after(bytes: &[u8], cols: usize, rows: usize) -> Vec<String> {
    use alacritty_terminal::index::{Column, Line};
    use alacritty_terminal::term::TermMode;
    let term = emulate(bytes, cols, rows);
    let grid = term.grid();
    let mut out: Vec<String> = (0..rows)
        .map(|y| {
            let row = &grid[Line(y as i32)];
            (0..cols)
                .map(|x| {
                    let c = &row[Column(x)];
                    let link = c.hyperlink().map(|h| h.uri().to_string());
                    let text: String =
                        std::iter::once(c.c).chain(c.zerowidth().into_iter().flatten().copied()).collect();
                    format!("{text:?} {:?} {:?} {:?} {:?} {link:?}", c.fg, c.bg, c.flags, c.underline_color())
                })
                .collect::<Vec<_>>()
                .join(" | ")
        })
        .collect();
    let cursor = grid.cursor.point;
    out.push(format!(
        "cursor {} {} shown {}",
        cursor.line.0,
        cursor.column.0,
        term.mode().contains(TermMode::SHOW_CURSOR)
    ));
    out
}

impl RawClient {
    /// The text of each row its terminal (120×40) shows.
    fn rows(&self) -> Vec<String> {
        text_after(&self.screen.lock().unwrap(), 120, 40)
    }

    /// Wait until its terminal shows `text` on a row; the rows then. Unlike
    /// [`RawClient::wait_for_screen`], this finds text drawn over cells that already had some of
    /// it, which a frame leaves out.
    fn wait_for_row(&self, text: &str) -> Vec<String> {
        self.wait_for_rows(text, |rows| rows.iter().any(|r| r.contains(text)))
    }

    /// Wait until the rows its terminal shows are `what` says (`done`); the rows then.
    fn wait_for_rows(&self, what: &str, done: impl Fn(&[String]) -> bool) -> Vec<String> {
        let start = Instant::now();
        loop {
            let rows = self.rows();
            if done(&rows) {
                return rows;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "never {what}:\n{}", rows.join("\n"));
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Wait until nothing more is drawn for a while; how much was drawn.
    fn settle(&self) -> usize {
        let mut seen = self.screen.lock().unwrap().len();
        let start = Instant::now();
        loop {
            std::thread::sleep(Duration::from_millis(300));
            let now = self.screen.lock().unwrap().len();
            if now == seen {
                return now;
            }
            seen = now;
            assert!(start.elapsed() < Duration::from_secs(10), "the screen never settles");
        }
    }
}

/// Frames are drawn over the last one, with the rows that did not change left out: whatever
/// happens, a client ends up showing what a client attaching then is shown whole.
#[test]
fn frames_drawn_over_the_last_one_end_as_a_whole_frame() {
    let env = Env::new("incremental");
    std::fs::create_dir_all(env.root.join("state/kraal")).unwrap();
    std::fs::write(env.root.join("state/kraal/welcomed"), "").unwrap();
    env.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    env.workspace("w2", "layout {\n    pane name=\"other\"\n}\n");
    env.workspace("w3", "layout {\n    pane name=\"third\"\n}\n");
    env.run(&["open", "w"]);
    let colors = "printf '\\033[1;31mred \\033[42;4mgreen\\033[0m \\346\\227\\245\\346\\234\\254 \
                  \\033]8;;https://example.org\\033\\\\link\\033]8;;\\033\\\\ e\\314\\201 done\\n'; exec cat";
    env.run(&["run", "--name", "colors", "--right", "--", "sh", "-c", colors]);
    env.run(&["run", "--name", "lines", "--down", "--", "sh", "-c", "seq 1 200; exec cat"]);
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_screen("done");
    let same = |client: &RawClient, step: &str| {
        client.settle();
        let fresh = RawClient::attach(&env, "w");
        fresh.settle();
        client.settle();
        let (a, b) = (client.screen.lock().unwrap().clone(), fresh.screen.lock().unwrap().clone());
        let (a, b) = (terminal_after(&a, 120, 40), terminal_after(&b, 120, 40));
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
    for b in b"yz 123" {
        client.send(&[*b]);
    }
    same(&client, "typing");
    // Output scrolling one pane.
    env.run(&["send", "-p", "lines", "more\r"]);
    same(&client, "scrolling");
    // Image placeholders without an image draw as blanks.
    env.run(&[
        "run",
        "--name",
        "img",
        "--right",
        "--",
        "sh",
        "-c",
        "printf 'img:\\364\\216\\273\\256 end\\n'; exec cat",
    ]);
    same(&client, "placeholders");
    // A floating pane over the others, then gone.
    env.run(&["run", "--name", "float", "--floating", "--", "sh", "-c", "echo FLOAT; exec cat"]);
    client.wait_for_screen("FLOAT");
    same(&client, "floating");
    env.run(&["close", "float"]);
    same(&client, "floating closed");
    // Another tab, and back.
    env.run(&["run", "--name", "tab2", "--tab", "--", "sh", "-c", "echo TAB2; exec cat"]);
    client.wait_for_screen("TAB2");
    same(&client, "new tab");
    env.run(&["focus", "colors"]);
    same(&client, "tab back");
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
    // A second workspace: the sidebar shows; a third: the sidebar changes.
    env.run(&["open", "w2"]);
    std::thread::sleep(Duration::from_millis(600));
    same(&client, "sidebar");
    env.run(&["open", "w3"]);
    same(&client, "sidebar changed");
    // A pane closed: the others take its place.
    env.run(&["close", "img"]);
    same(&client, "pane closed");
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

/// An account renamed from the agent picker, then from the command line: its rule and its usage
/// follow, and the agent running on it goes on under the new name.
#[test]
fn a_renamed_account_keeps_its_rules_its_usage_and_its_agents() {
    let env = Env::new("renameacct");
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    write_script(&bin.join("claude"), "#!/bin/sh\necho \"home=${CLAUDE_CONFIG_DIR:-default}\"\nexec sleep 100\n");
    let home = env.root.join("alpha-home");
    std::fs::create_dir_all(&home).unwrap();
    let file = env.root.join("config/kraal/accounts.toml");
    std::fs::write(
        &file,
        format!(
            "[[account]]\nname = \"alpha\"\nagent = \"claude\"\nhome = {:?}\n\n[[rule]]\nworkspace = \"w\"\naccounts = [\"alpha\"]\n",
            home.display().to_string()
        ),
    )
    .unwrap();
    let usage = env.root.join("state/kraal/usage");
    std::fs::create_dir_all(&usage).unwrap();
    std::fs::write(usage.join("claude-alpha.json"), r#"{"observed_at":0,"source":"statusline"}"#).unwrap();
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let socket = env.root.join("run/kraal/renameacct.sock");
    let out = env
        .cmd(&["agent", "claude", "--account", "alpha", "--tab"])
        .env("PATH", &path)
        .env("KRAAL_SOCKET", &socket)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let panes: serde_json::Value = serde_json::from_str(&env.run(&["panes", "--json"])).unwrap();
    let agent = panes.as_array().unwrap().iter().find(|p| p["command"][0] == "claude").unwrap()["id"].to_string();
    env.wait_for_text(&agent, &format!("home={}", home.display()));
    let (mut events, seen) = env.follow(&["events", "--since", "0", "agent.*"]);
    let hook = |payload: &str| {
        let mut c = env.cmd(&["hook", "claude"]);
        c.env("KRAAL_PANE", &agent).env("KRAAL_SOCKET", &socket).stdin(std::process::Stdio::piped());
        let mut child = c.spawn().unwrap();
        use std::io::Write;
        child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
        assert!(child.wait().unwrap().success());
    };
    // The account the next event of type `ty` names.
    let account_said = |ty: &str| -> String {
        loop {
            let line = seen.recv_timeout(Duration::from_secs(10)).expect("an agent event");
            let v: serde_json::Value = serde_json::from_str(&line).unwrap();
            if v["type"] == ty {
                return v["account"].as_str().unwrap().to_string();
            }
        }
    };
    hook(r#"{"hook_event_name":"UserPromptSubmit","session_id":"s-1"}"#);
    assert_eq!(account_said("agent.working"), "claude:alpha");
    let mut client = RawClient::attach(&env, "w");

    // Tab on the account in the agent picker, then its fourth action, Rename, and the new name.
    // The client is sent what changed since the frame before: a word is looked for only where
    // it is drawn over blank cells or other letters.
    client.send(b"\x02a");
    client.wait_for_screen("Start an agent");
    client.send(b"alpha");
    std::thread::sleep(Duration::from_millis(200));
    client.send(b"\t");
    client.wait_for_screen("Rename");
    client.send(b"\x1b[B\x1b[B\x1b[B\r\x15gamma\r");
    let named = |name: &str| std::fs::read_to_string(&file).unwrap().contains(&format!("name = \"{name}\""));
    wait_until(Duration::from_secs(10), "renamed from the picker", || named("gamma"));
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(text.contains("accounts = [\"gamma\"]") && !text.contains("alpha\""), "{text}");
    assert!(usage.join("claude-gamma.json").exists() && !usage.join("claude-alpha.json").exists());
    hook(r#"{"hook_event_name":"Stop","session_id":"s-1"}"#);
    assert_eq!(account_said("agent.done"), "claude:gamma");

    // From the command line: the session reads accounts.toml again when it changes.
    assert_eq!(env.run(&["accounts", "rename", "claude:gamma", "zulu"]).trim(), "renamed claude·gamma to claude·zulu");
    client.wait_for_screen("zulu");
    hook(r#"{"hook_event_name":"UserPromptSubmit","session_id":"s-1"}"#);
    assert_eq!(account_said("agent.working"), "claude:zulu");
    assert!(std::fs::read_to_string(&file).unwrap().contains("accounts = [\"zulu\"]"));
    assert!(usage.join("claude-zulu.json").exists());
    let out = env.try_run(&["accounts", "rename", "claude:default", "mine"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot be renamed") && !out.status.success());
    let screen = env.run(&["read", "-p", &agent]);
    assert!(screen.contains(&format!("home={}", home.display())), "the agent went on: {screen}");
    let _ = events.kill();
    let _ = events.wait();
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
    raw_frame(&mut client.stream, 0, br#"{"type":"resize","cols":120,"rows":40}"#);
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

/// A fake ssh, first in the PATH returned: it runs the command "on the server", that is here, as
/// the kraal of `remote` (with its own directories); "down.lan" does not answer. Every call is
/// logged in the file returned.
#[cfg(unix)]
fn fake_ssh(local: &Env, remote: &Env) -> (PathBuf, String) {
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
    (log, format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default()))
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

    let (log, path) = fake_ssh(&local, &remote);
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

    // `kraal agents` lists them too, after the server's name, with their id there.
    let listed = local.run(&["agents"]);
    assert!(listed.lines().any(|l| l.contains("gpu › w") && l.contains("api") && l.contains("working")), "{listed}");
    let agents: serde_json::Value = serde_json::from_str(&local.run(&["agents", "--json"])).unwrap();
    let there = agents.as_array().unwrap().iter().find(|a| a["server"] == "gpu").cloned().unwrap();
    assert_eq!((there["session"].as_str(), there["kind"].as_str()), (Some("rremote"), Some("remote")), "{there}");
    assert_eq!((there["id"].to_string(), there["agent"].as_str()), (api.clone(), Some("claude")), "{there}");
    // What another kraal watching this one reads lists this session's own agents only.
    let (mut follow, lines) = local.follow(&["agents", "--follow"]);
    assert_eq!(lines.recv_timeout(Duration::from_secs(10)).expect("a list").trim(), "[]");
    let _ = follow.kill();
    let _ = follow.wait();

    // Blocked there: told and announced here, as a local agent is.
    let (mut events, notified) = local.follow(&["events", "notification", "agent.*"]);
    claude_hook(&remote, "api", r#"{"hook_event_name":"Notification","message":"Claude needs your permission"}"#);
    let mut seen = std::collections::HashMap::new();
    while seen.len() < 2 {
        let line = notified.recv_timeout(Duration::from_secs(10)).expect("an event");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        seen.insert(v["type"].as_str().unwrap().to_string(), v);
    }
    let v = &seen["notification"];
    let said = (v["title"].as_str(), v["body"].as_str());
    assert_eq!(said, (Some("claude · api on gpu"), Some("needs your input")), "{v}");
    let v = &seen["agent.blocked"];
    assert_eq!(
        (v["server"].as_str(), v["session"].as_str(), v["pane"].to_string()),
        (Some("gpu"), Some("rremote"), api.clone())
    );
    assert_eq!(
        (v["name"].as_str(), v["agent"].as_str(), v["previous"].as_str()),
        (Some("api"), Some("claude"), Some("working"))
    );
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
    // The tab shows another pane there; again: the tab already attached is used, and the
    // client in it asked to show the agent's pane.
    remote.run(&["focus", "other"]);
    let start = Instant::now();
    while remote_api()["focused"] != false {
        assert!(start.elapsed() < Duration::from_secs(10), "{}", remote_api());
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(500));
    client.send(b"\x1ba");
    let start = Instant::now();
    while remote_api()["focused"] != true {
        assert!(start.elapsed() < Duration::from_secs(10), "the agent's pane is not shown again: {}", remote_api());
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!ssh_log().contains(" focus "), "asked through the tab, not over another connection: {}", ssh_log());
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

    // A configuration that cannot be read does not say that servers may be added: none is,
    // though the attach works.
    let broken = local.root.join("broken");
    std::fs::create_dir_all(broken.join("kraal")).unwrap();
    std::fs::copy(&inventory, broken.join("kraal/servers.toml")).unwrap();
    std::fs::write(broken.join("kraal/config.toml"), "[servers]\nauto_add = false\nmouse = [\n").unwrap();
    let before = std::fs::read_to_string(broken.join("kraal/servers.toml")).unwrap();
    let config = format!("XDG_CONFIG_HOME={}", broken.display());
    let args = ["run", "-n", "new", "--", "env", "-u", "KRAAL_PANE", &config, kraal, "-s", "rremote", "attach"];
    local.run(&[&args[..], &["--host", "new.lan"]].concat());
    local.wait_for_text("new", "api");
    std::thread::sleep(Duration::from_millis(500));
    assert!(ssh_log().contains("new.lan"));
    assert_eq!(std::fs::read_to_string(broken.join("kraal/servers.toml")).unwrap(), before);
    drop(client);
}

/// A tab attached to a server's session by hand is the one used, and each agent waiting there is
/// shown in it in turn: in the tab, whatever another terminal attached to that session shows.
#[cfg(unix)]
#[test]
fn a_tab_attached_to_a_server_shows_each_agent_there_in_turn() {
    let local = Env::new("tlocal");
    let remote = Env::new("tremote");
    // Two workspaces there, an agent shown in each, both waiting for an answer.
    remote.workspace("one", "layout {\n    pane name=\"api\"\n}\n");
    remote.workspace("two", "layout {\n    pane name=\"web\"\n}\n");
    remote.run(&["open", "one"]);
    remote.run(&["open", "two"]);
    for (pane, n, word) in [("api", 1, "ONE"), ("web", 2, "TWO")] {
        remote.run(&["send", "-p", pane, "--enter", &format!("echo IN-$(({n}))-{word}")]);
        remote.wait_for_text(pane, &format!("IN-{n}-{word}"));
        for event in ["SessionStart", "UserPromptSubmit", "Notification"] {
            let payload = format!(
                r#"{{"hook_event_name":"{event}","session_id":"s-{n}","message":"Claude needs your permission"}}"#
            );
            claude_hook(&remote, pane, &payload);
        }
    }
    let (_log, path) = fake_ssh(&local, &remote);
    std::fs::write(
        local.root.join("config/kraal/servers.toml"),
        "[[server]]\nname = \"gpu\"\nhost = \"gpu.lan\"\nagents = \"tremote\"\n",
    )
    .unwrap();
    local.workspace("w", "layout {\n    pane name=\"main\"\n}\n");
    let out = local.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    // Attached by hand, with a command line of its own.
    let kraal = env!("CARGO_BIN_EXE_kraal");
    local.run(&[
        "run",
        "--tab",
        "-n",
        "by-hand",
        "--",
        "env",
        "-u",
        "KRAAL_PANE",
        kraal,
        "-s",
        "tremote",
        "attach",
        "--host",
        "gpu",
    ]);
    local.wait_for_text("by-hand", "IN-1-ONE");
    let mut client = RawClient::attach(&local, "w");
    client.wait_for_screen("gpu › ");
    client.wait_for_screen("blocked · claude · two");
    // Another terminal attached there, used last: what that session is asked without saying
    // which client goes to that one.
    let mut other = RawClient::attach(&remote, "one");
    std::thread::sleep(Duration::from_millis(500));
    let shown = |word: &str, not: &str| {
        let start = Instant::now();
        loop {
            let screen = local.run(&["read", "-p", "by-hand"]);
            if screen.contains(word) && !screen.contains(not) {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "{word} is not what the tab shows:\n{screen}");
            std::thread::sleep(Duration::from_millis(50));
        }
    };
    // The client here looks at that tab (opened last, it is the one its workspace shows), which
    // shows the agent of `one`: that one is seen already, so alt+a goes to the other, then back.
    // Each time, the tab must come to show what it did not show before alt+a.
    let panes: serde_json::Value = serde_json::from_str(&local.run(&["panes", "--json"])).unwrap();
    let tab = panes.as_array().unwrap().iter().find(|p| p["name"] == "by-hand").cloned().unwrap();
    assert_eq!(tab["focused"], true, "{tab}");
    for (word, not) in [("IN-2-TWO", "IN-1-ONE"), ("IN-1-ONE", "IN-2-TWO"), ("IN-2-TWO", "IN-1-ONE")] {
        shown(not, word);
        // The terminal there gained the focus: it is the one used last there.
        other.send(b"\x1b[I");
        std::thread::sleep(Duration::from_millis(200));
        client.send(b"\x1ba");
        shown(word, not);
    }
    let panes: serde_json::Value = serde_json::from_str(&local.run(&["panes", "--json"])).unwrap();
    assert_eq!(panes.as_array().unwrap().len(), 2, "no other tab attached: {panes}");
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

/// After an update, `kraal update` names the sessions still on an older kraal with what a
/// restart stops in them, asks on a terminal (no by default when programs would stop), and has
/// the new kraal restart them when told to.
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

    // Without a terminal to ask on, and without --restart: said, not done.
    install();
    let out = update(&[]).output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{said}{}", String::from_utf8_lossy(&out.stderr));
    assert!(said.contains(&format!("Session updrestart still runs kraal {version}.")), "{said}");
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
    newer.wait_for_message(&format!(r#"{{"type":"attached","version":"{version}","features":["#));
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

/// The restart in the menu says what stops and asks first; then the session comes back, and
/// attached clients are told to come back to the workspace they showed.
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
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_message("\"attached\"");
    client.send(b"\x02Wr");
    client.wait_for_screen("Restart session menurestart on the kraal installed now?");
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
}

/// `ctrl+b q` asks first, Cancel selected, saying what stops (a short question when nothing
/// would); a yes saves the layout and stops the session, every attached client is told how it
/// comes back, and the next start brings the layout back.
#[test]
fn the_menu_quits_after_asking_and_keeps_the_layout() {
    let env = Env::new("menuquit");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    let pid = env.server_pid().expect("the server runs");
    let mut client = RawClient::attach(&env, "w");
    let other = RawClient::attach(&env, "w");
    client.wait_for_message("\"attached\"");
    other.wait_for_message("\"attached\"");
    client.send(b"\x02");
    client.wait_for_row("d detach · q quit");
    client.send(b"q");
    let rows = client.wait_for_row("Quit kraal? The layout is saved for next time.");
    assert!(!rows.iter().any(|r| r.contains("These programs stop")), "a shell at its prompt:\n{}", rows.join("\n"));
    client.send(b"\r");
    client.wait_for_rows("the question gone", |rows| !rows.iter().any(|r| r.contains("Quit kraal?")));
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(env.server_pid(), Some(pid), "Enter takes Cancel, selected first");

    env.run(&["run", "--name", "job", "--", "sh", "-c", "echo started; exec sleep 4249"]);
    env.wait_for_text("job", "started");
    client.send(b"\x02q");
    client.wait_for_row("Quit kraal? These programs stop:");
    let rows = client.wait_for_row("The layout is saved for next time.");
    let listed = rows.iter().any(|r| r.contains("sleep 4249 (pane ") && r.contains("\"job\" in w)"));
    assert!(listed, "{}", rows.join("\n"));
    client.send(b"y");
    let said = "stopped; kraal -s menuquit starts it again with its layout";
    client.wait_for_message(said);
    other.wait_for_message(said);
    wait_until(Duration::from_secs(10), "the server still runs", || env.server_pid().is_none());
    env.run(&["open", "w"]);
    let panes = env.run(&["panes"]);
    assert!(panes.contains("shell") && panes.contains("job"), "{panes}");
}

/// A quit stops the session's programs only once its layout is saved: when it cannot be, the
/// session keeps running, and the quit says why.
#[test]
fn a_quit_that_cannot_save_the_session_leaves_it_running() {
    use std::os::unix::fs::PermissionsExt;
    let env = Env::new("unsavedquit");
    env.workspace("w", "layout {\n    pane name=\"one\"\n}\n");
    env.run(&["open", "w"]);
    wait_until(Duration::from_secs(10), "the layout was never saved", || env.state_file().exists());
    let pid = env.server_pid().expect("the server runs");
    let sessions = env.root.join("state/kraal/sessions");
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o500)).unwrap();
    // A pane opened since the last save: the layout on disk is not the session's any more.
    env.run(&["run", "--name", "two", "--target", "one", "--", "sh"]);
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_message("\"attached\"");
    client.send(b"\x02q");
    client.wait_for_row("Quit kraal?");
    client.send(b"y");
    client.wait_for_rows("the question gone", |rows| !rows.iter().any(|r| r.contains("Quit kraal?")));
    client.send(b"\x02Vj");
    client.wait_for_row("not quitting: the layout could not be saved");
    std::fs::set_permissions(&sessions, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(env.server_pid(), Some(pid), "the session stopped");
    assert!(!client.messages.lock().unwrap().iter().any(|m| m.contains(r#""type":"exit""#)));
}

/// `exit` in the last shell of the last workspace ends the session, as in tmux and Zellij:
/// without a question, every attached terminal leaves, and nothing is left to restore.
#[cfg(target_os = "linux")]
#[test]
fn the_session_ends_with_its_last_pane() {
    let env = Env::new("lastexit");
    env.workspace("w", "layout {\n    pane name=\"only\"\n}\n");
    env.run(&["open", "w"]);
    wait_until(Duration::from_secs(10), "the layout was never saved", || env.state_file().exists());
    let client = RawClient::attach(&env, "w");
    client.wait_for_message("\"attached\"");
    let mut terminal = env.client();
    env.run(&["send", "-p", "only", "--enter", "exit"]);
    client.wait_for_message("the last pane closed, so kraal stopped");
    wait_until(Duration::from_secs(10), "the attached terminal stayed", || terminal.try_wait().unwrap().is_some());
    wait_until(Duration::from_secs(10), "the server still runs", || env.server_pid().is_none());
    assert!(!env.state_file().exists(), "an ended session was kept");
}

/// A command pane stays open when its program ends, to show how it ended: it has not ended, and
/// the session goes on until it is closed.
#[test]
fn a_command_pane_left_open_keeps_its_session() {
    let env = Env::new("keptopen");
    env.workspace("w", "layout {\n    pane name=\"job\" command=\"sh\" { args \"-c\" \"sleep 0.3; exit 3\"; }\n}\n");
    env.run(&["open", "w"]);
    let pid = env.server_pid().expect("the server runs");
    let client = RawClient::attach(&env, "w");
    client.wait_for_message("\"attached\"");
    env.run(&["wait", "-p", "job", "--until", "exit", "--timeout", "10"]);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(env.server_pid(), Some(pid), "the session ended");
    assert!(!client.messages.lock().unwrap().iter().any(|m| m.contains(r#""type":"exit""#)));
    env.run(&["close", "job"]);
    client.wait_for_message("the last pane closed, so kraal stopped");
}

/// `kraal restart` says what would stop before asking; a client attached in a terminal waits for
/// the next server and comes back to its workspace; an idle shell comes back; and a restart run
/// from a pane of the session itself outlives that pane.
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

    // Without a terminal to ask on: what stops is said, and nothing happens.
    let out = env.try_run(&["restart"]);
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "{said}");
    let listed = said.split_once("These programs stop:\n").map(|(_, rest)| rest).unwrap_or_default();
    assert!(listed.starts_with("  sleep 4245  (pane ") && listed.contains(" \"busy\" in w)"), "{said}");
    assert!(!said.contains("idle"), "a shell at its prompt runs nothing: {said}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("add --yes"));
    assert_eq!(env.server_pid(), Some(pid));
    // On a terminal: the list, then the question, whose Enter is no.
    let screen = in_terminal(env.cmd(&["restart"]), b"\n");
    let (listed, asked) = (screen.find("sleep 4245"), screen.find("Restart session restart? [y/N]"));
    assert!(listed.is_some() && asked.is_some() && listed < asked, "{screen}");
    assert!(screen.contains("cancelled"), "{screen}");
    assert_eq!(env.server_pid(), Some(pid));

    let out = env.run(&["restart", "--yes"]);
    assert!(out.contains("restarted session restart"), "{out}");
    let second = env.server_pid().expect("a server runs again");
    assert_ne!(second, pid);
    // The idle shell is back, with what it showed, and takes commands.
    env.wait_for_text("idle", "before-restart");
    env.run(&["send", "-p", "idle", "--enter", "echo after-restart"]);
    env.wait_for_text_since_restore("idle", "after-restart");
    // The client did not leave: it attached to the next server.
    assert!(client.try_wait().unwrap().is_none(), "the client left");
    attached();

    // From a pane of the session: the restart closes that pane, and happens all the same.
    let line = format!("'{}' restart --yes", env!("CARGO_BIN_EXE_kraal"));
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

/// A restart run in place of the shell of the session's last pane (`exec`): the command stays
/// until the restart has the session, which would otherwise end with that pane, its layout
/// deleted, and the session comes back.
#[cfg(target_os = "linux")]
#[test]
fn a_restart_from_the_last_pane_outlives_it() {
    let env = Env::new("lastpane");
    env.workspace("w", "layout {\n    pane name=\"only\"\n}\n");
    env.run(&["open", "w"]);
    let pidfile = env.root.join("pane.pid");
    env.run(&["send", "-p", "only", "--enter", &format!("echo $$ > '{}'", pidfile.display())]);
    let mut pane = PathBuf::new();
    wait_until(Duration::from_secs(10), "the shell never said its pid", || {
        let pid = std::fs::read_to_string(&pidfile).unwrap_or_default();
        pane = PathBuf::from(format!("/proc/{}", pid.trim()));
        pid.ends_with('\n')
    });
    let cmdline = |process: &Path| std::fs::read(process.join("cmdline")).unwrap_or_default();
    let kraal = env!("CARGO_BIN_EXE_kraal");
    let go = env.root.join("go");
    let line = format!("while [ ! -e '{}' ]; do sleep 0.1; done; exec '{kraal}' restart --yes", go.display());
    env.run(&["send", "-p", "only", "--enter", &line]);
    // With the server stopped, the restart waits for it, and nothing can end the command in the
    // pane meanwhile but the command itself.
    let server = env.server_pid().expect("the server runs");
    let stopped = Stopped::stop(server);
    std::fs::write(&go, "").unwrap();
    let helper = [kraal.as_bytes(), b"\0-s\0lastpane\0restart\0--yes\0"].concat();
    let started = || std::fs::read_dir("/proc").unwrap().flatten().any(|e| cmdline(&e.path()) == helper);
    wait_until(Duration::from_secs(20), "the restart never started", started);
    let command = [kraal.as_bytes(), b"\0restart\0--yes\0"].concat();
    for _ in 0..30 {
        assert_eq!(cmdline(&pane), command, "the command left the pane before the restart had the session");
        std::thread::sleep(Duration::from_millis(100));
    }
    drop(stopped);
    wait_until(Duration::from_secs(30), "the session did not come back", || {
        env.server_pid().is_some_and(|p| p != server)
    });
    assert!(env.run(&["panes"]).contains("only"));
}

/// `kraal restart` run in a pane of the session says what stops as it does from anywhere else:
/// the jobs in the background of its own pane too. Only the command itself and what runs it (a
/// script) are not programs that stop, in the foreground or in the background.
#[cfg(target_os = "linux")]
#[test]
fn a_restart_from_a_pane_says_what_stops_in_that_pane_too() {
    let env = Env::new("ownjobs");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    let pid = env.server_pid().expect("the server runs");
    let running = |argv: &[u8]| {
        std::fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .any(|e| std::fs::read(e.path().join("cmdline")).is_ok_and(|c| c == argv))
    };
    env.run(&["send", "-p", "shell", "--enter", "sleep 4251 &"]);
    wait_until(Duration::from_secs(10), "the job never started", || running(b"sleep\x004251\x00"));
    // Without a terminal to ask on, the command says what stops and goes no further.
    let said = env.root.join("said");
    let restart = format!("'{}' restart </dev/null >'{}' 2>&1", env!("CARGO_BIN_EXE_kraal"), said.display());
    let script = env.root.join("restart.sh");
    write_script(&script, &format!("#!/bin/sh\n{restart}\n"));
    let stops = |typed: &str| -> Vec<String> {
        let _ = std::fs::remove_file(&said);
        env.run(&["send", "-p", "shell", "--enter", typed]);
        let mut text = String::new();
        wait_until(Duration::from_secs(10), "the restart did not stop at what it would stop", || {
            text = std::fs::read_to_string(&said).unwrap_or_default();
            text.contains("add --yes")
        });
        text.lines().filter_map(|line| line.strip_prefix("  ")).map(String::from).collect()
    };
    // The pane's shell may have its own standard input elsewhere while the command runs: dash
    // does that for a command given `< file`, and this line does it in any shell. The jobs are
    // those of the shell's terminal all the same.
    let elsewhere = |command: &str| format!("exec 3<&0 </dev/null; {command}; exec <&3 3<&-");
    let typed = [
        restart.clone(),
        format!("sh '{}'", script.display()),
        format!("{restart} &"),
        elsewhere(&format!("'{}' restart >'{}' 2>&1", env!("CARGO_BIN_EXE_kraal"), said.display())),
    ];
    for typed in typed {
        let stops = stops(&typed);
        assert_eq!(stops.len(), 1, "{typed}: {stops:?}");
        assert!(stops[0].starts_with("sleep 4251 (in the background)  (pane "), "{typed}: {stops:?}");
    }
    // The same list as from outside the session.
    let outside = || {
        let out = env.try_run(&["restart"]);
        assert!(!out.status.success());
        let said = String::from_utf8_lossy(&out.stdout).into_owned();
        said.lines().filter_map(|line| line.strip_prefix("  ")).map(String::from).collect::<Vec<_>>()
    };
    let stops = outside();
    assert_eq!(stops.len(), 1, "{stops:?}");
    assert!(stops[0].starts_with("sleep 4251 (in the background)  (pane "), "{stops:?}");
    // And from outside while that shell runs a program with its standard input elsewhere.
    env.run(&["send", "-p", "shell", "--enter", &elsewhere("sleep 4253")]);
    wait_until(Duration::from_secs(10), "the program never started", || running(b"sleep\x004253\x00"));
    let stops = outside();
    assert_eq!(stops.len(), 2, "{stops:?}");
    assert!(stops[0].starts_with("sleep 4253  (pane "), "{stops:?}");
    assert!(stops[1].starts_with("sleep 4251 (in the background)  (pane "), "{stops:?}");
    assert_eq!(env.server_pid(), Some(pid));
    assert!(running(b"sleep\x004251\x00"));
}

/// A job left in the background of a shell stops with a restart (the terminal hangs up): it is
/// said with what stops, and the restart needs a yes for it. What the shell runs off its
/// terminal (a prompt's daemon) is not the user's job.
#[cfg(target_os = "linux")]
#[test]
fn a_restart_says_it_stops_the_jobs_in_the_background() {
    let env = Env::new("bgjobs");
    env.workspace("w", "layout {\n    pane name=\"shell\"\n}\n");
    env.run(&["open", "w"]);
    let pid = env.server_pid().expect("the server runs");
    let jobs = "sleep 4247 & (exec sleep 4248 </dev/null >/dev/null 2>&1) &";
    env.run(&["send", "-p", "shell", "--enter", jobs]);
    let running = |argv: &[u8]| {
        std::fs::read_dir("/proc")
            .unwrap()
            .flatten()
            .any(|e| std::fs::read(e.path().join("cmdline")).is_ok_and(|c| c == argv))
    };
    wait_until(Duration::from_secs(10), "the jobs never started", || {
        running(b"sleep\x004247\x00") && running(b"sleep\x004248\x00")
    });
    let out = env.try_run(&["restart"]);
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "{said}");
    assert!(said.contains("These programs stop:\n  sleep 4247 (in the background)  (pane "), "{said}");
    assert!(!said.contains("4248"), "{said}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("add --yes"));
    assert_eq!(env.server_pid(), Some(pid));
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
    if let Some(dir) = cmd.get_current_dir() {
        c.current_dir(dir);
    }
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

/// A throwaway sshd on 127.0.0.1 (port 2305 unless said otherwise) with its own host and user
/// keys, and an `ssh` that uses only them (never ~/.ssh): a server reached for real. `None` where
/// no sshd can run here.
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
        Sshd::start_on(dir, 2305)
    }

    fn start_on(dir: &Path, port: u16) -> Option<Sshd> {
        use std::os::fd::AsRawFd;
        let sshd = ["/usr/sbin/sshd", "/usr/bin/sshd"].into_iter().map(PathBuf::from).find(|p| p.exists())?;
        let ssh =
            ["/usr/bin/ssh", "/bin/ssh", "/usr/local/bin/ssh"].into_iter().map(PathBuf::from).find(|p| p.exists())?;
        std::fs::create_dir_all(dir).unwrap();
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(format!("/tmp/kraal-test-sshd-{port}.lock"))
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
                "Port {port}\nListenAddress 127.0.0.1\nHostKey {d}/host_key\nPidFile {d}/sshd.pid\n\
                 AuthorizedKeysFile {d}/authorized_keys\nStrictModes no\nUsePAM no\nPasswordAuthentication no\n\
                 KbdInteractiveAuthentication no\nPubkeyAuthentication yes\nAllowUsers {}\n",
                user.trim()
            ),
        )
        .unwrap();
        let host_key = std::fs::read_to_string(dir.join("host_key.pub")).unwrap();
        std::fs::write(dir.join("known_hosts"), format!("[127.0.0.1]:{port} {host_key}")).unwrap();
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
        // kraal gives a server a few seconds to answer. This one is on this machine, as slow as
        // the tests beside it make it, and that limit is not what its tests are about: its `ssh`
        // waits as long as they do (of an option given twice, ssh takes the first).
        write_script(
            &bin.join("ssh"),
            &format!(
                "#!/bin/sh\nexec '{}' -F '{d}/ssh_config' -o ConnectTimeout={} \"$@\"\n",
                ssh.display(),
                PATIENCE.as_secs()
            ),
        );
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
            if sshd.child.try_wait().ok().flatten().is_some() || start.elapsed() > PATIENCE {
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
    let kraal =
        |args: &[&str]| env.cmd(args).env("PATH", &path).env("EDITOR", &editor).env_remove("VISUAL").output().unwrap();
    let wait_until = |what: &str, ok: &dyn Fn() -> bool| {
        let start = Instant::now();
        while !ok() {
            assert!(start.elapsed() < PATIENCE, "timed out waiting: {what}");
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

/// `kraal buffer set` with `text` on its standard input.
fn set_buffer(env: &Env, args: &[&str], text: &str) -> Output {
    use std::io::Write;
    let mut child = env
        .cmd(&[&["buffer", "set"], args].concat())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(text.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

fn buffer_list(env: &Env) -> Vec<serde_json::Value> {
    let list: serde_json::Value = serde_json::from_str(&env.run(&["buffer", "list", "--json"])).unwrap();
    list.as_array().unwrap().clone()
}

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
    let (buffers, sessions) = (env.root.join("state/kraal/buffers"), env.root.join("state/kraal/sessions"));
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
    // Copy mode: up to the line, select it, copy. It goes to the terminal and to the buffers.
    for key in [&b"\x02v"[..], b"k", b"V", b"y"] {
        client.send(key);
        std::thread::sleep(Duration::from_millis(150));
    }
    client.wait_for_screen("\x1b]52;c;");
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
    client.send(b"\x1b");
    std::thread::sleep(Duration::from_millis(150));
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

/// A session, a real sshd (on its own port) as the server `box`, and a local editor told what
/// to do, for the tests of editing a server's file. The commands kraal runs there find a `sudo`
/// that takes the password `pass word` (and runs them as this user), and wait 2 seconds once
/// the file `slow` exists.
#[cfg(target_os = "linux")]
struct EditBench {
    env: Env,
    _sshd: Sshd,
    path: String,
    editor: PathBuf,
    srv: PathBuf,
}

#[cfg(target_os = "linux")]
impl EditBench {
    fn new(name: &str, port: u16) -> Option<EditBench> {
        let env = Env::new(name);
        let sshd = Sshd::start_on(&env.root.join("sshd"), port)?;
        let user = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout).unwrap();
        std::fs::write(
            env.root.join("config/kraal/servers.toml"),
            format!("[[server]]\nname = \"box\"\nhost = \"127.0.0.1\"\nport = {port}\nuser = \"{}\"\n", user.trim()),
        )
        .unwrap();
        let cfg = env.root.join("config/kraal/config.toml");
        let mut text = std::fs::read_to_string(&cfg).unwrap();
        let downloads = env.root.join("downloads");
        text.push_str(&format!("\n[servers]\nmultiplex = false\ndownload_dir = \"{}\"\n", downloads.display()));
        std::fs::write(&cfg, text).unwrap();
        let root = env.root.display().to_string();
        let fake = env.root.join("server-bin");
        std::fs::create_dir_all(&fake).unwrap();
        write_script(
            &fake.join("sudo"),
            "#!/bin/sh\nn=\nwhile [ $# -gt 0 ]; do case $1 in -n) n=1 ;; -p) shift ;; --) shift; break ;; -*) ;; *) break ;; esac; shift; done\n\
             if [ -n \"$n\" ]; then echo 'sudo: a password is required' >&2; exit 1; fi\n\
             IFS= read -r pw\nif [ \"$pw\" != 'pass word' ]; then echo 'Sorry, try again.' >&2; exit 1; fi\nexec \"$@\"\n",
        );
        // In front of the test's ssh: what runs there finds that sudo first.
        let bin = env.root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        write_script(
            &bin.join("ssh"),
            &format!(
                "#!/bin/sh\nif [ -e '{root}/slow' ]; then rm -f '{root}/slow'; sleep 2; fi\nn=$#; i=0\nfor a; do\n  i=$((i + 1))\n  \
                 if [ \"$i\" -eq \"$n\" ]; then set -- \"$@\" \"env PATH={}:/usr/local/bin:/usr/bin:/bin $a\"; else set -- \"$@\" \"$a\"; fi\n\
                 done\nshift \"$n\"\nexec '{}/ssh' \"$@\"\n",
                fake.display(),
                sshd.bin.display()
            ),
        );
        let editor = env.root.join("editor");
        write_script(
            &editor,
            &format!(
                "#!/bin/sh\nfor f; do :; done\nprintf '%s' \"$f\" > '{root}/editing.tmp' && mv '{root}/editing.tmp' '{root}/editing'\n\
                 n=0\nwhile :; do\n  if [ -e '{root}/step'$n ]; then . '{root}/step'$n; n=$((n + 1)); fi\n  \
                 if [ -e '{root}/quit' ]; then rm -f '{root}/quit' '{root}/editing' '{root}/step'*; exit 0; fi\n  sleep 0.05\ndone\n"
            ),
        );
        let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
        let srv = env.root.join("srv");
        std::fs::create_dir_all(&srv).unwrap();
        env.workspace("w", "layout {\n    pane name=\"sh\"\n}\n");
        let bench = EditBench { env, _sshd: sshd, path, editor, srv };
        let out = bench.kraal(&["open", "w"]).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        Some(bench)
    }

    fn kraal(&self, args: &[&str]) -> Command {
        let mut c = self.env.cmd(args);
        c.env("PATH", &self.path).env("EDITOR", &self.editor).env_remove("VISUAL");
        c
    }

    fn wait_until(&self, what: &str, ok: &dyn Fn() -> bool) {
        let start = Instant::now();
        while !ok() {
            assert!(start.elapsed() < PATIENCE, "timed out waiting: {what}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The editor's copy, once the editor runs.
    fn editing(&self) -> PathBuf {
        let editing = self.env.root.join("editing");
        self.wait_until("the editor starts", &|| editing.exists());
        PathBuf::from(read(&editing))
    }

    /// Have the editor run `script` (`$f` is the file) as its step `n`.
    fn step(&self, n: u32, script: &str) {
        let tmp = self.env.root.join("step.tmp");
        std::fs::write(&tmp, script).unwrap();
        std::fs::rename(&tmp, self.env.root.join(format!("step{n}"))).unwrap();
    }

    /// End the editor: its private copy goes.
    fn quit(&self, local: &Path) {
        std::fs::write(self.env.root.join("quit"), "").unwrap();
        let dir = local.parent().unwrap().to_path_buf();
        self.wait_until("the private copy is removed", &|| !dir.exists());
    }
}

#[cfg(target_os = "linux")]
fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

/// `kraal edit --sudo`: a pane asks for the password, sudo gets it, and each save is written as
/// root, through the same checks; a wrong password opens nothing, and a file one may not write
/// is refused with the way to edit it.
#[cfg(target_os = "linux")]
#[test]
fn a_server_file_is_edited_as_root_through_sudo() {
    use std::os::unix::fs::PermissionsExt;
    let Some(b) = EditBench::new("rsudo", 2311) else {
        eprintln!("skipped: no sshd can run here");
        return;
    };
    let mut client = RawClient::attach(&b.env, "w");
    std::thread::sleep(Duration::from_millis(500));
    // Named like the FIFO the password goes through, in the same private directory.
    let file = b.srv.join("sudo");
    std::fs::write(&file, "v1\n").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
    let target = format!("box:{}", file.display());
    let piped = std::process::Stdio::piped;
    // Its pane says so (once it is there).
    let asked = || {
        let out = b.env.try_run(&["read", "-p", "sudo · box"]);
        out.status.success() && String::from_utf8_lossy(&out.stdout).contains("password on box")
    };
    let cli = b.kraal(&["edit", "--sudo", &target]).stdout(piped()).stderr(piped()).spawn().unwrap();
    b.wait_until("the password is asked in its pane", &asked);
    assert!(!b.env.root.join("editing").exists(), "nothing opens before the password");
    client.send(b"pass word\r");
    let local = b.editing();
    let out = cli.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(read(&local), "v1\n");
    b.step(0, "printf 'v2\\n' > \"$f\"");
    b.wait_until("the save reaches the server", &|| read(&file) == "v2\n");
    assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o7777, 0o640, "the mode is kept");
    b.step(1, "printf 'v3\\n' > \"$f\"");
    b.wait_until("the next save, through the same sudo", &|| read(&file) == "v3\n");
    b.quit(&local);
    let names: Vec<String> =
        std::fs::read_dir(&b.srv).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(names, vec!["sudo".to_string()], "no temporary file or lock is left");

    // A wrong password: refused, nothing opens.
    let cli = b.kraal(&["edit", "--sudo", &target]).stdout(piped()).stderr(piped()).spawn().unwrap();
    b.wait_until("the password is asked in its pane", &asked);
    client.send(b"wrong\r");
    let out = cli.wait_with_output().unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("sudo refused"), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!b.env.root.join("editing").exists());

    // Without --sudo, a file one may not write is refused, saying how to edit it.
    let ro = b.srv.join("ro.conf");
    std::fs::write(&ro, "x\n").unwrap();
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o444)).unwrap();
    let out = b.kraal(&["edit", &format!("box:{}", ro.display())]).output().unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("kraal edit --sudo"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// "Keep the version of the server" while the editor saves again before that version arrived:
/// the new save is not overwritten, and the question comes back.
#[cfg(target_os = "linux")]
#[test]
fn keeping_the_servers_version_never_drops_a_later_save() {
    let Some(b) = EditBench::new("rkeep", 2311) else {
        eprintln!("skipped: no sshd can run here");
        return;
    };
    let mut client = RawClient::attach(&b.env, "w");
    std::thread::sleep(Duration::from_millis(500));
    let file = b.srv.join("theirs.txt");
    std::fs::write(&file, "v1\n").unwrap();
    let out = b.kraal(&["edit", &format!("box:{}", file.display())]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let local = b.editing();
    std::fs::write(&file, "theirs\n").unwrap();
    client.screen.lock().unwrap().clear();
    b.step(0, "printf 'mine\\n' > \"$f\"");
    client.wait_for_screen("Keep the version of box");
    // Reading the server's version takes a while; the user saves again meanwhile.
    std::fs::write(b.env.root.join("slow"), "").unwrap();
    client.send(b"\x1b[B");
    std::thread::sleep(Duration::from_millis(100));
    client.screen.lock().unwrap().clear();
    client.send(b"\r");
    b.step(1, "printf 'mine2\\n' > \"$f\"");
    b.wait_until("the second save", &|| read(&local) == "mine2\n");
    // Once the server's version arrived: asked again, the later save still there.
    client.wait_for_screen("Keep your version as a local file");
    assert_eq!(read(&local), "mine2\n", "the later save is still there");
    client.send(b"\r");
    let kept = b.env.root.join("downloads/theirs.txt");
    b.wait_until("the later save is kept", &|| read(&kept) == "mine2\n");
    assert_eq!(read(&file), "theirs\n");
    b.quit(&local);
}

/// Opening the worktree actions of a shell that went into a worktree never waits for git on the
/// server's loop: what git said about the pane's directory is enough.
#[cfg(target_os = "linux")]
#[test]
fn worktree_actions_never_wait_for_git() {
    let env = Env::new("wtslow");
    let repo = env.root.join("app");
    let _git = git_repo(&repo, &[("a.txt", "1\n")]);
    let out = env.cmd(&["wt", "new", "x", "--no-setup"]).current_dir(&repo).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let wt = env.root.join("app.worktrees/x");
    // A git slow to tell a worktree's base, once `slow` exists.
    let real = Command::new("sh").arg("-c").arg("command -v git").output().unwrap();
    let real = String::from_utf8(real.stdout).unwrap();
    let bin = env.root.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let slow = env.root.join("slow");
    write_script(
        &bin.join("git"),
        &format!(
            "#!/bin/sh\ncase \"$*\" in *kraalbase*) if [ -e '{}' ]; then sleep 5; fi ;; esac\nexec '{}' \"$@\"\n",
            slow.display(),
            real.trim()
        ),
    );
    env.workspace("w", &format!("layout {{\n    pane name=\"sh\" cwd={:?}\n}}\n", wt.display().to_string()));
    let path = format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default());
    let out = env.cmd(&["open", "w"]).env("PATH", &path).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mut client = RawClient::attach(&env, "w");
    client.wait_for_screen("kraal/x");
    std::fs::write(&slow, "").unwrap();
    client.send(b"\x02Rg");
    std::thread::sleep(Duration::from_millis(150));
    let start = Instant::now();
    env.run(&["panes"]);
    let waited = start.elapsed();
    std::fs::remove_file(&slow).unwrap();
    assert!(waited < Duration::from_millis(1000), "the server waited {waited:?} for git");
    client.wait_for_screen("Worktree x");
}

/// A pane opened from the command line in a worktree the session has not seen yet gets its
/// variables (number, port offset, port).
#[test]
fn a_pane_opened_in_an_unseen_worktree_gets_its_variables() {
    let env = Env::new("wtvars");
    let repo = env.root.join("app");
    let _git = git_repo(&repo, &[("a.txt", "1\n")]);
    let config = env.root.join("config/kraal/config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("\n[worktrees]\nport_base = 3000\n");
    std::fs::write(&config, text).unwrap();
    for name in ["x", "y"] {
        let out = env.cmd(&["wt", "new", name, "--no-setup"]).current_dir(&repo).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    }
    let wt = env.root.join("app.worktrees/x");
    env.workspace("w", "layout {\n    pane name=\"sh\"\n}\n");
    env.run(&["open", "w"]);
    let probe = "echo \"idx=$KRAAL_WORKTREE_INDEX offset=$KRAAL_PORT_OFFSET port=$PORT\"; exec sleep 60";
    env.run(&["run", "--name", "probe", "--cwd", &wt.display().to_string(), "--", "sh", "-c", probe]);
    env.wait_for_text("probe", "idx=1 offset=10 port=3010");
    // Reached through a symbolic link (as /var is /private/var on macOS), while git gives the
    // worktree's path with its links resolved.
    std::os::unix::fs::symlink(&env.root, env.root.join("link")).unwrap();
    let through = env.root.join("link/app.worktrees/y");
    env.run(&["run", "--name", "linked", "--cwd", &through.display().to_string(), "--", "sh", "-c", probe]);
    env.wait_for_text("linked", "idx=2 offset=20 port=3020");
}

/// `kraal wt prune` removes a branch merged by squash (its commits are not in the base's
/// history, their changes are), keeps one whose ignored files are its own unless told to discard
/// them, and `--no-setup` makes a worktree even when the repository's setup file is broken.
#[test]
fn merged_worktrees_are_pruned_however_they_were_merged() {
    let env = Env::new("wtsquash");
    let repo = env.root.join("app");
    let git = git_repo(&repo, &[(".gitignore", "*.db\n"), ("a.txt", "1\n")]);
    let wt = |args: &[&str]| env.cmd(&[&["wt"], args].concat()).current_dir(&repo).output().unwrap();
    let text =
        |out: &Output| format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    let dir = env.root.join("app.worktrees");
    for name in ["sq", "data"] {
        assert!(wt(&["new", name, "--no-setup"]).status.success());
    }
    let in_sq = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir.join("sq"))
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    };
    std::fs::write(dir.join("sq/a.txt"), "2\n").unwrap();
    in_sq(&["commit", "-q", "-am", "two"]);
    std::fs::write(dir.join("sq/b.txt"), "b\n").unwrap();
    in_sq(&["add", "b.txt"]);
    in_sq(&["commit", "-q", "-m", "b"]);
    git(&["merge", "-q", "--squash", "kraal/sq"]);
    git(&["commit", "-q", "-m", "sq (#1)"]);
    std::fs::write(dir.join("data/dev.db"), "rows").unwrap();
    // A broken setup file: the worktree is still made without it.
    std::fs::create_dir_all(repo.join(".kraal")).unwrap();
    std::fs::write(repo.join(".kraal/worktree.toml"), "setup = 1\n").unwrap();
    git(&["add", ".kraal"]);
    git(&["commit", "-q", "-m", "broken"]);
    let out = wt(&["new", "broken"]);
    assert!(!out.status.success() && text(&out).contains(".kraal/worktree.toml"), "{}", text(&out));
    let out = wt(&["new", "broken", "--no-setup"]);
    assert!(out.status.success(), "{}", text(&out));

    let out = wt(&["prune", "--dry-run"]);
    let said = text(&out);
    assert!(said.contains("kraal/sq (its changes are in its base)"), "{said}");
    assert!(said.contains("kept data: it has ignored files not in the main checkout (dev.db)"), "{said}");
    assert!(said.contains("--discard-ignored"), "{said}");
    let out = wt(&["prune"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(!dir.join("sq").exists() && !dir.join("broken").exists());
    assert_eq!(std::fs::read_to_string(dir.join("data/dev.db")).unwrap(), "rows", "its own files are kept");
    let out = wt(&["rm", "data"]);
    assert!(!out.status.success() && text(&out).contains("--discard-ignored"), "{}", text(&out));
    let out = wt(&["prune", "--discard-ignored"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(!dir.join("data").exists());
}
