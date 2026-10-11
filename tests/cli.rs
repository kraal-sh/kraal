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
        write_script(&sh, "#!/bin/sh\nexport PS1='$ '\nexec sh\n");
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
        let start = Instant::now();
        loop {
            let screen = self.run(&["read", "-p", pane]);
            // A narrow pane wraps long lines: the text may run over two rows.
            if screen.contains(text) || screen.lines().collect::<String>().contains(text) {
                return screen;
            }
            assert!(start.elapsed() < Duration::from_secs(10), "timed out waiting for {text:?} in:\n{screen}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// The pid of this test's server (Linux: from /proc).
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
}

impl RawClient {
    fn attach(env: &Env, workspace: &str) -> RawClient {
        use std::io::Read;
        let socket = env.root.join("run/kraal").join(format!("{}.sock", env.session));
        let mut stream = std::os::unix::net::UnixStream::connect(socket).unwrap();
        let hello =
            format!(r#"{{"type":"attach","version":2,"cols":120,"rows":40,"workspace":"{workspace}","remote":false}}"#);
        raw_frame(&mut stream, 0, hello.as_bytes());
        let screen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (mut reader, kept) = (stream.try_clone().unwrap(), screen.clone());
        std::thread::spawn(move || {
            let mut header = [0u8; 5];
            while reader.read_exact(&mut header).is_ok() {
                let mut payload = vec![0u8; u32::from_le_bytes(header[..4].try_into().unwrap()) as usize];
                if reader.read_exact(&mut payload).is_err() {
                    break;
                }
                if header[4] == 1 {
                    kept.lock().unwrap().extend_from_slice(&payload);
                }
            }
        });
        RawClient { stream, screen }
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
    env.wait_for_text("srv", "connected admin@db");
    let screen = env.wait_for_text("deploy", "restored session");
    assert!(!screen.lines().any(|l| l.trim() == "DEPLOYED"), "the command must not run again by itself:\n{screen}");
    // An ssh that runs a command on the server is a command too, not a shell to reconnect.
    let screen = env.wait_for_text("remote", "restored session");
    assert!(!screen.contains("connected"), "the remote command must not run again by itself:\n{screen}");
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
    let left: Vec<_> = std::fs::read_dir(&runs).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name()).collect();
    assert!(left.iter().all(|n| n.to_string_lossy().ends_with(".log")), "{left:?}");
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

/// A release as GitHub serves it, made from this build: `kraal-<target>.tar.gz` and its
/// checksum, in `dir`.
fn fake_release(dir: &Path) -> String {
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        _ => "unknown-linux-musl",
    };
    let target = format!("{}-{os}", std::env::consts::ARCH);
    let inner = dir.join(format!("kraal-{target}"));
    std::fs::create_dir_all(&inner).unwrap();
    std::fs::copy(env!("CARGO_BIN_EXE_kraal"), inner.join("kraal")).unwrap();
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
        |args: &[&str]| Command::new(&kraal).arg("update").args(args).env("KRAAL_DOWNLOAD_URL", &url).output().unwrap();

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
