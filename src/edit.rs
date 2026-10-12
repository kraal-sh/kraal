//! Editing a file of a server in the local editor: what runs on the server to read the file and
//! to write a new version of it, and the private local copy the editor works on. The session
//! itself (the editor's pane, each save sent back, the questions) is the server's
//! (`server::edits`).
//!
//! Nothing is written blindly: the server's version is identified by a hash made there when it
//! is read, and a new version replaces it only while that hash is still the same. A new version
//! is written next to the file, then renamed over it, with the file's mode (and owner, when the
//! user may give it): an interrupted save leaves the old file whole. Once the new version has
//! arrived, the check and the rename happen under a lock next to the file, so two kraal editors
//! of the same file never overwrite each other unseen.
//!
//! A file the user may not write is edited as root on request (`kraal edit --sudo`): one ssh
//! connection runs sudo for the whole edit, sudo reads the password kraal is handed once (typed
//! in a pane, never kept), and the same scripts run there as root.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};

/// The largest file edited this way.
pub const MAX_SIZE: u64 = 16 << 20;
/// How long reading or writing the file on the server may take.
const TIMEOUT: Duration = Duration::from_secs(60);
/// What precedes each answer of the server, so that noise from its login scripts is skipped.
const OPENED: &[u8] = b"KRAAL:EDIT\0";
const SAVED: &[u8] = b"KRAAL:SAVED\0";
const CHANGED: &[u8] = b"KRAAL:CHANGED\0";
/// Before the destination: never a prompt, and the server's bytes as they are.
const SSH_OPTIONS: [&str; 5] = ["-o", "BatchMode=yes", "-o", "ConnectTimeout=8", "-T"];
/// How the scripts end when the user may not read or write the file or its directory: what
/// sudo would allow.
const NOT_PERMITTED: i32 = 4;

/// `k FILE` prints what identifies the file's content: its sha256, else (no tool for it there)
/// its POSIX checksum and size. A server always picks the same tool, so two of them compare.
const IDENTIFY: &str =
    "k() { sha256sum < \"$1\" 2>/dev/null || shasum -a 256 < \"$1\" 2>/dev/null || cksum < \"$1\"; }";

/// The file on the server, as found when it was read: its directory (absolute, symbolic links
/// resolved) and its name there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFile {
    pub dir: String,
    pub name: String,
}

impl RemoteFile {
    pub fn path(&self) -> String {
        if self.dir.ends_with('/') {
            format!("{}{}", self.dir, self.name)
        } else {
            format!("{}/{}", self.dir, self.name)
        }
    }
}

/// A file read from a server.
#[derive(Debug)]
pub struct Opened {
    pub file: RemoteFile,
    pub content: Vec<u8>,
    /// What identified it on the server (see [`IDENTIFY`]).
    pub identity: String,
}

/// What became of a new version sent to the server.
#[derive(Debug, PartialEq, Eq)]
pub enum Saved {
    /// It replaced the file; what identifies it now.
    Written(String),
    /// The file changed on the server since it was read (what identifies it now): nothing was
    /// written.
    Changed(String),
}

/// The user may not read or write the file, or its directory, as themselves: as root they could
/// (what the server said).
#[derive(Debug)]
pub struct NotPermitted(pub String);

impl std::fmt::Display for NotPermitted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NotPermitted {}

/// Whether `name` is one plain name in a directory: what a server says a file is called can
/// only ever name a file in the directory it is put in.
pub fn plain_name(name: &str) -> bool {
    !name.is_empty() && name != "." && name != ".." && !name.contains(['/', '\0'])
}

