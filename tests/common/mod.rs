//! What the end-to-end tests share: a kraal session in directories of its own, and helpers.

pub mod isolation;

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

/// A session in directories of its own, stopped and removed when the test ends.
pub struct Env {
    pub root: PathBuf,
    pub session: String,
}

impl Env {
    pub fn new(name: &str) -> Env {
        isolation::check();
        // Under /tmp: macOS's temporary directory is too long for the sockets kraal makes there,
        // and kraal would fall back to a directory every test shares.
        let root = PathBuf::from("/tmp").join(format!("kraal-e2e-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["home", "run", "state", "config/kraal/workspaces"] {
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

    pub fn cmd(&self, args: &[&str]) -> Command {
        let mut c = self.isolated(Path::new(env!("CARGO_BIN_EXE_kraal")));
        c.args(["-s", &self.session]).args(args);
        c
    }

    /// `program` in this test's directories: a home and a session of its own, where the other
    /// tests have theirs ([`isolation`] keeps them all out of the developer's).
    pub fn isolated(&self, program: &Path) -> Command {
        let mut c = Command::new(program);
        c.env("HOME", self.root.join("home"))
            .env("XDG_RUNTIME_DIR", self.root.join("run"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CONFIG_HOME", self.root.join("config"));
        c
    }

    pub fn run(&self, args: &[&str]) -> String {
        let out = self.try_run(args);
        assert!(out.status.success(), "kraal {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    }

    pub fn try_run(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        // Nothing of what it says is read: a pipe would be read to an end that comes only once
        // every process that has it is gone.
        let quiet = std::process::Stdio::null;
        let _ = self.cmd(&["kill"]).stdout(quiet()).stderr(quiet()).status();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Write an executable script. A child process writes it: a file this process held open for
/// writing could be inherited by a program another test starts at that moment, and running the
/// script would then fail with "text file busy".
pub fn write_script(path: &Path, body: &str) {
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

/// Wait until `done`, failing the test with `what` after `limit`.
pub fn wait_until(limit: Duration, what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < limit, "{what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A git repository at `repo` with one commit of `files`, and a function running git in it.
pub fn git_repo(repo: &Path, files: &[(&str, &str)]) -> impl Fn(&[&str]) {
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

/// Drive the agent of pane `pane` in `env`'s session through Claude's hooks.
pub fn claude_hook(env: &Env, pane: &str, payload: &str) {
    use std::io::Write;
    let socket = env.root.join("run/kraal").join(format!("{}.sock", env.session));
    let mut c = env.cmd(&["hook", "claude"]);
    c.env("KRAAL_PANE", pane).env("KRAAL_SOCKET", &socket).stdin(std::process::Stdio::piped());
    let mut child = c.spawn().unwrap();
    child.stdin.take().unwrap().write_all(payload.as_bytes()).unwrap();
    assert!(child.wait_with_output().unwrap().status.success());
}
