//! Cross-version tests: this kraal with a session that another release started, and that
//! release's commands with a session this kraal started. `KRAAL_OLD` is the other release's
//! binary: `tests/compat.sh` fetches the releases kraal keeps working with and runs these with
//! each of them.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
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
        // Under /tmp: macOS's temporary directory is too long for the sockets kraal makes there.
        let root = PathBuf::from("/tmp").join(format!("kraal-compat-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["run", "state", "config/kraal/workspaces"] {
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
        c.env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env_remove("KRAAL_PANE")
            .env_remove("KRAAL_SOCKET")
            .env_remove("KRAAL_SESSION")
            .env_remove("KRAAL_SESSION_NAME")
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

    /// An agent hook run in pane `pane`, as an agent runs `kraal hook claude`.
    fn hook(&self, kraal: &Kraal, pane: &str, payload: &str) {
        let mut c = self.cmd(kraal, &["hook", "claude"]);
        c.env("KRAAL_PANE", pane).env("KRAAL_SOCKET", self.socket()).stdin(Stdio::piped());
        let mut child = c.spawn().unwrap();
        child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
        assert!(child.wait_with_output().unwrap().status.success());
    }

    /// `kraal events` from `kraal`: the first event of type `kind` about pane `name`, once `act`
    /// has run (the stream starts from the session's first event: none is missed).
    fn event(&self, kraal: &Kraal, name: &str, kind: &str, act: impl FnOnce()) -> serde_json::Value {
        use std::io::BufRead;
        let mut child = self.cmd(kraal, &["events", "--since", "0", "pane.*"]).stdout(Stdio::piped()).spawn().unwrap();
        let out = child.stdout.take().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        act();
        let start = Instant::now();
        let found = loop {
            let left = Duration::from_secs(10).saturating_sub(start.elapsed());
            let line = rx.recv_timeout(left).unwrap_or_else(|_| panic!("no {kind} event for {name}"));
            let event: serde_json::Value = serde_json::from_str(&line).unwrap();
            if event["name"] == name && event["type"] == kind {
                break event;
            }
        };
        let _ = child.kill();
        let _ = child.wait();
        found
    }

    /// `kraal attach` from `kraal` in a terminal (`script`, from util-linux) until the session
    /// has resized its panes to it, then ctrl+b d; what the terminal showed.
    #[cfg(target_os = "linux")]
    fn attach_and_detach(&self, kraal: &Kraal) -> String {
        use std::sync::{Arc, Mutex};
        let attach = self.cmd(kraal, &["attach"]);
        let line = std::iter::once(attach.get_program())
            .chain(attach.get_args())
            .map(|a| format!("'{}'", a.to_string_lossy()))
            .collect::<Vec<_>>()
            .join(" ");
        let mut c = Command::new("script");
        c.args(["-q", "-f", "-c", &format!("stty cols 100 rows 30; exec {line}"), "/dev/null"]);
        for (k, v) in attach.get_envs() {
            match v {
                Some(v) => c.env(k, v),
                None => c.env_remove(k),
            };
        }
        c.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
        let mut child = c.spawn().expect("script (util-linux)");
        let mut keys = child.stdin.take().unwrap();
        let shown = Arc::new(Mutex::new(Vec::new()));
        let reader = {
            let (mut out, shown) = (child.stdout.take().unwrap(), shown.clone());
            std::thread::spawn(move || {
                let mut buf = [0u8; 8192];
                while let Ok(n @ 1..) = out.read(&mut buf) {
                    shown.lock().unwrap().extend_from_slice(&buf[..n]);
                }
            })
        };
        // Attached: the panes fit in its 30 rows.
        let start = Instant::now();
        while !self.panes(&Kraal::this()).iter().all(|p| p["rows"].as_u64().unwrap() < 30) {
            let seen = String::from_utf8_lossy(&shown.lock().unwrap()).into_owned();
            assert!(start.elapsed() < Duration::from_secs(20), "kraal {} never attached: {seen}", kraal.version);
            std::thread::sleep(Duration::from_millis(100));
        }
        keys.write_all(b"\x02d").unwrap();
        let start = Instant::now();
        while child.try_wait().unwrap().is_none() {
            let seen = String::from_utf8_lossy(&shown.lock().unwrap()).into_owned();
            assert!(start.elapsed() < Duration::from_secs(10), "kraal {} did not detach: {seen}", kraal.version);
            std::thread::sleep(Duration::from_millis(50));
        }
        drop(keys);
        reader.join().unwrap();
        let shown = String::from_utf8_lossy(&shown.lock().unwrap()).into_owned();
        assert!(shown.contains("[kraal: detached]"), "kraal {}: {shown}", kraal.version);
        shown
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
        let mut stream = std::os::unix::net::UnixStream::connect(self.socket()).unwrap();
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let message = serde_json::json!({ "type": "request", "version": 2, "request": request }).to_string();
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

    /// `kraal events` from `kraal`, too slow for the session: it reads one event, then nothing
    /// while far more happens than is kept waiting for a reader, then all it is given. The lines
    /// it read then, how it ended and what it said.
    fn slow_reader(&self, kraal: &Kraal) -> (Vec<serde_json::Value>, Option<i32>, String) {
        use std::io::BufRead;
        let notify = |title: &str, body: &str| {
            let answer = self.ask(serde_json::json!({ "cmd": "notify", "title": title, "body": body }));
            assert!(answer["result"]["Ok"].is_object(), "{answer}");
        };
        notify("first", "");
        let mut child = self
            .cmd(kraal, &["events", "--since", "0", "notification"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let out = child.stdout.take().unwrap();
        let (tx, lines) = std::sync::mpsc::channel();
        let (go, asked) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            let mut out = std::io::BufReader::new(out).lines().map_while(Result::ok);
            if let Some(first) = out.next() {
                let _ = tx.send(first);
            }
            let _ = asked.recv();
            for line in out {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let first = lines.recv_timeout(Duration::from_secs(10)).expect("the event from before");
        assert!(first.contains(r#""title":"first""#), "{first}");
        let body = "x".repeat(16 << 10);
        for i in 0..300 {
            notify(&i.to_string(), &body);
        }
        go.send(()).unwrap();
        let mut read = Vec::new();
        loop {
            match lines.recv_timeout(Duration::from_secs(20)) {
                Ok(line) => read.push(serde_json::from_str(&line).unwrap()),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(_) => {
                    let _ = child.kill();
                    panic!("kraal {}: the stream never ended, {} lines read", kraal.version, read.len());
                }
            }
        }
        let out = child.wait_with_output().unwrap();
        (read, out.status.code(), String::from_utf8_lossy(&out.stderr).into_owned())
    }

    /// Whether the server lists `feature` (one this kraal has and the oldest release does not).
    fn server_has(&self, feature: &str) -> bool {
        self.pong()["features"].as_array().is_some_and(|l| l.iter().any(|f| f == feature))
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.try_run(&Kraal::this(), &["kill"]);
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn write_script(path: &Path, body: &str) {
    // A child process writes it: a file this process held open for writing could be inherited
    // by a program another test starts, and running the script would fail with "text file busy".
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("cat > \"$1\" && chmod 755 \"$1\"")
        .arg("sh")
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(body.as_bytes()).unwrap();
    assert!(child.wait().unwrap().success(), "writing {}", path.display());
}

/// What both releases do the same with a session: `client` drives the session `server` runs.
fn drive(env: &Env, client: &Kraal) {
    let ls = env.run(client, &["ls"]);
    assert!(ls.lines().any(|l| l.starts_with(&env.session) && l.contains("running")), "{ls}");
    let panes = env.panes(client);
    assert!(panes.iter().any(|p| p["name"] == "first"), "{panes:?}");
    // A new pane, input and output.
    let id = env.run(client, &["run", "--name", "worker", "--target", "first", "--right", "--", "sh"]);
    assert!(id.trim().parse::<u32>().is_ok(), "{id}");
    env.run(client, &["send", "-p", "worker", "--enter", "echo", "compat-$((40+2))"]);
    env.wait_for_text(client, "worker", "compat-42");
    env.run(client, &["keys", "-p", "worker", "ctrl+u"]);
    // An agent, reported by the client's hook.
    env.hook(client, "first", r#"{"hook_event_name":"SessionStart","session_id":"s-1"}"#);
    env.hook(client, "first", r#"{"hook_event_name":"UserPromptSubmit","session_id":"s-1"}"#);
    let agents = env.run(client, &["agents"]);
    assert!(agents.contains("first") && agents.contains("working"), "{agents}");
    let agents: serde_json::Value = serde_json::from_str(&env.run(client, &["agents", "--json"])).unwrap();
    assert_eq!(agents[0]["state"], "working", "{agents}");
    assert_eq!(env.run(client, &["wait", "-p", "first", "--until", "working", "--timeout", "2"]).trim(), "working");
    // The event stream.
    let exited = env.event(client, "job", "pane.exited", || {
        env.run(client, &["run", "--name", "job", "--no-focus", "--", "sh", "-c", "exit 3"]);
    });
    assert_eq!(exited["code"], 3, "{exited}");
    let workspaces = env.run(client, &["workspaces"]);
    assert!(workspaces.contains('w'), "{workspaces}");
    env.run(client, &["close", "worker"]);
    assert!(!env.panes(client).iter().any(|p| p["name"] == "worker"));
}

/// This kraal drives a session of the other release, says what that release cannot do and how
/// to update it, attaches to it, and stops it.
#[test]
#[ignore = "needs KRAAL_OLD, another release's kraal: tests/compat.sh runs it"]
fn this_kraal_drives_a_session_of_another_release() {
    let (old, this) = (Kraal::old(), Kraal::this());
    let env = Env::new("compat-new");
    env.run(&old, &["open", "w"]);
    drive(&env, &this);
    // What the session's release cannot do is refused with the reason, before anything is done.
    if !env.server_has("buffers") {
        let out = env.try_run(&this, &["buffer", "list"]);
        let err = String::from_utf8_lossy(&out.stderr);
        let why = format!("session {} runs kraal {}, which cannot keep paste buffers: ", env.session, old.version);
        assert!(!out.status.success() && err.contains(&why), "{err}");
        assert!(err.contains(&format!("`kraal -s {} restart` updates it", env.session)), "{err}");
    } else {
        let mut set = env.cmd(&this, &["buffer", "set", "kept"]).stdin(Stdio::piped()).spawn().unwrap();
        set.stdin.take().unwrap().write_all(b"text").unwrap();
        assert!(set.wait().unwrap().success());
        assert_eq!(env.run(&this, &["buffer", "show", "kept"]), "text");
    }
    if !env.server_has("edit_sudo") {
        let out = env.try_run(&this, &["edit", "--sudo", "nowhere:/etc/hosts"]);
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success() && err.contains("which cannot edit a server's files as root"), "{err}");
    }
    #[cfg(target_os = "linux")]
    {
        let shown = env.attach_and_detach(&this);
        if old.version != this.version {
            let notice = format!("session {} runs {}: `kraal -s {} restart`", env.session, old.version, env.session);
            assert!(shown.contains(&notice), "{shown}");
        }
    }
    let stopped = env.run(&this, &["kill"]);
    assert!(stopped.contains(&format!("stopped session {}", env.session)), "{stopped}");
    assert!(std::os::unix::net::UnixStream::connect(env.socket()).is_err(), "still listening");
}

/// The other release's commands drive a session of this kraal, attach to it and stop it.
#[test]
#[ignore = "needs KRAAL_OLD, another release's kraal: tests/compat.sh runs it"]
fn another_release_drives_a_session_of_this_kraal() {
    let (old, this) = (Kraal::old(), Kraal::this());
    let env = Env::new("compat-old");
    env.run(&this, &["open", "w"]);
    drive(&env, &old);
    #[cfg(target_os = "linux")]
    env.attach_and_detach(&old);
    let stopped = env.run(&old, &["kill"]);
    assert!(stopped.contains(&format!("stopped session {}", env.session)), "{stopped}");
    assert!(std::os::unix::net::UnixStream::connect(env.socket()).is_err(), "still listening");
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
    let out = env.run(&this, &["restart", "--yes"]);
    assert!(out.contains(&format!("restarted session {}", env.session)), "{out}");
    assert_ne!(env.server_pid(), before, "the same server still runs");
    let pong = env.pong();
    assert_eq!(pong["version"], this.version.as_str(), "{pong}");
    assert!(pong["features"].is_array(), "the session runs this kraal: {pong}");
    env.wait_for_text(&this, "first", "before-42");
    env.run(&this, &["send", "-p", "first", "--enter", "echo", "after-$((40+2))"]);
    env.wait_for_text(&this, "first", "after-42");
    let ls = env.run(&this, &["ls"]);
    assert!(ls.contains(&format!("running  {}", this.version)), "{ls}");
}

/// A reader of the events too slow for the session is dropped by either release. A session of
/// this kraal tells the other release's `kraal events` so, in messages every release reads: a
/// last line, then the reason it fails with. A session of a release that drops its readers
/// without a word ends this kraal's `kraal events` as it always did.
#[test]
#[ignore = "needs KRAAL_OLD, another release's kraal: tests/compat.sh runs it"]
fn a_reader_too_slow_for_the_events_ends_with_either_release() {
    let (old, this) = (Kraal::old(), Kraal::this());
    // What a reader read before it was dropped: the events in order, then the last line if any.
    let in_order = |read: &[serde_json::Value]| {
        let told = read.last().is_some_and(|l| l["type"] == "events.dropped");
        let events = &read[..read.len() - usize::from(told)];
        let titles: Vec<&str> = events.iter().filter_map(|e| e["title"].as_str()).collect();
        assert_eq!(titles, (0..events.len()).map(|i| i.to_string()).collect::<Vec<_>>());
        assert!(!events.is_empty() && events.len() < 300, "{} events read: never dropped", events.len());
        told
    };
    let env = Env::new("compat-slow-old");
    env.run(&this, &["open", "w"]);
    let (read, status, said) = env.slow_reader(&old);
    assert!(in_order(&read), "kraal {} was not told: {:?}", old.version, read.last().map(|l| &l["type"]));
    assert_eq!(status, Some(1), "{said}");
    assert!(said.contains("too slow") && said.contains("--since"), "{said}");
    let env = Env::new("compat-slow-new");
    env.run(&old, &["open", "w"]);
    let (read, status, said) = env.slow_reader(&this);
    if in_order(&read) {
        assert_eq!(status, Some(1), "{said}");
        assert!(said.contains("too slow"), "{said}");
    } else {
        assert_eq!((status, said.as_str()), (Some(0), ""), "a session of kraal {}", old.version);
    }
}