/// What reads the file at `$f`, in a subshell: it follows symbolic links, refuses what is not a
/// regular file it may read and replace, or is over [`MAX_SIZE`], and prints [`OPENED`], the
/// file's directory, name, identity and size, its content, then its identity again (a file that
/// changed while it was read is told apart). As root, a file is refused only when it cannot be
/// written at all.
fn open_function(as_root: bool) -> String {
    let hint = if as_root { "" } else { ": kraal edit --sudo edits it as root" };
    let lines = [
        "o() (".to_string(),
        "case $f in */*) d=${f%/*}/ ;; *) d=./ ;; esac".into(),
        "n=${f##*/}".into(),
        "cd -- \"$d\" || exit 3".into(),
        "i=0".into(),
        // Each link is followed from its own directory, as the system does.
        "while [ -n \"$n\" ] && [ -L \"$n\" ]; do \
         i=$((i + 1)); \
         if [ \"$i\" -gt 40 ]; then echo 'too many levels of symbolic links' >&2; exit 3; fi; \
         l=$(readlink -- \"$n\" && printf x) || l=; \
         case $l in *x) l=${l%x}; l=${l%?} ;; *) echo 'a symbolic link that cannot be read' >&2; exit 3 ;; esac; \
         case $l in */*) cd -- \"${l%/*}/\" || exit 3 ;; esac; \
         n=${l##*/}; \
         done"
            .into(),
        "if [ -z \"$n\" ] || [ -d \"$n\" ]; then echo 'not a file but a directory' >&2; exit 3; fi".into(),
        "if [ ! -e \"$n\" ]; then echo 'no such file' >&2; exit 3; fi".into(),
        "if [ ! -f \"$n\" ]; then echo 'not a regular file' >&2; exit 3; fi".into(),
        format!("if [ ! -r \"$n\" ]; then echo 'you may not read it{hint}' >&2; exit {NOT_PERMITTED}; fi"),
        format!(
            "if [ ! -w \"$n\" ]; then \
             o=$(ls -ld -- \"$n\" | awk '{{print $3}}'); \
             echo \"you may not write it (it belongs to $o){hint}\" >&2; exit {NOT_PERMITTED}; fi"
        ),
        format!(
            "if [ ! -w . ]; then echo 'you may not write in its directory, where its new versions are written before they replace it{hint}' >&2; exit {NOT_PERMITTED}; fi"
        ),
        "d=$(pwd -P && printf x) || exit 3".into(),
        "d=${d%x}; d=${d%?}".into(),
        "s=$(wc -c < \"$n\") || exit 3".into(),
        "set -- $s".into(),
        format!("if [ \"$1\" -gt {MAX_SIZE} ]; then echo 'it is larger than {} MiB' >&2; exit 3; fi", MAX_SIZE >> 20),
        "h=$(k \"$n\") || exit 3".into(),
        "printf 'KRAAL:EDIT\\0%s\\0%s\\0%s\\0%s\\0' \"$d\" \"$n\" \"$h\" \"$1\"".into(),
        "cat -- \"$n\" || exit 3".into(),
        "h=$(k \"$n\") || exit 3".into(),
        "printf '\\0%s\\0' \"$h\"".into(),
        ")".into(),
    ];
    lines.join("\n")
}

/// What writes the bytes `receive` prints as the new version of `$sn` in `$sd`, in a subshell, if
/// the file is still the one `$se` identifies and the bytes are `$sl` long: a new file next to
/// it, with its mode (and owner when allowed), renamed over it. The new version arrives first;
/// then, holding a lock that other kraal saves of the same file wait for, the file is checked
/// again and replaced. Prints [`SAVED`] and the new identity, or [`CHANGED`] and the identity
/// of what is there now.
fn save_function() -> String {
    let lines = [
        "s() (",
        "cd -- \"$sd\" || exit 3",
        "n=$sn",
        "gone() { echo 'it was removed, or replaced by something that is not a regular file' >&2; exit 3; }",
        "if [ -L \"$n\" ] || [ ! -f \"$n\" ]; then gone; fi",
        "h=$(k \"$n\") || exit 3",
        // Changed already: nothing to send.
        "if [ \"$h\" != \"$se\" ]; then printf 'KRAAL:CHANGED\\0%s\\0' \"$h\"; exit 0; fi",
        "t=$(mktemp ./.kraal-edit-XXXXXX) || exit 3",
        "l=",
        "trap '[ -z \"$t\" ] || rm -f -- \"$t\"; [ -z \"$l\" ] || rmdir -- \"$l\"' EXIT",
        "trap 'exit 1' HUP INT TERM",
        // A copy first, for the mode, owner, group and ACLs; then the new content in it.
        "cp -p -- \"$n\" \"$t\" || exit 3",
        "receive > \"$t\" || exit 3",
        "s=$(wc -c < \"$t\") || exit 3",
        "set -- $s",
        "if [ \"$1\" != \"$sl\" ]; then echo 'the new version did not arrive whole: the file was left as it was' >&2; exit 3; fi",
        "set -- $(printf %s \"$n\" | cksum)",
        "m=./.kraal-edit-lock-$1",
        "i=0",
        "until mkdir -- \"$m\" 2>/dev/null; do \
         if [ ! -d \"$m\" ]; then echo 'no lock can be made next to it' >&2; exit 3; fi; \
         i=$((i + 1)); \
         if [ \"$i\" -gt 100 ]; then echo \"another save of this file is under way; if none is, remove $(pwd -P)/${m#./}\" >&2; exit 3; fi; \
         sleep 0.1 2>/dev/null || { sleep 1; i=$((i + 9)); }; \
         done",
        "l=$m",
        "if [ -L \"$n\" ] || [ ! -f \"$n\" ]; then gone; fi",
        "h=$(k \"$n\") || exit 3",
        "if [ \"$h\" != \"$se\" ]; then printf 'KRAAL:CHANGED\\0%s\\0' \"$h\"; exit 0; fi",
        "h=$(k \"$t\") || exit 3",
        "mv -f -- \"$t\" \"$n\" || exit 3",
        "t=",
        "rmdir -- \"$l\"",
        "l=",
        "printf 'KRAAL:SAVED\\0%s\\0' \"$h\"",
        ")",
    ];
    lines.join("\n")
}

/// What reads the file at `path` (absolute, `~/…`, or relative to the home directory): see
/// [`open_function`].
pub fn open_script(path: &str) -> String {
    let f = crate::files::remote_dir_arg(path);
    [IDENTIFY.to_string(), open_function(false), format!("f={f}"), "o".into()].join("\n")
}

/// What reads `len` bytes as the new version of `file`, written if the file is still the one
/// `expected` identifies: see [`save_function`].
pub fn save_script(file: &RemoteFile, expected: &str, len: usize) -> String {
    let q = crate::shell::quote_always;
    [
        IDENTIFY.to_string(),
        "receive() { cat; }".into(),
        save_function(),
        format!("sd={}", q(&file.dir)),
        format!("sn={}", q(&file.name)),
        format!("se={}", q(expected)),
        format!("sl={len}"),
        "s".into(),
    ]
    .join("\n")
}

/// The file [`open_script`] printed, or why it cannot be edited.
pub fn parse_opened(out: &[u8]) -> Result<Opened> {
    let unexpected = || anyhow::anyhow!("the server's answer is not the file kraal asked for");
    let start = find(out, OPENED).ok_or_else(unexpected)? + OPENED.len();
    let mut fields = out[start..].splitn(5, |&b| b == 0);
    let (dir, name, identity, size) = (
        fields.next().ok_or_else(unexpected)?,
        fields.next().ok_or_else(unexpected)?,
        fields.next().ok_or_else(unexpected)?,
        fields.next().ok_or_else(unexpected)?,
    );
    let rest = fields.next().ok_or_else(unexpected)?;
    let size: usize = std::str::from_utf8(size).ok().and_then(|s| s.parse().ok()).ok_or_else(unexpected)?;
    let (dir, name) = match (std::str::from_utf8(dir), std::str::from_utf8(name)) {
        // The name is used for the local copies: it must be a name, in the directory it is put in.
        (Ok(d), Ok(n)) if d.starts_with('/') && plain_name(n) => (d.to_string(), n.to_string()),
        (Ok(_), Ok(_)) => return Err(unexpected()),
        // Shown approximately, it could name another file.
        _ => bail!("its path is not valid UTF-8: kraal cannot point at it safely"),
    };
    let (content, after) = (rest.get(..size), rest.get(size..));
    let again = after.and_then(|a| a.strip_prefix(b"\0")).and_then(|a| a.split(|&b| b == 0).next());
    match (content, again) {
        (Some(content), Some(again)) if again == identity => Ok(Opened {
            file: RemoteFile { dir, name },
            content: content.to_vec(),
            identity: String::from_utf8_lossy(identity).into_owned(),
        }),
        _ => bail!("it changed while it was read: try again"),
    }
}

/// What [`save_script`] printed.
pub fn parse_saved(out: &[u8]) -> Result<Saved> {
    let field = |marker: &[u8]| {
        let start = find(out, marker)? + marker.len();
        let value = out[start..].split(|&b| b == 0).next()?;
        Some(String::from_utf8_lossy(value).into_owned())
    };
    match (field(SAVED), field(CHANGED)) {
        (Some(identity), None) => Ok(Saved::Written(identity)),
        (None, Some(identity)) => Ok(Saved::Changed(identity)),
        _ => bail!("the server's answer is not what kraal expects"),
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// `program` (ssh) with `ssh` (options, destination last) running `script` with `sh`, whatever
/// the login shell there.
fn ssh_command(program: &str, ssh: &[String], script: &str) -> Result<Command> {
    let (destination, options) = ssh.split_last().context("no ssh destination")?;
    let mut cmd = Command::new(program);
    cmd.args(options).args(SSH_OPTIONS).arg(destination).arg(crate::shell::sh_c(script));
    Ok(cmd)
}

/// How kraal reaches the file: ssh as the user, one command each time, or the root session
/// sudo runs for the whole edit.
#[derive(Clone)]
pub enum Channel {
    /// ssh arguments, destination last.
    Ssh(Vec<String>),
    Sudo(Arc<Mutex<Session>>),
}

impl Channel {
    /// Read `path` (see [`open_script`]).
    pub fn open(&self, path: &str) -> Result<Opened> {
        match self {
            Channel::Ssh(ssh) => open(ssh, path),
            Channel::Sudo(s) => s.lock().map_err(|_| anyhow::anyhow!("the sudo session failed"))?.open(path),
        }
    }

    /// Send `content` as the new version of `file`, which must still be what `expected`
    /// identifies.
    pub fn save(&self, file: &RemoteFile, expected: &str, content: &[u8]) -> Result<Saved> {
        match self {
            Channel::Ssh(ssh) => save(ssh, file, expected, content),
            Channel::Sudo(s) => {
                s.lock().map_err(|_| anyhow::anyhow!("the sudo session failed"))?.save(file, expected, content)
            }
        }
    }
}

/// Read `path` on the server `ssh` reaches (options, destination last).
pub fn open(ssh: &[String], path: &str) -> Result<Opened> {
    open_with("ssh", ssh, path)
}

fn open_with(program: &str, ssh: &[String], path: &str) -> Result<Opened> {
    if path.is_empty() {
        bail!("which file? (SERVER:PATH)");
    }
    let mut cmd = ssh_command(program, ssh, &open_script(path))?;
    let limit = usize::try_from(MAX_SIZE).unwrap_or(usize::MAX).saturating_add(64 << 10);
    let out = crate::exec::run(&mut cmd, TIMEOUT, limit).with_context(|| format!("running {program}"))?;
    if out.code == Some(NOT_PERMITTED) {
        return Err(NotPermitted(crate::exec::last_stderr_line(&out)).into());
    }
    if !out.success() {
        bail!("{}", crate::exec::last_stderr_line(&out));
    }
    if out.truncated {
        bail!("the server's answer is larger than {} MiB", MAX_SIZE >> 20);
    }
    parse_opened(&out.stdout)
}

/// Send `content` as the new version of `file`, which must still be what `expected` identifies.
pub fn save(ssh: &[String], file: &RemoteFile, expected: &str, content: &[u8]) -> Result<Saved> {
    save_with("ssh", ssh, file, expected, content)
}

fn save_with(program: &str, ssh: &[String], file: &RemoteFile, expected: &str, content: &[u8]) -> Result<Saved> {
    let mut cmd = ssh_command(program, ssh, &save_script(file, expected, content.len()))?;
    let (reader, mut writer) = std::io::pipe().context("making a pipe")?;
    let data = content.to_vec();
    // The server stops reading when its file changed: what is left is not needed.
    let feeder = std::thread::spawn(move || {
        let _ = writer.write_all(&data);
    });
    let out = crate::exec::run_with_input(&mut cmd, reader.into(), TIMEOUT, 64 << 10)
        .with_context(|| format!("running {program}"));
    // The command holds the pipe's other end: once it is gone, a feeder the server stopped
    // reading from is told so, and ends.
    drop(cmd);
    let _ = feeder.join();
    let out = out?;
    if !out.success() {
        bail!("{}", crate::exec::last_stderr_line(&out));
    }
    parse_saved(&out.stdout)
}

/// The root side of a sudo session: it reads requests from its standard input, one line each,
/// arguments as `printf` escapes, and answers each with what the scripts print, then an end
/// marker with their status and errors. `open PATH`; `save DIR NAME EXPECTED LEN` followed by
/// the LEN bytes of the new version, always read whole (so the next request is a request) into
/// a private directory before anything else. `$1` is the session's own marker.
fn helper_script() -> String {
    let lines = [
        IDENTIFY.to_string(),
        open_function(true),
        "receive() { cat -- \"$u/new\"; }".into(),
        save_function(),
        "x=$1".into(),
        "set -f".into(),
        "u=$(mktemp -d) || exit 3".into(),
        "trap 'rm -rf -- \"$u\"' EXIT".into(),
        "trap 'exit 1' HUP INT TERM".into(),
        "dec() { v=$(printf \"${1}x\") && v=${v%x}; }".into(),
        // Exactly $1 bytes of the input: each dd reads once, at most what is still due.
        "rd() { : > \"$u/new\" || return 1; g=0; \
         while [ \"$g\" -lt \"$1\" ]; do \
         dd bs=$(($1 - g)) count=1 >> \"$u/new\" 2>/dev/null || return 1; \
         w=$(wc -c < \"$u/new\" | tr -d ' ') || return 1; \
         [ \"$w\" -gt \"$g\" ] || return 1; g=$w; \
         done; }"
            .into(),
        "printf 'KRAAL:ROOT:%s\\0' \"$x\"".into(),
        "while IFS= read -r q; do".into(),
        "set -- $q".into(),
        "case $1 in".into(),
        "open) dec \"$2\"; f=$v; o < /dev/null 2> \"$u/err\"; r=$? ;;".into(),
        "save) rd \"$5\" || exit 3; dec \"$2\"; sd=$v; dec \"$3\"; sn=$v; dec \"$4\"; se=$v; sl=$5; \
         s < /dev/null 2> \"$u/err\"; r=$? ;;"
            .into(),
        "*) exit 3 ;;".into(),
        "esac".into(),
        "printf 'KRAAL:END:%s\\0%s\\0' \"$x\" \"$r\"".into(),
        "cat -- \"$u/err\"".into(),
        "printf '\\0'".into(),
        "done".into(),
    ];
    lines.join("\n")
}

/// What starts a sudo session as the user: it makes `path` absolute there (`~` is the user's
/// home, a relative path is in it: `$HOME` as the user knows it, its links not resolved), says
/// whether sudo needs a password and which path that is, then runs [`helper_script`] through
/// sudo, which reads the password from its input first when it needs one.
fn launch_script(path: &str, nonce: &str) -> String {
    let q = crate::shell::quote_always;
    [
        format!("x={}", q(nonce)),
        format!("h={}", q(&helper_script())),
        format!("f={}", crate::files::remote_dir_arg(path)),
        "case $f in /*) ;; *) f=${HOME:-$(pwd)}/$f ;; esac".into(),
        "if sudo -n true 2>/dev/null; then a=ready; set -- -n; else a=password; set -- -S -p ''; fi".into(),
        "printf 'KRAAL:SUDO:%s\\0%s\\0%s\\0' \"$x\" \"$a\" \"$f\"".into(),
        "exec sudo \"$@\" -- sh -c \"$h\" kraal-edit \"$x\"".into(),
    ]
    .join("\n")
}

/// One ssh connection to a server running the root side of an edit through sudo, for as long as
/// the edit lasts: sudo is asked once.
pub struct Session {
    child: Child,
    stdin: Option<ChildStdin>,
    out: crossbeam_channel::Receiver<Vec<u8>>,
    /// What came out and is not read yet.
    buf: Vec<u8>,
    err: Arc<Mutex<Vec<u8>>>,
    /// Ends each answer: what a file's content cannot hold by chance.
    nonce: String,
    /// The path asked for, absolute on the server.
    pub path: String,
    /// sudo waits for the password (see [`Session::authenticate`]).
    pub needs_password: bool,
}

impl Session {
    /// Connect to the server `ssh` reaches and start sudo there for `path`.
    pub fn start(ssh: &[String], path: &str) -> Result<Session> {
        Session::start_with("ssh", ssh, path)
    }

    fn start_with(program: &str, ssh: &[String], path: &str) -> Result<Session> {
        if path.is_empty() {
            bail!("which file? (SERVER:PATH)");
        }
        let nonce = new_nonce();
        let mut cmd = ssh_command(program, ssh, &launch_script(path, &nonce))?;
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().with_context(|| format!("running {program}"))?;
        let stdin = child.stdin.take();
        let (tx, out) = crossbeam_channel::unbounded();
        if let Some(mut pipe) = child.stdout.take() {
            std::thread::spawn(move || {
                let mut chunk = vec![0u8; 64 << 10];
                while let Ok(n) = pipe.read(&mut chunk) {
                    if n == 0 || tx.send(chunk[..n].to_vec()).is_err() {
                        break;
                    }
                }
            });
        }
        let err = Arc::new(Mutex::new(Vec::new()));
        if let Some(mut pipe) = child.stderr.take() {
            let err = err.clone();
            std::thread::spawn(move || {
                let mut chunk = [0u8; 4096];
                while let Ok(n) = pipe.read(&mut chunk) {
                    if n == 0 {
                        break;
                    }
                    if let Ok(mut e) = err.lock()
                        && e.len() < 64 << 10
                    {
                        e.extend_from_slice(&chunk[..n]);
                    }
                }
            });
        }
        let mut s =
            Session { child, stdin, out, buf: Vec::new(), err, nonce, path: String::new(), needs_password: false };
        let marker = format!("KRAAL:SUDO:{}\0", s.nonce).into_bytes();
        let deadline = Instant::now() + Duration::from_secs(30);
        let at = s.wait_for(&marker, deadline, true)?;
        s.wait_fields(at, 2, deadline)?;
        let fields: Vec<&[u8]> = s.buf[at..].splitn(3, |&b| b == 0).collect();
        let [mode, path, rest] = fields[..] else { bail!("the server's answer is not what kraal expects") };
        let path = std::str::from_utf8(path).map_err(|_| anyhow::anyhow!("its path is not valid UTF-8"))?;
        (s.path, s.needs_password) = (path.to_string(), mode == b"password");
        s.buf = rest.to_vec();
        if !s.needs_password {
            s.wait_root(Instant::now() + Duration::from_secs(30))?;
        }
        Ok(s)
    }

    /// Hand sudo the password it waits for, then wipe it.
    pub fn authenticate(&mut self, password: &mut [u8]) -> Result<()> {
        let sent = match self.stdin.as_mut() {
            Some(stdin) => stdin.write_all(password).and_then(|()| stdin.write_all(b"\n")).and_then(|()| stdin.flush()),
            None => Err(std::io::ErrorKind::BrokenPipe.into()),
        };
        wipe(password);
        sent.context("sudo is gone")?;
        self.needs_password = false;
        self.wait_root(Instant::now() + Duration::from_secs(30))
    }

    fn wait_root(&mut self, deadline: Instant) -> Result<()> {
        let marker = format!("KRAAL:ROOT:{}\0", self.nonce).into_bytes();
        let at = self.wait_for(&marker, deadline, true)?;
        self.buf.drain(..at);
        Ok(())
    }

    /// Read until `marker`; the position just after it. Until the root side runs (`sudo`), sudo
    /// refusing (a wrong password, a user it does not know) ends the wait at once.
    fn wait_for(&mut self, marker: &[u8], deadline: Instant, sudo: bool) -> Result<usize> {
        loop {
            if let Some(at) = find(&self.buf, marker) {
                return Ok(at + marker.len());
            }
            let refused = |said: &str| {
                let words =
                    ["Sorry", "incorrect password", "not in the sudoers", "not allowed", "a password is required"];
                sudo && words.iter().any(|w| said.contains(w))
            };
            let said = self.said();
            if refused(&said) {
                self.stop();
                bail!("sudo refused: {}", last_line(&said));
            }
            match self.out.recv_deadline(deadline) {
                Ok(chunk) => self.buf.extend_from_slice(&chunk),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    self.stop();
                    bail!("the server did not answer in time");
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    // What it said on the way out may still be coming.
                    let _ = self.child.wait();
                    std::thread::sleep(Duration::from_millis(50));
                    let said = self.said();
                    if refused(&said) {
                        bail!("sudo refused: {}", last_line(&said));
                    }
                    bail!("{}", if said.trim().is_empty() { "the connection ended".into() } else { last_line(&said) });
                }
            }
        }
    }

    /// Read until `count` fields, each ended by a NUL, follow position `at`.
    fn wait_fields(&mut self, at: usize, count: usize, deadline: Instant) -> Result<()> {
        while self.buf[at..].iter().filter(|&&b| b == 0).count() < count {
            match self.out.recv_deadline(deadline) {
                Ok(chunk) => self.buf.extend_from_slice(&chunk),
                Err(_) => bail!("the server's answer is cut short"),
            }
        }
        Ok(())
    }

    fn said(&self) -> String {
        self.err.lock().map(|e| String::from_utf8_lossy(&e).into_owned()).unwrap_or_default()
    }

    /// Send a request (and what follows it), and read its answer: the scripts' status, what they
    /// printed and their errors.
    fn request(&mut self, line: &str, payload: &[u8]) -> Result<(i32, Vec<u8>, String)> {
        anyhow::ensure!(!self.needs_password, "sudo still waits for the password");
        let stdin = self.stdin.as_mut().context("the sudo session ended")?;
        let sent = stdin
            .write_all(line.as_bytes())
            .and_then(|()| stdin.write_all(b"\n"))
            .and_then(|()| stdin.write_all(payload))
            .and_then(|()| stdin.flush());
        if sent.is_err() {
            self.stop();
            bail!("the sudo session ended: {}", last_line(&self.said()));
        }
        let marker = format!("KRAAL:END:{}\0", self.nonce).into_bytes();
        let at = self.wait_for(&marker, Instant::now() + TIMEOUT, false)?;
        // `<status>\0<errors>\0`
        self.wait_fields(at, 2, Instant::now() + Duration::from_secs(10))?;
        let rest = self.buf.split_off(at);
        let output = std::mem::take(&mut self.buf);
        let output = output[..output.len() - marker.len()].to_vec();
        let mut fields = rest.splitn(3, |&b| b == 0);
        let status = fields.next().and_then(|s| std::str::from_utf8(s).ok()?.parse().ok()).unwrap_or(-1);
        let errors = String::from_utf8_lossy(fields.next().unwrap_or_default()).into_owned();
        self.buf = fields.next().unwrap_or_default().to_vec();
        Ok((status, output, errors))
    }

    pub fn open(&mut self, path: &str) -> Result<Opened> {
        let (status, out, errors) = self.request(&format!("open {}", arg(path)), &[])?;
        if status != 0 {
            bail!("{}", last_line(&errors));
        }
        parse_opened(&out)
    }

    pub fn save(&mut self, file: &RemoteFile, expected: &str, content: &[u8]) -> Result<Saved> {
        let line = format!("save {} {} {} {}", arg(&file.dir), arg(&file.name), arg(expected), content.len());
        let (status, out, errors) = self.request(&line, content)?;
        if status != 0 {
            bail!("{}", last_line(&errors));
        }
        parse_saved(&out)
    }

    /// End it: sudo and the connection go (its input closed ends the root side).
    fn stop(&mut self) {
        self.stdin = None;
        let deadline = Instant::now() + Duration::from_secs(1);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A request's argument: one word of plain characters and `printf` escapes.
fn arg(s: &str) -> String {
    crate::shell::printf_escape(s, false)
}

fn last_line(text: &str) -> String {
    text.lines().rev().map(str::trim).find(|l| !l.is_empty()).unwrap_or("the sudo session ended").to_string()
}

fn new_nonce() -> String {
    let mut bytes = [0u8; 16];
    let random = std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut bytes));
    if random.is_err() {
        let t = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default();
        bytes = (t ^ (u128::from(std::process::id()) << 64)).to_le_bytes();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Overwrite a secret in memory.
fn wipe(secret: &mut [u8]) {
    for b in secret.iter_mut() {
        // SAFETY: a plain write to a byte we own; volatile so it is not optimised away.
        unsafe { std::ptr::write_volatile(b, 0) };
    }
}

/// Where a pane hands kraal the sudo password: a FIFO in the edit's private directory, so the
/// password is never written to a disk.
pub fn password_fifo(dir: &Path) -> std::io::Result<PathBuf> {
    let fifo = dir.join("sudo");
    let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).map_err(std::io::Error::other)?;
    // SAFETY: a plain syscall on a path we own.
    if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(fifo)
}

/// What the pane asking for the password runs: it reads it without echo and writes it to
/// `fifo`, or says it was not given.
pub fn password_command(fifo: &Path, prompt: &str) -> Vec<String> {
    let script = "f=$1; trap 'printf -- \"-\\n\" > \"$f\"; exit 1' HUP INT TERM; printf '%s' \"$2\"; \
                  stty -echo 2>/dev/null; IFS= read -r p; s=$?; stty echo 2>/dev/null; printf '\\n'; \
                  if [ \"$s\" != 0 ]; then printf -- '-\\n' > \"$f\"; exit 1; fi; printf '+%s\\n' \"$p\" > \"$f\"";
    ["sh", "-c", script, "kraal-sudo"]
        .iter()
        .map(|s| s.to_string())
        .chain([fifo.display().to_string(), prompt.into()])
        .collect()
}

/// The password the pane wrote to `fifo`, or `None` when it was not given, or not before
/// `timeout`.
pub fn read_password(fifo: &Path, timeout: Duration) -> std::io::Result<Option<Vec<u8>>> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(fifo)?;
    let deadline = Instant::now() + timeout;
    let mut got: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 512];
    let line = loop {
        match f.read(&mut chunk) {
            Ok(n) if n > 0 => {
                got.extend_from_slice(&chunk[..n]);
                wipe(&mut chunk);
                if let Some(end) = got.iter().position(|&b| b == b'\n') {
                    break Some(end);
                }
                continue;
            }
            // Nobody wrote yet (no writer, or nothing written).
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) => {
                wipe(&mut got);
                return Err(e);
            }
        }
        if Instant::now() > deadline {
            break None;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let password = match line {
        Some(end) if got.first() == Some(&b'+') => Some(got[1..end].to_vec()),
        _ => None,
    };
    wipe(&mut got);
    Ok(password)
}

/// What a look at the local copy tells of it: a save (in place, or by a rename) changes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
    inode: u64,
}

pub fn stamp(path: &Path) -> Option<Stamp> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(path).ok()?;
    Some(Stamp { modified: m.modified().ok(), len: m.len(), inode: m.ino() })
}

/// A new private directory (0700) under `base`, for one edit: its copy, and the pane that
/// asks for a sudo password.
pub fn private_dir(base: &Path) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    crate::paths::ensure_private_dir(base)?;
    let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default();
    for n in 0..100 {
        let dir = base.join(format!("{}-{stamp:x}-{n}", std::process::id()));
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("no free name for a private directory"))
}

/// Put `content` in the private directory `dir` as a file named `name` that only the user may
/// read (0600): what the editor works on.
pub fn copy_into(dir: &Path, name: &str, content: &[u8]) -> std::io::Result<PathBuf> {
    if !plain_name(name) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{name:?} is not a file name")));
    }
    let file = dir.join(name);
    write_new(&file, content)?;
    Ok(file)
}

/// Put `content` in a new private directory under `base` (see [`private_dir`], [`copy_into`]).
/// Returns the directory and the file.
pub fn local_copy(base: &Path, name: &str, content: &[u8]) -> std::io::Result<(PathBuf, PathBuf)> {
    let dir = private_dir(base)?;
    match copy_into(&dir, name, content) {
        Ok(file) => Ok((dir, file)),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dir);
            Err(e)
        }
    }
}

/// Write `content` over the editor's copy `local` (the server's version, say) unless the copy
/// is no longer as `seen`: a save made meanwhile is never overwritten. The copy is replaced by a
/// rename; its new stamp, or `None` when it had changed.
pub fn replace_copy(local: &Path, seen: Option<Stamp>, content: &[u8]) -> std::io::Result<Option<Stamp>> {
    // Next to it, in its private directory, under any other name than its own.
    let new = if local.file_name().is_some_and(|n| n == ".kraal-new") { ".kraal-new-1" } else { ".kraal-new" };
    let temp = local.with_file_name(new);
    let _ = std::fs::remove_file(&temp);
    write_new(&temp, content)?;
    if stamp(local) != seen {
        let _ = std::fs::remove_file(&temp);
        return Ok(None);
    }
    std::fs::rename(&temp, local)?;
    Ok(stamp(local))
}

/// Keep `content` in `dir` as `name`, or `name-1`, `name-2`… (before its extension) when that
/// is taken: nothing there is replaced. Only the user may read it (0600).
pub fn keep(dir: &Path, name: &str, content: &[u8]) -> std::io::Result<PathBuf> {
    if !plain_name(name) {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{name:?} is not a file name")));
    }
    std::fs::create_dir_all(dir)?;
    let (stem, ext) = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => (stem, format!(".{ext}")),
        _ => (name, String::new()),
    };
    for n in 0..1000 {
        let candidate = if n == 0 { dir.join(name) } else { dir.join(format!("{stem}-{n}{ext}")) };
        match write_new(&candidate, content) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other(format!("no free name for {name} in {}", dir.display())))
}

/// A new file (never one that exists, nor through a link) holding `content`, 0600.
fn write_new(path: &Path, content: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    f.write_all(content)?;
    f.sync_all()
}

#[cfg(test)]
mod tests;
