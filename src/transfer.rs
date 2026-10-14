//! File transfers between this machine and servers, or between two servers: `kraal cp`.
//!
//! Endpoints are local paths or `server:path`, where `server` is an inventory name or
//! `user@host[:port]`. rsync is used when available (resumable, with progress), a tar stream
//! through ssh otherwise (links stay links, unlike scp); server-to-server copies stream a tar
//! archive through this machine so each side keeps its own port, jump host and key.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::servers::{self, Known, Server};

// A transfer has a handful of endpoints: a box around the server would only add an indirection.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Endpoint {
    Local(PathBuf),
    Remote { server: Server, path: String },
}

impl Endpoint {
    pub fn display(&self) -> String {
        match self {
            Endpoint::Local(p) => p.display().to_string(),
            Endpoint::Remote { server, path } => format!("{}:{path}", server.name),
        }
    }

    /// The same place, as a directory to copy into (`dir/`).
    pub fn into_dir(self) -> Endpoint {
        match self {
            Endpoint::Local(p) if !p.display().to_string().ends_with('/') => {
                Endpoint::Local(PathBuf::from(format!("{}/", p.display())))
            }
            Endpoint::Remote { server, path } if !path.ends_with('/') => {
                let path = if path.is_empty() { "~/".to_string() } else { format!("{path}/") };
                Endpoint::Remote { server, path }
            }
            same => same,
        }
    }
}

/// Parse `path`, `server:path` or `user@host:path`.
pub fn parse_endpoint(s: &str, known: &[Known]) -> Result<Endpoint> {
    if let Some((head, path)) = split_remote(s) {
        if let Some(server) = servers::resolve(known, head)
            && (known.iter().any(|k| k.server.name == head)
                || head.contains('@')
                || head.contains('.')
                || head.starts_with('['))
        {
            return Ok(Endpoint::Remote { server, path: path.to_string() });
        }
        if !Path::new(s).exists() {
            bail!("unknown server {head:?} (see `kraal servers`)");
        }
    }
    Ok(Endpoint::Local(PathBuf::from(s)))
}

/// `head:path` when the text before the first `:` looks like a host, not a path.
pub(crate) fn split_remote(s: &str) -> Option<(&str, &str)> {
    if s.starts_with('/') || s.starts_with("./") || s.starts_with("../") || s.starts_with('~') {
        return None;
    }
    // `[v6]:path` and `user@[v6]:path`
    if let Some(close) = s.find("]:") {
        return Some((&s[..close + 1], &s[close + 2..]));
    }
    let (head, path) = s.split_once(':')?;
    (!head.is_empty() && !head.contains('/')).then_some((head, path))
}

/// Remote paths: `~/x` and `~` become home-relative, which rsync and scp understand.
fn remote_path(p: &str) -> String {
    if p == "~" {
        return String::new();
    }
    p.strip_prefix("~/").unwrap_or(p).to_string()
}

/// One step of a transfer.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub program: String,
    pub args: Vec<String>,
    /// Command to run instead if this one fails because rsync is missing on the server.
    pub fallback: Option<Box<Step>>,
    /// A command this one's output goes to (`left | right`); both must succeed.
    pub pipe_to: Option<Box<Step>>,
    /// Done by kraal itself, not by `program` (which with `args` only says what it does).
    pub here: Option<Install>,
    /// The partial files this one keeps (rsync's `--partial-dir`), and how [`run`] looks after
    /// them.
    pub partial: Option<Box<Partial>>,
}

impl Step {
    fn new(program: &str, args: Vec<String>) -> Step {
        Step { program: program.to_string(), args, fallback: None, pipe_to: None, here: None, partial: None }
    }

    pub fn shell_line(&self) -> String {
        let line = std::iter::once(self.program.as_str())
            .chain(self.args.iter().map(|s| s.as_str()))
            .map(crate::shell::quote)
            .collect::<Vec<_>>()
            .join(" ");
        match &self.pipe_to {
            Some(next) => format!("{line} | {}", next.shell_line()),
            None => line,
        }
    }
}

/// How to reach a server: ssh options (without destination) for each server.
pub type SshOptions<'a> = &'a dyn Fn(&Server) -> Vec<String>;

/// How a transfer goes.
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// rsync can be used on this machine (a version with what kraal asks of it, see
    /// [`rsync_capable`]). Without it, files go through tar streams.
    pub rsync: bool,
    /// What already exists in the destination may be replaced: everything (`None`), or only
    /// these names. Anything else stays as it is, even if it appears during the copy.
    pub replace: Option<Vec<String>>,
    /// A move: each source file is deleted once rsync copied it, and only then (`kraal cp --move`).
    pub moving: bool,
    /// Whether the destination is a directory, when the caller already knows; otherwise it is
    /// looked at (on a server, asked) when that decides where a copy is staged.
    pub dest_dir: Option<bool>,
}

impl Options {
    fn may_replace(&self, name: &str) -> bool {
        self.replace.as_ref().is_none_or(|allowed| allowed.iter().any(|a| a == name))
    }
}

/// Where partly transferred files wait, inside the destination directory, until they are
/// complete: an interrupted copy never leaves half a file in place of the old one. The name is
/// the same each time this transfer (these sources, this destination) is run again by the same
/// user on the same machine, so a retry takes up where the interrupted one stopped, and rsync
/// removes the directory once it used what it held. Another machine or user copying the same
/// way gets a name of its own: the lock and the notes of [`Partial`] are kept on each machine,
/// so partial files are never shared, nor removed by another machine's cleanup. rsync leaves a
/// directory of that name out of the copy: being this transfer's own, it is never one of the
/// user's. What a run left that no later run uses (a source renamed or gone meanwhile) is
/// removed once a copy to the same place succeeds (see [`Partial`]).
fn partial_dir(sources: &[Endpoint], dest: &Endpoint) -> String {
    format!("--partial-dir={}", partial_name(partial_key(&client_id(), sources, dest)))
}

/// This machine and user, as far as the system tells.
fn client_id() -> String {
    let machine = crate::process::machine_id().or_else(crate::context::hostname).unwrap_or_default();
    // SAFETY: getuid cannot fail.
    format!("{machine} {}", unsafe { libc::getuid() })
}

/// The key of a transfer's partial files for `client` (see [`partial_dir`]).
fn partial_key(client: &str, sources: &[Endpoint], dest: &Endpoint) -> u64 {
    crate::text::fnv64(&format!("{client}\n{:016x}", transfer_key(sources, dest)))
}

fn partial_name(key: u64) -> String {
    format!(".kraal-partial-{key:016x}")
}

/// What a transfer that keeps partial files needs around its run, done by [`run`]:
/// - one run at a time: a second run of the same transfer waits for the first, instead of
///   writing into the same partial files;
/// - a note kept while it runs and removed when it succeeds, so that what an interrupted run left
///   is found again: when a copy into the same place succeeds, the partial directories of the
///   runs that did not finish (and no longer run) are removed, its own included.
///
/// Both are kept on this machine: runs from another machine are not seen.
#[derive(Debug, Clone, PartialEq)]
pub struct Partial {
    key: u64,
    /// Where the partial directories are (the receiving side and the directory under which they
    /// are), as one text: runs with the same place clean up after each other.
    place: String,
    /// Where they may be (shell words on the receiving side): the receiving directory, and the
    /// destination itself when named without `/`. Kept in the run's note.
    tops: Vec<String>,
    /// How a shell script runs on the receiving side, with this run's connection: the script is
    /// added as the last argument (wrapped for a server's login shell when `remote`).
    runner: Step,
    remote: bool,
}

impl Partial {
    /// The cleanup of the partial directories under `tops` (another run's, from its note: they
    /// are looked for where that run put them, through this run's connection, which works now).
    fn cleanup(&self, tops: &[String]) -> Step {
        let mut step = self.runner.clone();
        let script = cleanup_script(tops);
        step.args.push(if self.remote { crate::shell::sh_c(&script) } else { script });
        step
    }
}

/// The shell script of a [`Partial`]'s cleanup: under each of `tops` (shell words) that is a
/// directory, on its file system, removes the directories named as read on stdin, one per line
/// (partial directory names only). The copy may have gone into either: a destination named
/// without `/` is filled when it is a directory, else its parent receives it. A top that is a
/// link to a directory is followed, as the copy followed it; a link that leads nowhere, or a
/// directory that cannot be entered, fails, so that the notes stay for a later run.
fn cleanup_script(tops: &[String]) -> String {
    format!(
        "names=; while IFS= read -r n; do case $n in .kraal-partial-{hex}) names=\"$names $n\";; esac; done; \
         [ -n \"$names\" ] || exit 0; \
         for t in {tops}; do \
           if [ -d \"$t\" ]; then \
             (cd -P -- \"$t\" || exit 1; set --; for n in $names; do set -- \"$@\" -o -name \"$n\"; done; shift; \
              find . -xdev -type d \\( \"$@\" \\) -prune -exec rm -rf -- {{}} +) || exit 1; \
           elif [ -L \"$t\" ]; then exit 1; fi; \
         done",
        hex = "[0-9a-f]".repeat(16),
        tops = tops.join(" ")
    )
}

/// What a run's note says: its place, and where its partial directories may be.
fn note_text(partial: &Partial) -> String {
    serde_json::json!({ "place": partial.place, "tops": partial.tops }).to_string()
}

/// A note's place and the places it names (none in a note that names only its place).
fn read_note(text: &str) -> (String, Option<Vec<String>>) {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(v) => {
            let place = v["place"].as_str().unwrap_or_default().to_string();
            let tops = v["tops"].as_array().map(|a| a.iter().filter_map(|s| s.as_str().map(String::from)).collect());
            (place, tops)
        }
        Err(_) => (text.lines().next().unwrap_or_default().to_string(), None),
    }
}

/// Where kraal keeps its locks and notes of transfers with partial files.
fn notes_dir() -> PathBuf {
    // Tests run side by side in one process: each (its thread is named after it) has its own.
    #[cfg(test)]
    return std::env::temp_dir().join(format!(
        "kraal-transfer-notes-{}-{:016x}",
        std::process::id(),
        crate::text::fnv64(std::thread::current().name().unwrap_or_default())
    ));
    #[cfg(not(test))]
    crate::paths::state_dir().join("transfers")
}

/// An exclusive flock(2) on `file`; `false` when another holds it and `wait` is false.
fn flock(file: &std::fs::File, wait: bool) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;
    let how = if wait { libc::LOCK_EX } else { libc::LOCK_EX | libc::LOCK_NB };
    loop {
        // SAFETY: flock on a descriptor we own; released when it is closed.
        if unsafe { libc::flock(file.as_raw_fd(), how) } == 0 {
            return Ok(true);
        }
        let e = std::io::Error::last_os_error();
        match e.kind() {
            std::io::ErrorKind::Interrupted => continue,
            std::io::ErrorKind::WouldBlock => return Ok(false),
            _ => return Err(e),
        }
    }
}

fn open_lock(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

/// The lock file `path`, created if needed and locked (waiting for the run that holds it when
/// `wait`); `None` when another run holds it and `wait` is false. A lock file goes with the
/// run that held it last, or every transfer ever made would leave one: one removed while this
/// waited for it is not the lock of the next runs any more, so the file there is locked instead.
fn hold_lock(path: &Path, wait: bool) -> std::io::Result<Option<std::fs::File>> {
    use std::os::unix::fs::MetadataExt;
    loop {
        let file = open_lock(path)?;
        if !flock(&file, wait)? {
            return Ok(None);
        }
        let held = file.metadata()?;
        if std::fs::symlink_metadata(path).is_ok_and(|now| (now.dev(), now.ino()) == (held.dev(), held.ino())) {
            return Ok(Some(file));
        }
    }
}

/// A run of a transfer with partial files, between its start and its end (see [`Partial`]).
struct PartialRun<'a> {
    partial: &'a Partial,
    /// Held for the run, and removed at its end (see [`hold_lock`]).
    lock: PathBuf,
    _held: std::fs::File,
    note: PathBuf,
    /// An earlier run of this transfer did not finish.
    had: bool,
}

impl Drop for PartialRun<'_> {
    fn drop(&mut self) {
        // While it is still held: a run waiting for it then locks a new file.
        let _ = std::fs::remove_file(&self.lock);
    }
}

impl<'a> PartialRun<'a> {
    fn start(partial: &'a Partial) -> Result<PartialRun<'a>> {
        let dir = notes_dir();
        crate::paths::ensure_private_dir(&dir).with_context(|| format!("preparing {}", dir.display()))?;
        let key = format!("{:016x}", partial.key);
        let lock = dir.join(format!("{key}.lock"));
        let held = match hold_lock(&lock, false)? {
            Some(held) => held,
            None => {
                eprintln!("the same copy is already running in another kraal: waiting for it to end");
                loop {
                    if let Some(held) = hold_lock(&lock, true)? {
                        break held;
                    }
                }
            }
        };
        let note = dir.join(format!("{key}.note"));
        let had = note.exists();
        let run = PartialRun { partial, lock, _held: held, note, had };
        crate::paths::write_private(&run.note, note_text(partial).as_bytes())
            .with_context(|| format!("writing {}", run.note.display()))?;
        Ok(run)
    }

    /// The run succeeded: remove the partial directories of the runs into the same place that did
    /// not finish and no longer run, its own earlier ones included, each looked for where the
    /// run that left it put it. A failure here only says so: the copy itself is done, and the
    /// notes of what could not be removed stay for a later run to try again.
    fn finish(self) {
        let dir = notes_dir();
        // Places to look, each with the names to remove there and the notes it settles.
        let mut work: Vec<(Vec<String>, Vec<String>, Vec<PathBuf>)> = Vec::new();
        let mut add =
            |tops: &[String], name: String, note: Option<PathBuf>| match work.iter_mut().find(|(t, _, _)| t == tops) {
                Some((_, names, notes)) => {
                    names.push(name);
                    notes.extend(note);
                }
                None => work.push((tops.to_vec(), vec![name], note.into_iter().collect())),
            };
        let own = &self.partial.tops;
        if self.had {
            add(own, partial_name(self.partial.key), None);
        }
        let mut held = Vec::new();
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let file = entry.file_name().to_string_lossy().into_owned();
            let Some(key) = file.strip_suffix(".note").and_then(|k| u64::from_str_radix(k, 16).ok()) else {
                continue;
            };
            if path == self.note {
                continue;
            }
            let (place, tops) = read_note(&std::fs::read_to_string(&path).unwrap_or_default());
            if place != self.partial.place {
                continue;
            }
            // A run that is still going holds its lock: its partial files are left alone.
            let lock = dir.join(format!("{key:016x}.lock"));
            if let Ok(Some(file)) = hold_lock(&lock, false) {
                add(tops.as_deref().unwrap_or(own), partial_name(key), Some(path));
                held.push((lock, file));
            }
        }
        let mut own_done = true;
        for (tops, names, notes) in &work {
            eprintln!("removing what interrupted copies to here left: {}", names.join(" "));
            match run_with_input(&self.partial.cleanup(tops), &names.join("\n")) {
                Ok(()) => {
                    for note in notes {
                        let _ = std::fs::remove_file(note);
                    }
                }
                Err(e) => {
                    eprintln!("could not remove them ({e:#}); a later copy here tries again");
                    if self.had && tops == own {
                        own_done = false;
                    }
                }
            }
        }
        if own_done {
            let _ = std::fs::remove_file(&self.note);
        }
        // The locks of the runs looked at go too, while held (see [`hold_lock`]).
        for (lock, _held) in held {
            let _ = std::fs::remove_file(lock);
        }
    }
}

/// Run `step` with `input` on its stdin.
fn run_with_input(step: &Step, input: &str) -> Result<()> {
    use std::io::Write;
    let mut child = std::process::Command::new(&step.program)
        .args(&step.args)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("running {}", step.program))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = writeln!(stdin, "{input}");
    }
    let status = child.wait()?;
    if !status.success() {
        bail!("{} failed ({status})", step.program);
    }
    Ok(())
}

/// A number naming one transfer: these sources, this destination.
fn transfer_key(sources: &[Endpoint], dest: &Endpoint) -> u64 {
    let key: Vec<String> = sources.iter().map(Endpoint::display).chain([dest.display()]).collect();
    crate::text::fnv64(&key.join("\n"))
}

/// Shell function `up PATH`: sets `$P` to the directory holding PATH (given without trailing
/// slashes). Unlike `$(dirname …)`, a name ending in a newline keeps it.
const UP: &str = "up() { case $1 in */*) P=${1%/*}; [ -n \"$P\" ] || P=/ ;; *) P=. ;; esac; }; ";

/// Shell functions `dev PATH`, the device PATH itself is on (a link: where the link is), and
/// `devl PATH`, the device of what PATH leads to (a link to a directory: that directory's), with
/// GNU or BSD `stat`. What is renamed counts as itself; where it is renamed to, as what it leads to.
const DEV: &str = "dev() { stat -c %d -- \"$1\" 2>/dev/null || stat -f %d -- \"$1\"; }; \
     devl() { stat -L -c %d -- \"$1\" 2>/dev/null || stat -L -f %d -- \"$1\"; }; ";

/// Shell function `mv_keeps`: whether this server's `mv` can put a directory in place without
/// ever replacing one. GNU coreutils' does from 8.30 on: in a move it first renames with
/// renameat2(RENAME_NOREPLACE), and with `-n` leaves a taken name alone. Any other `mv` checks,
/// then renames: an empty directory appearing in between would be replaced.
const MV_KEEPS: &str = "mv_keeps() { set -- $(mv --version 2>/dev/null \
     | sed -n '1s/^mv (GNU coreutils) \\([0-9]*\\)\\.\\([0-9]*\\).*/\\1 \\2/p'); \
     [ -n \"$1\" ] && { [ \"$1\" -gt 8 ] || { [ \"$1\" -eq 8 ] && [ \"$2\" -ge 30 ]; }; }; }; ";

/// What a copy says when it would have to put a directory in place on a server whose `mv`
/// cannot do it safely (see [`MV_KEEPS`]).
macro_rules! mv_refused {
    () => {
        "this server's mv cannot put a directory in place without the risk of replacing one \
         (it takes GNU coreutils 8.30 or later)"
    };
}

/// Shell function `kraal_put FROM DIR`, on a server: puts FROM into DIR under its own name, and
/// never over anything already there, not even an empty directory. A file or a link is
/// hard-linked (link(2) fails when the name exists), then its staged name removed. A directory is
/// renamed, keeping what it is (owner, group, permissions, ACLs, extended attributes), with
/// `mv -T -n` where [`MV_KEEPS`] says it is safe; elsewhere it is not put in place (a copy that
/// stages a directory checks this before it starts). Needs [`DEV`] and [`MV_KEEPS`].
const PUT: &str = concat!(
    "kraal_put() { t=$2/${1##*/}; if [ -d \"$1\" ] && [ ! -L \"$1\" ]; then \
     [ \"$(devl \"$1\")\" = \"$(devl \"$2\")\" ] || { echo \"$2 is on another file system than $1\" >&2; return 1; }; \
     mv_keeps || { echo \"$t: ",
    mv_refused!(),
    "\" >&2; return 1; }; \
     mv -T -n -- \"$1\" \"$t\" 2>/dev/null; if [ -e \"$1\" ] || [ -L \"$1\" ]; then return 1; fi; \
     else { ln -P -- \"$1\" \"$2\"/ 2>/dev/null || ln -- \"$1\" \"$2\"/; } && rm -f -- \"$1\"; fi; }; "
);

/// Whether this machine's rsync is recent enough for kraal (3.1 or later: `--info=progress2`,
/// `--protect-args`). Older ones (macOS's 2.6.9, openrsync) are not used.
pub fn rsync_capable() -> bool {
    let mut cmd = std::process::Command::new("rsync");
    cmd.arg("--version");
    crate::exec::run(&mut cmd, std::time::Duration::from_secs(5), 64 << 10)
        .ok()
        .filter(|out| out.success())
        .is_some_and(|out| rsync_version_ok(&out.stdout_text()))
}

/// Whether `rsync --version` output names a version 3.1 or later.
fn rsync_version_ok(text: &str) -> bool {
    text.lines().filter(|l| l.starts_with("rsync")).find_map(|l| l.split_once("version")).is_some_and(|(_, rest)| {
        let mut parts = rest.trim().split(|c: char| !c.is_ascii_digit());
        let major: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        let minor: u32 = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
        (major, minor) >= (3, 1)
    })
}

/// The name a source has in the destination directory, and whether it means a directory's
/// contents (`dir/`, `dir/.`, `.`, a server's `~`), as rsync reads them.
fn source_name(e: &Endpoint) -> Result<(String, bool)> {
    let raw = match e {
        Endpoint::Local(p) => p.display().to_string(),
        Endpoint::Remote { path, .. } => path.clone(),
    };
    let contents = raw.ends_with('/')
        || raw.ends_with("/.")
        || raw == "."
        || matches!(e, Endpoint::Remote { .. } if raw.is_empty() || raw == "~");
    let trimmed = raw.trim_end_matches('/');
    let trimmed = trimmed.strip_suffix("/.").unwrap_or(trimmed);
    let last = trimmed.rsplit('/').next().unwrap_or(trimmed);
    if contents {
        // Its name is that of the directory, for messages; what is copied is what it holds.
        let name = match e {
            Endpoint::Local(p) => {
                p.canonicalize().ok().and_then(|c| c.file_name().map(|n| n.to_string_lossy().into_owned()))
            }
            Endpoint::Remote { .. } => None,
        };
        return Ok((name.unwrap_or_else(|| last.to_string()), true));
    }
    match last {
        "" | "." | ".." | "~" => bail!("{raw}: name a file or directory to copy"),
        n => Ok((n.to_string(), false)),
    }
}

/// A local path as an operand: one starting with `-` would be read as an option.
fn local_operand(p: &Path) -> String {
    let s = p.display().to_string();
    if s.starts_with('-') { format!("./{s}") } else { s }
}

/// A remote source path as rsync must get it to take it literally: even with `-s`, the remote
/// rsync expands `*`, `?` and `[...]` itself (`backup[1]` would be `backup1`), unless they are
/// escaped. A path without them is taken as it is, backslashes included.
fn rsync_literal(path: &str) -> String {
    if !path.contains(['*', '?', '[']) {
        return path.to_string();
    }
    let mut out = String::with_capacity(path.len() + 8);
    for c in path.chars() {
        if matches!(c, '\\' | '*' | '?' | '[' | ']') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `user@host` for rsync: an IPv6 address needs brackets before the `:path`.
fn rsync_host(server: &Server) -> String {
    let host = if server.host.contains(':') && !server.host.starts_with('[') {
        format!("[{}]", server.host)
    } else {
        server.host.clone()
    };
    match &server.user {
        Some(u) => format!("{u}@{host}"),
        None => host,
    }
}

/// Build the commands that copy (or move) `sources` to `dest`. Sources that may replace what is
/// in the destination are copied there directly. Those that must not are copied into a private
/// staging directory next to it, then each put in place by an operation that fails when the name
/// is taken (see [`PUT`]): a file that appears during the copy is not replaced either, and the
/// copy waits in the staging directory instead.
pub fn plan(sources: &[Endpoint], dest: &Endpoint, ssh: SshOptions, opts: &Options) -> Result<Vec<Step>> {
    if sources.is_empty() {
        bail!("nothing to copy");
    }
    let remote_servers: Vec<&Server> = sources
        .iter()
        .filter_map(|s| match s {
            Endpoint::Remote { server, .. } => Some(server),
            Endpoint::Local(_) => None,
        })
        .collect();
    let all_local = remote_servers.is_empty();
    let all_remote = remote_servers.len() == sources.len();
    if !all_local && !all_remote {
        bail!("copy local files and remote files in separate commands");
    }
    if all_remote && remote_servers.iter().any(|s| !s.same_connection(remote_servers[0])) {
        bail!("all remote sources must be on the same server");
    }
    for s in sources {
        if let Endpoint::Local(p) = s
            && p.symlink_metadata().is_err()
        {
            bail!("{} does not exist", p.display());
        }
    }
    // Several sources go into the destination directory; one alone may be renamed to it.
    let dest_text = match dest {
        Endpoint::Local(d) => d.display().to_string(),
        Endpoint::Remote { path, .. } => path.clone(),
    };
    let into_dir = sources.len() > 1 || dest_text.ends_with('/');
    let mut steps = Vec::new();
    for keep in [false, true] {
        let mut group = Vec::new();
        for s in sources {
            if opts.may_replace(&source_name(s)?.0) != keep {
                group.push(s.clone());
            }
        }
        if !group.is_empty() {
            let how = if keep { Group::Keep } else { Group::Replace };
            steps.extend(plan_group(&group, dest, ssh, opts, how, into_dir)?);
        }
    }
    Ok(steps)
}

/// How a group of sources reaches its destination.
#[derive(Clone, Copy, PartialEq)]
enum Group {
    /// Copied in place, replacing what has their names.
    Replace,
    /// Never replacing anything: staged, then put in place (see [`plan_staged`]).
    Keep,
    /// Into a new, private staging directory: nothing else is ever there, so an interrupted
    /// transfer needs no partial-file directory, and no name is kept for one.
    Stage,
}

/// The commands for sources that share one rule.
fn plan_group(
    sources: &[Endpoint],
    dest: &Endpoint,
    ssh: SshOptions,
    opts: &Options,
    how: Group,
    into_dir: bool,
) -> Result<Vec<Step>> {
    if how == Group::Keep {
        return plan_staged(sources, dest, ssh, opts, into_dir);
    }
    let rsync_args = |remote: Option<&[String]>| {
        let mut args = vec!["-a".to_string(), "--info=progress2".to_string()];
        if let Some(ssh_opts) = remote {
            args.push("-s".to_string());
            if how == Group::Replace {
                args.push(partial_dir(sources, dest));
            }
            args.extend(["-e".to_string(), ssh_e(ssh_opts)]);
        }
        if opts.moving {
            // Only what was copied is deleted, and a file is compared by content, not by size and
            // date: one skipped as already there is identical.
            args.extend(["--remove-source-files".to_string(), "--checksum".to_string()]);
        }
        args.push("--".into());
        args
    };
    let local_paths = || -> Vec<String> {
        sources
            .iter()
            .filter_map(|e| match e {
                Endpoint::Local(p) => Some(local_operand(p)),
                _ => None,
            })
            .collect()
    };
    let remote_paths = || -> Vec<String> {
        sources
            .iter()
            .filter_map(|e| match e {
                Endpoint::Remote { path, .. } => Some(remote_path(path)),
                _ => None,
            })
            .collect()
    };
    // Directory semantics forced on the destination operand when needed.
    let as_dir = |d: String| if into_dir && !d.ends_with('/') { format!("{d}/") } else { d };
    let single = (!into_dir).then(|| source_name(&sources[0])).transpose()?.filter(|(_, contents)| !contents);
    let single = single.map(|(name, _)| name);
    match (&sources[0], dest) {
        (Endpoint::Local(_), Endpoint::Local(d)) => {
            if opts.rsync {
                let mut args = rsync_args(None);
                args.extend(local_paths());
                args.push(as_dir(local_operand(d)));
                return Ok(vec![Step::new("rsync", args)]);
            }
            if opts.moving {
                bail!("moving across file systems needs rsync 3.1 or later here: copy, then delete");
            }
            let mut steps = Vec::new();
            if into_dir {
                steps.push(Step::new("mkdir", vec!["-p".into(), "--".into(), local_operand(d)]));
            }
            // Links are copied as links, never followed.
            let mut args = vec!["-RP".to_string(), "--".to_string()];
            args.extend(local_paths());
            args.push(local_operand(d));
            steps.push(Step::new("cp", args));
            Ok(steps)
        }
        (Endpoint::Local(_), Endpoint::Remote { server, path }) => {
            let opts_ssh = ssh(server);
            let tar = upload_tar(sources, server, &opts_ssh, path, single.as_deref())?;
            if !opts.rsync {
                return refuse_tar_move(opts, tar);
            }
            let mut args = rsync_args(Some(&opts_ssh));
            args.extend(local_paths());
            args.push(as_dir(format!("{}:{}", rsync_host(server), remote_path(path))));
            let mut step = Step::new("rsync", args);
            if how == Group::Replace {
                // Partial directories are under the directory the sources go into.
                let rel = trim_dir(&remote_path(path)).to_string();
                let mut tops = vec![remote_abs_dir(&rel)];
                if !into_dir {
                    tops.push(remote_abs_dir(&split_parent(&rel).0));
                }
                // Runs into the same directory know each other by it (the last top): the
                // destination itself is only one more place to look.
                let top = tops.last().cloned().unwrap_or_default();
                let mut on = opts_ssh.clone();
                on.extend(["-T".into(), server.destination()]);
                step.partial = Some(Box::new(Partial {
                    key: partial_key(&client_id(), sources, dest),
                    place: format!("{}:{} {top}", server.destination(), server.port.unwrap_or(22)),
                    tops,
                    runner: Step::new("ssh", on),
                    remote: true,
                }));
            }
            Ok(vec![with_fallback(step, tar, opts)])
        }
        (Endpoint::Remote { server, .. }, Endpoint::Local(d)) => {
            let opts_ssh = ssh(server);
            let tar = download_tar(sources, server, &opts_ssh, d, single.as_deref())?;
            if !opts.rsync {
                return refuse_tar_move(opts, tar);
            }
            let mut args = rsync_args(Some(&opts_ssh));
            args.extend(remote_paths().into_iter().map(|p| format!("{}:{}", rsync_host(server), rsync_literal(&p))));
            args.push(as_dir(local_operand(d)));
            let mut step = Step::new("rsync", args);
            if how == Group::Replace {
                let d = std::path::absolute(PathBuf::from(trim_dir(&d.display().to_string())))?;
                let mut dirs = vec![d.clone()];
                if !into_dir {
                    dirs.extend(d.parent().map(Path::to_path_buf));
                }
                let tops: Vec<String> =
                    dirs.iter().map(|d| crate::shell::quote_always(&d.display().to_string())).collect();
                let top = tops.last().cloned().unwrap_or_default();
                step.partial = Some(Box::new(Partial {
                    key: partial_key(&client_id(), sources, dest),
                    place: format!("here {top}"),
                    tops,
                    runner: Step::new("sh", vec!["-c".into()]),
                    remote: false,
                }));
            }
            Ok(vec![with_fallback(step, tar, opts)])
        }
        (Endpoint::Remote { server: from, .. }, Endpoint::Remote { server: to, path: to_path }) => {
            if from.same_connection(to) {
                // Same server: copy (or move) there.
                let program = if opts.moving { "mv" } else { "cp -RP" };
                let mut line = String::new();
                let target = remote_shell_path(&remote_path(to_path));
                if into_dir {
                    line.push_str(&format!("mkdir -p -- {target} || exit 1; "));
                }
                if opts.moving {
                    // Across file systems mv copies, then deletes the whole source, with what
                    // appeared in it meanwhile: only renames are done here. The sources go into
                    // the destination when it is a directory, else next to it.
                    line.push_str(&format!(
                        "{UP}{DEV}if [ {} = 1 ] || [ -d {target} ]; then T={target}; else up {target}; T=$P; fi; \
                         t=$(devl \"$T\") || exit 1; \
                         for s in {}; do d=$(dev \"$s\") || exit 1; if [ \"$d\" != \"$t\" ]; then \
                         echo \"$s is on another file system than the destination: copy, then delete\" >&2; exit 1; fi; done; ",
                        u8::from(into_dir),
                        remote_paths().iter().map(|p| remote_shell_path(p)).collect::<Vec<_>>().join(" ")
                    ));
                }
                line.push_str(program);
                line.push_str(" --");
                for p in remote_paths() {
                    line.push(' ');
                    line.push_str(&remote_shell_path(&p));
                }
                line.push(' ');
                line.push_str(&target);
                let mut args = ssh(from);
                args.push(from.destination());
                args.push(crate::shell::sh_c(&line));
                return Ok(vec![Step::new("ssh", args)]);
            }
            if opts.moving {
                bail!("moving from one server to another is not supported: copy, then delete");
            }
            // Two servers: tar on one side, untar on the other, streamed through here.
            let mut left = ssh(from);
            left.push(from.destination());
            left.push(crate::shell::sh_c(&pack_remote(&remote_paths())));
            let mut right = ssh(to);
            right.push(to.destination());
            let dest_dir = remote_abs_dir(&remote_path(to_path));
            right.push(crate::shell::sh_c(&unpack_script(&dest_dir, single.as_deref())));
            let mut step = Step::new("ssh", left);
            step.pipe_to = Some(Box::new(Step::new("ssh", right)));
            Ok(vec![step])
        }
    }
}

/// What a staging directory of this version holds in its `format` file. Anything else found where
/// a staging directory would be (an older kraal's, one not finished being made) is never removed.
const STAGE_FORMAT: &str = "kraal-stage 2";

/// The start of the names of the staging directories of one transfer (same sources, same
/// destination); each run adds its own `-<32 hex>`.
fn stage_prefix(sources: &[Endpoint], dest: &Endpoint) -> String {
    format!(".kraal-stage-{:016x}", transfer_key(sources, dest))
}

/// This process as a staging directory records who made it: machine, boot, process id, start
/// time. `None` when the system cannot say: such a staging directory is never judged abandoned.
fn owner_token() -> Option<String> {
    let machine = crate::process::machine_id()?;
    let start = crate::process::start_time(std::process::id())?;
    Some(format!("{machine} {} {} {start}", word_or_dash(&crate::process::boot_id()), std::process::id()))
}

fn word_or_dash(s: &str) -> &str {
    if s.is_empty() { "-" } else { s }
}

/// The processes running on this machine now, whatever their name, as ` pid:start ` words
/// (` pid:? ` when the system does not say when one started). A staging directory made on this
/// machine and boot is over only when its maker started before this process and is not among
/// them: it had ended by the time the list was made. One whose maker started later may well run
/// though the list, made before, does not show it: it is kept.
fn live_processes() -> String {
    let mut out = String::from(" ");
    for pid in crate::process::pids() {
        match crate::process::start_time(pid) {
            Some(start) => out.push_str(&format!("{pid}:{start} ")),
            None => out.push_str(&format!("{pid}:? ")),
        }
    }
    out
}

/// Sources that must not replace anything: copied into a private staging directory in the
/// directory they go to (one file system), then each put in place by a rename or a link that
/// never takes a name that is taken. A name taken meanwhile leaves its item staged, and the
/// command fails saying where it waits.
///
/// Each run stages anew, and never writes into, puts in place or removes what another run
/// staged, unless it is sure that run is over: a staging directory of this format, made by a
/// copy (its sources are still where they were), on this machine, by a kraal that started before
/// this one and has ended (see [`live_processes`]).
/// One left by a move (it holds the only copy of what it moved), by an older kraal or by a copy
/// still running stays as it is; a move refuses while one is there, and names it. What the copy
/// writes (`payload`) is kept apart from kraal's own files (`format`, `kind`, `owner`, `work`): no
/// name a source may have is taken by them.
fn plan_staged(
    sources: &[Endpoint],
    dest: &Endpoint,
    ssh: SshOptions,
    opts: &Options,
    into_dir: bool,
) -> Result<Vec<Step>> {
    let prefix = stage_prefix(sources, dest);
    let run = crate::accounts::new_uuid().replace('-', "");
    let stage_name = format!("{prefix}-{run}");
    // One source to a name that is not a directory takes that name; otherwise the sources go
    // into the destination, which an existing directory is, whether or not it ends in `/`.
    let single = match sources {
        [one] if !into_dir => Some(source_name(one)?).filter(|(_, contents)| !contents).map(|(n, _)| n),
        _ => None,
    };
    let into = single.is_none()
        || match (opts.dest_dir, dest) {
            (Some(known), _) => known,
            (None, Endpoint::Local(d)) => d.is_dir(),
            (None, Endpoint::Remote { server, path }) => remote_is_dir(server, ssh, path)?,
        };
    let single = single.filter(|_| !into);
    // Where the sources end up and the staging directory, as shell words; where the copy writes;
    // and the name a single source takes.
    let (final_dir, stage_word, payload, new_name, local) = match dest {
        Endpoint::Local(d) => {
            let trimmed = PathBuf::from(trim_dir(&d.display().to_string()));
            let dir = if into {
                trimmed.clone()
            } else {
                trimmed.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")).to_path_buf()
            };
            let stage = dir.join(&stage_name);
            let word = |p: &Path| crate::shell::quote_always(&p.display().to_string());
            let name = trimmed.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let payload = Endpoint::Local(PathBuf::from(format!("{}/payload/", stage.display())));
            (word(&dir), word(&stage), payload, name, Some((dir, stage)))
        }
        Endpoint::Remote { server, path } => {
            let rel = trim_dir(&remote_path(path)).to_string();
            let dir = if into { rel.clone() } else { split_parent(&rel).0 };
            let stage = match dir.as_str() {
                "" => stage_name.clone(),
                "/" => format!("/{stage_name}"),
                d => format!("{d}/{stage_name}"),
            };
            let payload = Endpoint::Remote { server: server.clone(), path: format!("{stage}/payload/") };
            (remote_abs_dir(&dir), remote_abs_dir(&stage), payload, split_parent(&rel).1, None)
        }
    };
    let kind = if opts.moving { "move" } else { "copy" };
    let owner = owner_token();
    let (machine, boot) = match &owner {
        Some(o) => {
            let mut words = o.split(' ');
            (words.next().unwrap_or("").to_string(), words.next().unwrap_or("").to_string())
        }
        None => (String::new(), String::new()),
    };
    // What another run left: a copy's staging is removed if that copy is sure to be over, else
    // left as it is; anything else (a move's, an older kraal's, one half made) is left as it is,
    // and a move refuses while it is there.
    let other = if opts.moving {
        "echo \"$o: an earlier move to here did not finish, or this staging is not one kraal can tell \
         is over: it may hold the only copy of what it moved; put it in place or remove it, then try \
         again (nothing was done)\" >&2; exit 1;"
    } else {
        "echo \"$o: staging of an earlier move or of another kraal, left as it is\" >&2;"
    };
    // On a server whose `mv` cannot put a directory in place safely, a copy that may stage one
    // stops before anything is staged or moved: a move would otherwise leave what it moved
    // waiting in its staging directory.
    let directories = match dest {
        Endpoint::Remote { .. } => format!(
            "{MV_KEEPS}need=; {}if [ -n \"$need\" ] && ! mv_keeps; then echo {} >&2; exit 1; fi; ",
            directory_test(sources, dest, &|server, script| remote_output_asking(server, ssh, script))?,
            crate::shell::quote(concat!(mv_refused!(), ": nothing was done"))
        ),
        Endpoint::Local(_) => String::new(),
    };
    let prepare = format!(
        "{directories}d={final_dir}; s={stage_word}; me={machine}; boot={boot}; since={since}; live={live}; \
         mkdir -p -- \"$d\" || exit 1; \
         for o in \"$d\"/{prefix}*; do [ -e \"$o\" ] || [ -L \"$o\" ] || continue; k=; w=; \
         case ${{o##*/}} in {prefix}-{hex}) \
         if [ -d \"$o\" ] && [ ! -L \"$o\" ] && [ -O \"$o\" ] && [ -d \"$o\"/payload ] && [ -d \"$o\"/work ] \
         && [ \"$(cat -- \"$o\"/format 2>/dev/null)\" = {format} ]; then \
         k=$(cat -- \"$o\"/kind 2>/dev/null); w=$(cat -- \"$o\"/owner 2>/dev/null); fi;; esac; \
         if [ \"$k\" = copy ]; then \
         if [ -n \"$me\" ]; then set -f; set -- $w; set +f; \
         if [ $# = 4 ] && [ \"$1\" = \"$me\" ]; then \
         if [ \"$2\" != \"$boot\" ]; then rm -rf -- \"$o\" || exit 1; continue; fi; \
         if [ \"$4\" -lt \"$since\" ] 2>/dev/null; then \
         case \"$live\" in *\" $3:$4 \"*|*\" $3:? \"*) ;; *) rm -rf -- \"$o\" || exit 1; continue;; esac; fi; fi; fi; \
         echo \"$o: staging of a copy that may still be running (or made on another machine), left as it is\" >&2; continue; fi; \
         {other} done; \
         mkdir -m 700 -- \"$s\" && mkdir -- \"$s\"/payload \"$s\"/work && printf '%s\\n' {owner} > \"$s\"/owner \
         && echo {kind} > \"$s\"/kind && echo {format} > \"$s\"/format \
         || {{ echo \"could not prepare $s: nothing was done\" >&2; exit 1; }}",
        machine = crate::shell::quote(&machine),
        boot = crate::shell::quote(&boot),
        since = crate::shell::quote(
            &crate::process::start_time(std::process::id()).map(|s| s.to_string()).unwrap_or_default()
        ),
        live = crate::shell::quote(&live_processes()),
        hex = "[0-9a-f]".repeat(32),
        format = crate::shell::quote(STAGE_FORMAT),
        owner = crate::shell::quote(owner.as_deref().unwrap_or("unknown")),
    );
    let waits = if opts.moving {
        "not put in place, as something already has its name: what was moved waits in $s, and it is the only copy"
    } else {
        "not replaced: what is already there stays; the copy waits in $s"
    };
    let script_step = |script: String| match dest {
        Endpoint::Local(_) => Step::new("sh", vec!["-c".into(), script]),
        Endpoint::Remote { server, .. } => {
            let mut args = ssh(server);
            args.extend(["-T".into(), server.destination(), crate::shell::sh_c(&script)]);
            Step::new("ssh", args)
        }
    };
    let install = match local {
        // Here, kraal renames with a call that refuses to replace anything.
        Some((dir, stage)) => {
            let rename = single.clone().map(|src| (src, new_name.clone()));
            let mut args = vec!["put".to_string(), stage.display().to_string(), dir.display().to_string()];
            args.extend(rename.iter().flat_map(|(a, b)| [a.clone(), b.clone()]));
            let install = Install { stage, dir, rename, moving: opts.moving };
            Step { here: Some(install), ..Step::new("kraal", args) }
        }
        None => {
            // A single source is renamed in the private work directory first, then put in place
            // like the others. Every other entry of `payload` is a source: it holds nothing else.
            let place = match &single {
                Some(src) => {
                    let (src, new) = (crate::shell::quote(src), crate::shell::quote(&new_name));
                    format!(
                        "mv -- \"$s\"/payload/{src} \"$s\"/work/{new} && kraal_put \"$s\"/work/{new} \"$d\" || fail=1; "
                    )
                }
                None => "for e in \"$s\"/payload/* \"$s\"/payload/.[!.]* \"$s\"/payload/..?*; do \
                         [ -e \"$e\" ] || [ -L \"$e\" ] || continue; kraal_put \"$e\" \"$d\" || fail=1; done; "
                    .to_string(),
            };
            script_step(format!(
                "{DEV}{MV_KEEPS}{PUT}d={final_dir}; s={stage_word}; fail=; {place}\
                 rmdir -- \"$s\"/work \"$s\"/payload 2>/dev/null || fail=1; \
                 if [ -n \"$fail\" ]; then echo \"{waits}\" >&2; exit 1; fi; \
                 rm -f -- \"$s\"/format \"$s\"/kind \"$s\"/owner && rmdir -- \"$s\""
            ))
        }
    };
    let mut steps = vec![script_step(prepare)];
    steps.extend(plan_group(sources, &payload, ssh, opts, Group::Stage, true)?);
    steps.push(install);
    Ok(steps)
}

/// Shell words, run on the server `dest` is on, that set `need=1` when a source may be a
/// directory to put in place there: known here for this machine's files, looked at on the server
/// for its own, asked of another server for its own (once, before anything is done; taken as
/// likely when it does not answer). `ask` runs a script on a server and returns what it printed.
fn directory_test(
    sources: &[Endpoint],
    dest: &Endpoint,
    ask: &dyn Fn(&Server, &str) -> Result<String>,
) -> Result<String> {
    let Endpoint::Remote { server: to, .. } = dest else { return Ok(String::new()) };
    let is_dir = |p: &Path| p.symlink_metadata().is_ok_and(|m| m.is_dir());
    let mut out = String::new();
    let mut elsewhere: Option<(&Server, String)> = None;
    for source in sources {
        let (_, contents) = source_name(source)?;
        match source {
            Endpoint::Local(p) => {
                let may = if contents {
                    std::fs::read_dir(p).map(|d| d.flatten().any(|e| is_dir(&e.path()))).unwrap_or(true)
                } else {
                    is_dir(p)
                };
                if may {
                    out.push_str("need=1; ");
                }
            }
            Endpoint::Remote { server, path } if server.same_connection(to) => {
                out.push_str(&remote_directory_test(&remote_path(path)))
            }
            // All the remote sources of a copy are on one server.
            Endpoint::Remote { server, path } => {
                elsewhere.get_or_insert_with(|| (server, String::new())).1 += &remote_directory_test(&remote_path(path))
            }
        }
    }
    if let Some((server, test)) = elsewhere {
        let answer = ask(server, &format!("need=; {test}echo \"$need\""));
        if answer.map_or(true, |a| a.trim() == "1") {
            out.push_str("need=1; ");
        }
    }
    Ok(out)
}

/// Shell words, run on the server `path` is on, that set `need=1` when it is a directory (for
/// `dir/`, when it holds one).
fn remote_directory_test(path: &str) -> String {
    match contents_of(path) {
        Some(dir) => format!(
            "for e in {w}/* {w}/.[!.]* {w}/..?*; do [ -d \"$e\" ] && [ ! -L \"$e\" ] && need=1; done; ",
            w = remote_abs_dir(&dir)
        ),
        None => {
            let w = remote_abs_dir(trim_dir(path));
            format!("[ -d {w} ] && [ ! -L {w} ] && need=1; ")
        }
    }
}

/// A staged copy to put in place on this machine (see [`plan_staged`]), by kraal itself: each
/// item is renamed with renameat2(RENAME_NOREPLACE) (macOS: renamex_np(RENAME_EXCL)), which fails
/// when the name is taken, so nothing is ever replaced, and a directory stays what it is (owner,
/// group, permissions, ACLs, extended attributes).
#[derive(Debug, Clone, PartialEq)]
pub struct Install {
    stage: PathBuf,
    dir: PathBuf,
    /// A single source and the name it takes.
    rename: Option<(String, String)>,
    moving: bool,
}

fn install_here(i: &Install) -> Result<()> {
    let payload = i.stage.join("payload");
    let items: Vec<(PathBuf, PathBuf)> = match &i.rename {
        Some((src, new)) => vec![(payload.join(src), i.dir.join(new))],
        None => std::fs::read_dir(&payload)
            .with_context(|| format!("reading {}", payload.display()))?
            .map(|e| e.map(|e| (e.path(), i.dir.join(e.file_name()))))
            .collect::<std::io::Result<_>>()
            .with_context(|| format!("reading {}", payload.display()))?,
    };
    let mut waiting = false;
    for (from, to) in items {
        match crate::paths::rename_no_replace(&from, &to) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => waiting = true,
            Err(e) => {
                eprintln!("{}: {e}", to.display());
                waiting = true;
            }
        }
    }
    let stage = i.stage.display();
    if waiting {
        if i.moving {
            bail!(
                "not put in place, as something already has its name: what was moved waits in {stage}, and it is the only copy"
            );
        }
        bail!("not replaced: what is already there stays; the copy waits in {stage}");
    }
    for f in ["format", "kind", "owner"] {
        std::fs::remove_file(i.stage.join(f)).with_context(|| format!("cleaning {stage}"))?;
    }
    for d in [payload, i.stage.join("work"), i.stage.clone()] {
        std::fs::remove_dir(&d).with_context(|| format!("cleaning {stage}"))?;
    }
    Ok(())
}

/// Whether `path` on `server` is a directory (or a link to one).
fn remote_is_dir(server: &Server, ssh: SshOptions, path: &str) -> Result<bool> {
    let d = remote_abs_dir(trim_dir(&remote_path(path)));
    let out = remote_output(server, ssh, &format!("if [ -d {d} ]; then echo dir; fi; exit 0"))
        .with_context(|| format!("could not check {}:{path}", server.name))?;
    Ok(out.lines().any(|l| l == "dir"))
}

/// The tar stream that replaces rsync when it is missing, for a copy; a move refuses.
fn refuse_tar_move(opts: &Options, tar: Step) -> Result<Vec<Step>> {
    if opts.moving {
        bail!("moving needs rsync 3.1 or later on this machine: copy, then delete");
    }
    Ok(vec![tar])
}

/// rsync, with the tar stream as fallback when the server has no rsync. A move has none: only
/// rsync deletes exactly what it copied.
fn with_fallback(rsync: Step, tar: Step, opts: &Options) -> Step {
    if opts.moving { rsync } else { Step { fallback: Some(Box::new(tar)), ..rsync } }
}

/// `tar cf -` of local sources, each from its own directory (`dir/` packs what is in dir).
/// Members start with `./`: a name starting with `-` is not an option. Links stay links. Each
/// `-C` is absolute: a relative one would be taken from the previous `-C`.
fn pack_local(sources: &[Endpoint]) -> Result<Step> {
    let mut args = vec!["cf".to_string(), "-".to_string()];
    for s in sources {
        let Endpoint::Local(p) = s else { continue };
        let (name, contents) = source_name(s)?;
        let dir = if contents {
            p.clone()
        } else {
            p.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new(".")).to_path_buf()
        };
        let dir = std::path::absolute(&dir).with_context(|| format!("{}", dir.display()))?;
        let member = if contents { ".".to_string() } else { format!("./{name}") };
        args.extend(["-C".to_string(), dir.display().to_string(), member]);
    }
    Ok(Step::new("tar", args))
}

/// The same on a server, for paths relative to home or absolute. Each `-C` is absolute: a
/// relative one would be taken from the previous `-C`.
fn pack_remote(paths: &[String]) -> String {
    let mut pack = String::from("tar cf -");
    for p in paths {
        match contents_of(p) {
            Some(dir) => pack.push_str(&format!(" -C {} .", remote_abs_dir(&dir))),
            None => {
                let (dir, name) = split_parent(p);
                pack.push_str(&format!(" -C {} {}", remote_abs_dir(&dir), crate::shell::quote(&format!("./{name}"))));
            }
        }
    }
    pack
}

/// The directory whose contents a remote source path means (`dir/`, `dir/.`, `.`, home, the
/// root `/`), if it means contents.
fn contents_of(p: &str) -> Option<String> {
    let dir = if p.is_empty() || p == "." {
        ""
    } else if let Some(d) = p.strip_suffix("/.") {
        d
    } else if p.ends_with('/') {
        trim_dir(p)
    } else {
        return None;
    };
    // `/.` and `/` are the root, not home.
    Some(if dir.is_empty() && p.starts_with('/') { "/".to_string() } else { dir.to_string() })
}

/// Shell script that unpacks a tar stream from stdin into `dest` (a shell word). As with rsync,
/// an existing directory (or a name ending in `/`, or several sources: `single` is `None`)
/// receives the sources; otherwise the one source takes `dest` as its name.
fn unpack_script(dest: &str, single: Option<&str>) -> String {
    let into = format!("mkdir -p -- {dest} && tar xf - -C {dest}");
    let Some(name) = single else { return into };
    let name = crate::shell::quote(name);
    format!(
        "{UP}if [ -d {dest} ]; then {into}; exit; fi; \
         up {dest}; mkdir -p -- \"$P\" && t=$(mktemp -d \"$P/.kraal-XXXXXX\") || exit 1; \
         tar xf - -C \"$t\"; s=$?; \
         if [ $s = 0 ]; then if [ -d \"$t\"/{name} ] && [ ! -L \"$t\"/{name} ]; \
         then mkdir -p -- {dest} && mv -- \"$t\"/{name} {dest}/; else mv -- \"$t\"/{name} {dest}; fi; s=$?; fi; \
         rm -rf -- \"$t\"; exit $s"
    )
}

/// Local sources to a server through tar, for when rsync is missing on either side.
fn upload_tar(
    sources: &[Endpoint],
    server: &Server,
    ssh_opts: &[String],
    path: &str,
    single: Option<&str>,
) -> Result<Step> {
    let mut right = ssh_opts.to_vec();
    right.push(server.destination());
    right.push(crate::shell::sh_c(&unpack_script(&remote_abs_dir(&remote_path(path)), single)));
    let mut step = pack_local(sources)?;
    step.pipe_to = Some(Box::new(Step::new("ssh", right)));
    Ok(step)
}

/// Remote sources to this machine through tar.
fn download_tar(
    sources: &[Endpoint],
    server: &Server,
    ssh_opts: &[String],
    dest: &Path,
    single: Option<&str>,
) -> Result<Step> {
    let paths: Vec<String> = sources
        .iter()
        .filter_map(|e| match e {
            Endpoint::Remote { path, .. } => Some(remote_path(path)),
            _ => None,
        })
        .collect();
    let mut left = ssh_opts.to_vec();
    left.push(server.destination());
    left.push(crate::shell::sh_c(&pack_remote(&paths)));
    let dest = crate::shell::quote_always(&dest.display().to_string());
    let mut step = Step::new("ssh", left);
    step.pipe_to = Some(Box::new(Step::new("sh", vec!["-c".into(), unpack_script(&dest, single)])));
    Ok(step)
}

/// `-e` value for rsync: the ssh command with its options.
fn ssh_e(opts: &[String]) -> String {
    crate::shell::join(&[&["ssh".to_string()], opts].concat())
}

/// A remote path for the remote shell: relative paths stay relative to home.
fn remote_shell_path(p: &str) -> String {
    if p.is_empty() { ".".to_string() } else { crate::shell::quote(p) }
}

/// A remote directory for the remote shell, always absolute (relative ones are under home).
fn remote_abs_dir(p: &str) -> String {
    match p {
        "" | "." | "~" => "\"$HOME\"".to_string(),
        _ if p.starts_with('/') => crate::shell::quote(p),
        _ => format!("\"$HOME\"/{}", crate::shell::quote(p.trim_start_matches("~/"))),
    }
}

/// `path` without its trailing slashes, except the root itself.
fn trim_dir(path: &str) -> &str {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() && path.starts_with('/') { "/" } else { trimmed }
}

/// The names among `sources` that already exist in the directory `dest`: what a copy into it would
/// replace. A server's directory is asked in one ssh command. Anything that cannot be told for
/// sure counts as there (a copy that must not replace refuses rather than guesses): a `dest`
/// that is a file, an entry that cannot be examined, a source whose name is not a plain name.
pub fn existing_in(sources: &[Endpoint], dest: &Endpoint, ssh: SshOptions) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for s in sources {
        let raw = match s {
            Endpoint::Local(p) => p.display().to_string(),
            Endpoint::Remote { path, .. } => path.clone(),
        };
        // "dir/" copies what is in dir, not dir: its names are not known here.
        let name = source_name(s).ok().filter(|(_, contents)| !contents).map(|(n, _)| n);
        let Some(name) = name else {
            bail!("{raw}: name the file or directory itself (without a trailing /) to copy without replacing");
        };
        names.push(name);
    }
    match dest {
        Endpoint::Local(dir) => match dir.symlink_metadata() {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => bail!("could not check {}: {e}", dir.display()),
            Ok(m) if !m.is_dir() && !dir.is_dir() => bail!("{} is a file: copying would replace it", dir.display()),
            Ok(_) => Ok(names
                .into_iter()
                .filter(
                    |n| !matches!(dir.join(n).symlink_metadata(), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
                )
                .collect()),
        },
        Endpoint::Remote { server, path } => {
            let quoted: Vec<String> = names.iter().map(|n| crate::shell::quote(n)).collect();
            let d = remote_abs_dir(trim_dir(path));
            let script = format!(
                "if [ ! -e {d} ] && [ ! -L {d} ]; then exit 0; fi; \
                 if [ ! -d {d} ]; then echo 'is a file: copying would replace it' >&2; exit 3; fi; \
                 cd -- {d} || exit 3; \
                 for f in {}; do if [ -e \"$f\" ] || [ -L \"$f\" ]; then printf '%s\\n' \"$f\"; fi; done",
                quoted.join(" ")
            );
            let out =
                remote_output(server, ssh, &script).with_context(|| format!("could not check {}", dest.display()))?;
            Ok(out.lines().filter(|l| !l.is_empty()).map(str::to_string).collect())
        }
    }
}

/// What a POSIX script prints on a server (asked once, without prompts), or why it failed.
fn remote_output(server: &Server, ssh: SshOptions, script: &str) -> Result<String> {
    let mut args = ssh(server);
    args.extend(["-o".into(), "BatchMode=yes".into(), "-T".into(), server.destination(), crate::shell::sh_c(script)]);
    let out =
        crate::exec::run(std::process::Command::new("ssh").args(&args), std::time::Duration::from_secs(30), 1 << 20)?;
    if !out.success() {
        bail!("{}", crate::exec::last_stderr_line(&out));
    }
    Ok(out.stdout_text())
}

/// [`remote_output`] for a server the copy itself reaches next, which may ask for a password
/// or a key's passphrase: ssh may prompt in the terminal, as the copy's own ssh would.
fn remote_output_asking(server: &Server, ssh: SshOptions, script: &str) -> Result<String> {
    let mut args = ssh(server);
    args.extend(["-T".into(), server.destination(), crate::shell::sh_c(script)]);
    let out = crate::exec::run_interactive(
        std::process::Command::new("ssh").args(&args),
        std::process::Stdio::null(),
        std::time::Duration::from_secs(300),
        1 << 20,
    )?;
    if !out.success() {
        bail!("{}", crate::exec::last_stderr_line(&out));
    }
    Ok(out.stdout_text())
}

fn split_parent(p: &str) -> (String, String) {
    let trimmed = p.trim_end_matches('/');
    match trimmed.rsplit_once('/') {
        Some(("", name)) => ("/".to_string(), name.to_string()),
        Some((dir, name)) => (dir.to_string(), name.to_string()),
        None => (String::new(), trimmed.to_string()),
    }
}

// ---- moves --------------------------------------------------------------------------------

/// Refuse a move that would lose its sources: one whose destination is the source itself or
/// inside it (seen through another path too: a symbolic link, a bind mount), where a copy would
/// have nothing to do before the source is deleted.
pub fn check_move(sources: &[Endpoint], dest: &Endpoint) -> Result<()> {
    let Endpoint::Local(d) = dest else { return Ok(()) };
    for s in sources {
        let Endpoint::Local(p) = s else { continue };
        let (name, _) = source_name(s)?;
        let parent = p.parent().filter(|x| !x.as_os_str().is_empty()).unwrap_or(Path::new("."));
        if same_file(parent, d) {
            bail!("{} is already in {}: nothing to move", p.display(), d.display());
        }
        if same_file(p, &d.join(&name)) || same_file(p, d) {
            bail!("{} and {} are the same: nothing to move", p.display(), d.display());
        }
        if let (Ok(src), Some(dst)) = (p.canonicalize(), resolved(d))
            && p.symlink_metadata().is_ok_and(|m| m.is_dir())
            && dst.starts_with(&src)
        {
            bail!("cannot move {} into itself", p.display());
        }
    }
    Ok(())
}

/// Refuse a move between this machine and a server that are one place (a server that is this
/// machine, or sees the same files through a shared mount) when a source would land on itself:
/// the destination directory holds it, or is inside it. rsync would write each file over itself,
/// then delete it. Markers are put next to each source and inside each source directory, then
/// looked for at the destination: the first in the directory the sources go to, the second there
/// and in every directory above it.
pub fn check_move_across(sources: &[Endpoint], dest: &Endpoint, ssh: SshOptions) -> Result<()> {
    let id = crate::accounts::new_uuid();
    let marks = Marks { next_to: format!(".kraal-move-{id}-by"), inside: format!(".kraal-move-{id}-in") };
    let into = sources.len() > 1 || dest.display().ends_with('/');
    // One source to a new name lands on itself only under its own name.
    let same_name = |name: Option<String>| match (sources, name) {
        ([one], Some(name)) => source_name(one).is_ok_and(|(n, contents)| contents || n == name),
        _ => true,
    };
    let refuse = |place: String| -> Result<()> {
        bail!("{place} is where the sources are (both sides see the same files): nothing was moved")
    };
    match (sources.first(), dest) {
        (Some(Endpoint::Local(_)), Endpoint::Remote { server, path }) => {
            let mut placed = LocalMarkers(Vec::new());
            for s in sources {
                if let Endpoint::Local(p) = s {
                    placed.put(p, source_name(s)?.1, &marks)?;
                }
            }
            let where_ = remote_abs_dir(trim_dir(&remote_path(path)));
            let seen = remote_output(server, ssh, &seen_script(&where_, into, &marks))
                .with_context(|| format!("could not check {}", dest.display()))?;
            let said = |what: &str| seen.lines().any(|l| l == what);
            if said("inside") || (said("next to") && (said("into") || same_name(Some(split_parent(path).1)))) {
                return refuse(dest.display());
            }
        }
        (Some(Endpoint::Remote { server, .. }), Endpoint::Local(d)) => {
            let paths: Vec<(String, bool)> = sources
                .iter()
                .filter_map(|s| match s {
                    Endpoint::Remote { path, .. } => {
                        let contents = source_name(s).is_ok_and(|(_, c)| c);
                        Some((remote_abs_dir(trim_dir(&remote_path(path))), contents))
                    }
                    Endpoint::Local(_) => None,
                })
                .collect();
            remote_output(server, ssh, &place_script(&paths, &marks))
                .with_context(|| format!("could not check {}", sources[0].display()))?;
            let (next_to, inside) = local_seen(d, into, &marks);
            let removed = remote_output(server, ssh, &unplace_script(&paths, &marks));
            let name = d.file_name().map(|n| n.to_string_lossy().into_owned());
            if inside || (next_to && (into || d.is_dir() || same_name(name))) {
                return refuse(d.display().to_string());
            }
            removed.with_context(|| format!("could not clean up on {}", server.name))?;
        }
        _ => {}
    }
    Ok(())
}

/// The two marker names of one move check.
struct Marks {
    /// Put in the directory that holds each source.
    next_to: String,
    /// Put inside each source directory.
    inside: String,
}

/// Markers put on this machine, removed when dropped.
struct LocalMarkers(Vec<PathBuf>);

impl LocalMarkers {
    /// Mark `source`: next to it, and inside it if it is a directory. Fails when its directory
    /// cannot be written (the move could not delete it either). A directory's contents (`dir/`)
    /// are only marked inside: the directory holding it need not be writable.
    fn put(&mut self, source: &Path, contents: bool, marks: &Marks) -> Result<()> {
        let mut places = Vec::new();
        if !contents {
            let parent = source.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
            places.push((parent.join(&marks.next_to), true));
        }
        if source.symlink_metadata().is_ok_and(|m| m.is_dir()) || contents {
            places.push((source.join(&marks.inside), contents));
        }
        for (file, needed) in places {
            match std::fs::OpenOptions::new().write(true).create_new(true).open(&file) {
                Ok(_) => self.0.push(file),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) if needed => return Err(e).with_context(|| format!("could not check {}", source.display())),
                Err(_) => {}
            }
        }
        Ok(())
    }
}

impl Drop for LocalMarkers {
    fn drop(&mut self) {
        for f in &self.0 {
            let _ = std::fs::remove_file(f);
        }
    }
}

/// Script that says, for a destination `dest` (a shell word), `into` when the sources go into it
/// (`into`, or it is a directory; else they go next to it, under its name), `next to` when the
/// directory they go to holds the first marker, and `inside` when it or a directory above it
/// holds the second.
fn seen_script(dest: &str, into: bool, marks: &Marks) -> String {
    let (by, inside) = (crate::shell::quote(&marks.next_to), crate::shell::quote(&marks.inside));
    format!(
        "{UP}d={dest}; if [ {} = 1 ] || [ -d \"$d\" ]; then D=$d; echo into; else up \"$d\"; D=$P; fi; \
         if [ -e \"$D\"/{by} ]; then echo 'next to'; fi; x=$D; \
         while :; do if [ -e \"$x\"/{inside} ]; then echo inside; break; fi; \
         up \"$x\"; [ \"$P\" = \"$x\" ] && break; x=$P; done; exit 0",
        u8::from(into)
    )
}

/// The same check on this machine: (next to, inside).
fn local_seen(dest: &Path, into: bool, marks: &Marks) -> (bool, bool) {
    let dir = if into || dest.is_dir() {
        dest.to_path_buf()
    } else {
        dest.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new(".")).to_path_buf()
    };
    let starts: Vec<PathBuf> = [std::path::absolute(&dir).ok(), resolved(&dir)].into_iter().flatten().collect();
    let next_to = starts.iter().any(|d| d.join(&marks.next_to).symlink_metadata().is_ok());
    let inside = starts.iter().any(|d| d.ancestors().any(|a| a.join(&marks.inside).symlink_metadata().is_ok()));
    (next_to, inside)
}

/// Script that marks each of `sources` (shell words, and whether the source means a directory's
/// contents): next to it, and inside it if it is a directory. It fails when the directory holding
/// one cannot be written. Contents are only marked inside (what holds the directory may not be
/// ours to write), which then must work.
fn place_script(sources: &[(String, bool)], marks: &Marks) -> String {
    let (by, inside) = (crate::shell::quote(&marks.next_to), crate::shell::quote(&marks.inside));
    let mut script = String::from(UP);
    for (p, contents) in sources {
        if *contents {
            script.push_str(&format!("p={p}; : > \"$p\"/{inside} || exit 3; "));
        } else {
            script.push_str(&format!(
                "p={p}; up \"$p\"; : > \"$P\"/{by} || exit 3; \
                 if [ -d \"$p\" ] && [ ! -L \"$p\" ]; then : > \"$p\"/{inside} 2>/dev/null; fi; "
            ));
        }
    }
    script.push_str("exit 0");
    script
}

/// Script that removes what [`place_script`] put.
fn unplace_script(sources: &[(String, bool)], marks: &Marks) -> String {
    let (by, inside) = (crate::shell::quote(&marks.next_to), crate::shell::quote(&marks.inside));
    let mut script = String::from(UP);
    for (p, _) in sources {
        script.push_str(&format!("p={p}; up \"$p\"; rm -f -- \"$P\"/{by} \"$p\"/{inside}; "));
    }
    script.push_str("exit 0");
    script
}

/// `path` with its links resolved, even when its last components do not exist yet.
fn resolved(path: &Path) -> Option<PathBuf> {
    let absolute = std::path::absolute(path).ok()?;
    let mut existing = absolute.as_path();
    let mut rest = Vec::new();
    loop {
        if let Ok(found) = existing.canonicalize() {
            return Some(rest.iter().rev().fold(found, |p, c| p.join(c)));
        }
        rest.push(existing.file_name()?.to_owned());
        existing = existing.parent()?;
    }
}

/// Whether two paths name the same file or directory (same device and inode).
fn same_file(a: &Path, b: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    match (std::fs::metadata(a), std::fs::metadata(b)) {
        (Ok(x), Ok(y)) => x.dev() == y.dev() && x.ino() == y.ino(),
        _ => false,
    }
}

/// Move local sources to a local destination by renaming them where the file system allows it
/// (atomic, nothing copied, nothing deleted). What must not be replaced is renamed only if
/// nothing has its name, checked by the rename itself. Returns the sources left for a copy:
/// those on another file system, and directories to merge into existing ones.
pub fn rename_local(sources: &[Endpoint], dest: &Endpoint, opts: &Options) -> Result<Vec<Endpoint>> {
    let Endpoint::Local(d) = dest else { return Ok(sources.to_vec()) };
    if sources.iter().any(|s| !matches!(s, Endpoint::Local(_))) {
        return Ok(sources.to_vec());
    }
    let into = sources.len() > 1 || d.display().to_string().ends_with('/') || d.is_dir();
    let mut left = Vec::new();
    for s in sources {
        let Endpoint::Local(p) = s else { continue };
        let (name, contents) = source_name(s)?;
        let target = if into { d.join(&name) } else { d.clone() };
        let replace = opts.may_replace(&name);
        let merge = target.symlink_metadata().is_ok_and(|m| m.is_dir());
        if contents || (replace && merge) || target.parent().is_some_and(|t| !t.as_os_str().is_empty() && !t.is_dir()) {
            left.push(s.clone());
            continue;
        }
        let renamed = if replace { std::fs::rename(p, &target) } else { crate::paths::rename_no_replace(p, &target) };
        match renamed {
            Ok(()) => eprintln!("moved {} → {}", p.display(), target.display()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                bail!("{} already exists: {} was not moved", target.display(), p.display())
            }
            // Another file system, or one that cannot rename without replacing: copy instead.
            Err(e) if matches!(e.raw_os_error(), Some(libc::EXDEV | libc::EINVAL | libc::ENOSYS | libc::ENOTSUP)) => {
                left.push(s.clone())
            }
            Err(e) => return Err(e).with_context(|| format!("moving {} to {}", p.display(), target.display())),
        }
    }
    Ok(left)
}

/// After a move by rsync, which deletes the files it copied: remove the source directories left
/// empty. One that still has files (they appeared during the move, or could not be copied)
/// stays, and is named.
pub fn prune_moved(sources: &[Endpoint], ssh: SshOptions) -> Result<Vec<String>> {
    let mut kept = Vec::new();
    let mut remote: Vec<(Server, Vec<String>)> = Vec::new();
    for s in sources {
        match s {
            Endpoint::Local(p) => {
                if p.symlink_metadata().is_ok_and(|m| m.is_dir()) {
                    remove_empty_dirs(p);
                    if p.exists() {
                        kept.push(p.display().to_string());
                    }
                }
            }
            Endpoint::Remote { server, path } => match remote.iter_mut().find(|(x, _)| x.same_connection(server)) {
                Some((_, v)) => v.push(remote_path(path)),
                None => remote.push((server.clone(), vec![remote_path(path)])),
            },
        }
    }
    for (server, paths) in remote {
        let mut args = ssh(&server);
        args.extend(["-T".into(), server.destination(), crate::shell::sh_c(&prune_script(&paths))]);
        let out = crate::exec::run(
            std::process::Command::new("ssh").args(&args),
            std::time::Duration::from_secs(120),
            1 << 20,
        )?;
        if !out.success() {
            bail!("cleaning up on {}: {}", server.name, crate::exec::last_stderr_line(&out));
        }
        kept.extend(out.stdout_text().lines().filter(|l| !l.is_empty()).map(|l| format!("{}:{l}", server.name)));
    }
    Ok(kept)
}

/// Shell script that removes the empty directories under each of `paths` (on a server, relative
/// to home), and lists those that still hold files. Every path is absolute: `find` would read a
/// relative one starting with `-` as an expression (`-delete`), not as where to look.
fn prune_script(paths: &[String]) -> String {
    let mut script = String::from("for d in");
    for p in paths {
        script.push(' ');
        script.push_str(&remote_abs_dir(p));
    }
    // rmdir leaves directories that are not empty; those are listed.
    script.push_str(
        "; do if [ -d \"$d\" ] && [ ! -L \"$d\" ]; then find \"$d\" -depth -type d -exec rmdir {} \\; 2>/dev/null; \
         [ -e \"$d\" ] && printf '%s\\n' \"$d\"; fi; done; exit 0",
    );
    script
}

/// Remove the empty directories under `dir`, and `dir` if it ends up empty. Links are not followed.
fn remove_empty_dirs(dir: &Path) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                remove_empty_dirs(&e.path());
            }
        }
    }
    let _ = std::fs::remove_dir(dir);
}

/// What deleting some targets does: the local paths, and the remote paths grouped by
/// connection (one `rm -rf` each).
#[derive(Debug, Default)]
pub struct RemovalPlan {
    pub local: Vec<PathBuf>,
    pub remote: Vec<(Server, Vec<String>)>,
}

/// Check every target, then say what deleting them does; nothing is deleted yet.
pub fn removal_plan(targets: &[Endpoint]) -> Result<RemovalPlan> {
    let mut local = Vec::new();
    let mut remote: Vec<(Server, Vec<String>)> = Vec::new();
    for t in targets {
        match t {
            Endpoint::Local(p) => {
                std::fs::symlink_metadata(p).with_context(|| format!("{}", p.display()))?;
                local.push(p.clone());
            }
            Endpoint::Remote { server, path } => {
                let path = remote_path(path);
                if path.is_empty() || path == "/" || path == "." {
                    bail!("refusing to delete {}:{path}", server.name);
                }
                match remote.iter_mut().find(|(s, _)| s.same_connection(server)) {
                    Some((_, v)) => v.push(path),
                    None => remote.push((server.clone(), vec![path])),
                }
            }
        }
    }
    Ok(RemovalPlan { local, remote })
}

/// Delete local paths and remote paths.
pub fn remove(targets: &[Endpoint], ssh: SshOptions) -> Result<()> {
    let RemovalPlan { local, remote } = removal_plan(targets)?;
    for p in local {
        let meta = std::fs::symlink_metadata(&p).with_context(|| format!("{}", p.display()))?;
        if meta.is_dir() { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) }
            .with_context(|| format!("deleting {}", p.display()))?;
    }
    for (server, paths) in remote {
        let mut line = String::from("rm -rf --");
        for p in &paths {
            line.push(' ');
            line.push_str(&crate::shell::quote(p));
        }
        let mut args = ssh(&server);
        args.push(server.destination());
        args.push(crate::shell::sh_c(&line));
        let status = std::process::Command::new("ssh").args(&args).status().context("running ssh")?;
        if !status.success() {
            bail!("deleting on {} failed ({status})", server.name);
        }
    }
    Ok(())
}

pub fn have_program(name: &str) -> bool {
    std::env::var_os("PATH").map(|paths| std::env::split_paths(&paths).any(|d| d.join(name).is_file())).unwrap_or(false)
}

/// Run `step`, piped into its `pipe_to` if any: the status of the first that failed.
fn run_step(step: &Step) -> Result<std::process::ExitStatus> {
    let Some(next) = &step.pipe_to else {
        return std::process::Command::new(&step.program)
            .args(&step.args)
            .status()
            .with_context(|| format!("running {}", step.program));
    };
    let mut left = std::process::Command::new(&step.program)
        .args(&step.args)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("running {}", step.program))?;
    let pipe = left.stdout.take().context("no pipe")?;
    let right = std::process::Command::new(&next.program).args(&next.args).stdin(pipe).status();
    let left = left.wait()?;
    let right = right.with_context(|| format!("running {}", next.program))?;
    Ok(if left.success() { right } else { left })
}

/// Run the steps in the foreground, showing their output. Falls back to a tar stream when rsync
/// is missing on the server (rsync exits with 12 or 127 then).
pub fn run(steps: &[Step]) -> Result<()> {
    for step in steps {
        eprintln!("$ {}", step.shell_line());
        if let Some(install) = &step.here {
            install_here(install)?;
            continue;
        }
        let partial = step.partial.as_deref().map(PartialRun::start).transpose()?;
        let status = run_step(step)?;
        if !status.success() {
            let Some(fb) = step.fallback.as_deref().filter(|_| matches!(status.code(), Some(12) | Some(127))) else {
                bail!("{} failed ({status})", step.program);
            };
            eprintln!("rsync is not available on the server; using tar");
            eprintln!("$ {}", fb.shell_line());
            let status = run_step(fb)?;
            if !status.success() {
                bail!("{} failed ({status})", fb.program);
            }
        }
        if let Some(partial) = partial {
            partial.finish();
        }
    }
    Ok(())
}

/// Recognize complete local path lists without expanding shell expressions.
pub fn dropped_paths(text: &str) -> Option<Vec<PathBuf>> {
    if text.chars().any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t')) {
        return None;
    }
    let text = text.trim();
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = text.chars().peekable();
    let mut quote = None;
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some('"'), '\\') => {
                let next = chars.next()?;
                if !matches!(next, '"' | '\\' | '$' | '`') {
                    cur.push('\\');
                }
                cur.push(next);
            }
            (Some('"') | None, '$' | '`') => return None,
            (Some(_), c) => cur.push(c),
            (None, '\'' | '"') => {
                quote = Some(c);
                in_word = true;
            }
            (None, '\\') => {
                cur.push(chars.next()?);
                in_word = true;
            }
            (None, '*' | '?' | '[' | ']' | ';' | '|' | '&' | '<' | '>' | '(' | ')') => return None,
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            (None, c) => {
                cur.push(c);
                in_word = true;
            }
        }
    }
    if quote.is_some() {
        return None;
    }
    if in_word {
        words.push(cur);
    }
    let parsed =
        words.into_iter().map(|word| local_path(&word)).collect::<Option<Vec<_>>>().filter(|paths| !paths.is_empty());
    // Some terminals leave spaces unquoted. Never choose between a whole name and a list.
    let raw = (!text.contains(['\'', '"', '\\', '\n', '\r', '\t']))
        .then(|| local_path(text))
        .flatten()
        .map(|path| vec![path]);
    if parsed.is_some() {
        let words: Vec<_> = text.split_whitespace().collect();
        for (i, word) in words.iter().enumerate() {
            let start = word.as_ptr() as usize - text.as_ptr() as usize;
            let Some(next) = words.get(i + 1) else { break };
            let next_start = next.as_ptr() as usize - text.as_ptr() as usize;
            // Joining absolute words would need a directory whose name includes the separator.
            if !Path::new(&text[start..next_start]).is_dir() {
                continue;
            }
            for last in &words[i + 1..] {
                let last_end = last.as_ptr() as usize - text.as_ptr() as usize + last.len();
                if last_end - start > 4096 {
                    break;
                }
                let joined = &text[start..last_end];
                if !joined.contains(['\'', '"', '\\', '\n', '\r', '\t']) && local_path(joined).is_some() {
                    return None;
                }
            }
        }
    }
    match (parsed, raw) {
        (Some(a), Some(b)) if a != b => None,
        (Some(paths), _) | (_, Some(paths)) => Some(paths),
        _ => None,
    }
}

fn local_path(word: &str) -> Option<PathBuf> {
    let decoded;
    let word = if let Some(uri) = word.strip_prefix("file://") {
        if !uri.starts_with('/') || uri.contains(['?', '#']) {
            return None;
        }
        let mut bytes = Vec::new();
        let mut input = uri.bytes();
        while let Some(byte) = input.next() {
            bytes.push(if byte == b'%' {
                let hi = char::from(input.next()?).to_digit(16)?;
                let lo = char::from(input.next()?).to_digit(16)?;
                (hi * 16 + lo) as u8
            } else {
                byte
            });
        }
        decoded = String::from_utf8(bytes).ok()?;
        decoded.as_str()
    } else {
        word
    };
    if word.chars().any(char::is_control) {
        return None;
    }
    let path = PathBuf::from(word);
    (path.is_absolute() && std::fs::symlink_metadata(&path).is_ok()).then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_names_in_a_local_directory() {
        let dir = std::env::temp_dir().join(format!("kraal-existing-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("dest")).unwrap();
        std::fs::write(dir.join("dest/a.txt"), "old").unwrap();
        let sources = vec![Endpoint::Local(dir.join("a.txt")), Endpoint::Local(dir.join("b.txt"))];
        let none = |_: &Server| Vec::new();
        let there = existing_in(&sources, &Endpoint::Local(dir.join("dest")), &none).unwrap();
        assert_eq!(there, vec!["a.txt".to_string()]);
        // A file is not a directory to copy into: it would be replaced.
        let file = Endpoint::Local(dir.join("dest/a.txt"));
        assert!(existing_in(&sources, &file, &none).is_err());
        // "dir/" means its contents, whose names are not known here.
        let contents = vec![Endpoint::Local(PathBuf::from(format!("{}/", dir.display())))];
        assert!(existing_in(&contents, &Endpoint::Local(dir.join("dest")), &none).is_err());
        // A destination that does not exist yet has nothing to replace.
        assert!(existing_in(&sources, &Endpoint::Local(dir.join("new")), &none).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn removal_groups_by_connection() {
        let web = |port| Server {
            name: format!("web{port}"),
            host: "web.lan".into(),
            port: Some(port),
            ..Default::default()
        };
        let targets = vec![
            Endpoint::Remote { server: web(22), path: "/tmp/a".into() },
            Endpoint::Remote { server: web(2222), path: "/tmp/b".into() },
            Endpoint::Remote { server: web(22), path: "/tmp/c".into() },
        ];
        let RemovalPlan { local, remote } = removal_plan(&targets).unwrap();
        assert!(local.is_empty());
        assert_eq!(remote.len(), 2, "same host, different ports: two connections");
        assert_eq!(remote[0].1, vec!["/tmp/a", "/tmp/c"]);
        assert_eq!((remote[1].0.port, remote[1].1.clone()), (Some(2222), vec!["/tmp/b".to_string()]));
        // Nothing is deleted when one target is refused, local files included.
        let dir = std::env::temp_dir().join(format!("kraal-rm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bad = vec![Endpoint::Local(dir.clone()), Endpoint::Remote { server: web(22), path: "/".into() }];
        assert!(remove(&bad, &|_| Vec::new()).is_err());
        assert!(dir.exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    fn known() -> Vec<Known> {
        let mut inv = crate::servers::Inventory::empty();
        inv.servers = vec![
            Server { name: "a".into(), host: "a.lan".into(), user: Some("u".into()), ..Default::default() },
            Server {
                name: "b".into(),
                host: "b.lan".into(),
                user: Some("v".into()),
                port: Some(2222),
                ..Default::default()
            },
        ];
        crate::servers::known(&inv, &[], &[])
    }

    fn opts(s: &Server) -> Vec<String> {
        s.port.map(|p| vec!["-p".to_string(), p.to_string()]).unwrap_or_default()
    }

    fn with_rsync(rsync: bool) -> Options {
        Options { rsync, ..Default::default() }
    }

    #[test]
    fn upload_uses_rsync_with_a_tar_fallback() {
        let k = known();
        let src = Endpoint::Local(std::env::temp_dir());
        let dest = parse_endpoint("b:~/up/", &k).unwrap();
        let steps = plan(&[src], &dest, &opts, &with_rsync(true)).unwrap();
        assert_eq!(steps[0].program, "rsync");
        let args = &steps[0].args;
        assert!(args.contains(&"ssh -p 2222".to_string()), "{args:?}");
        // Names reach the server as they are (no remote shell), a half-sent file never replaces
        // the old one, and nothing after `--` is an option.
        for flag in ["-s", "--"] {
            assert!(args.contains(&flag.to_string()), "{flag}: {args:?}");
        }
        // The partial-file directory is this transfer's own: the same when it runs again (a retry
        // takes up where it stopped), another for another transfer.
        let partial = |steps: &[Step]| steps[0].args.iter().find(|a| a.starts_with("--partial-dir=")).cloned().unwrap();
        let first = partial(&steps);
        assert!(first.len() == "--partial-dir=.kraal-partial-".len() + 16, "{first}");
        let again = plan(&[Endpoint::Local(std::env::temp_dir())], &dest, &opts, &with_rsync(true)).unwrap();
        assert_eq!(partial(&again), first);
        let elsewhere = parse_endpoint("b:~/other/", &k).unwrap();
        let other = plan(&[Endpoint::Local(std::env::temp_dir())], &elsewhere, &opts, &with_rsync(true)).unwrap();
        assert_ne!(partial(&other), first);
        assert_eq!(args.last().unwrap(), "v@b.lan:up/");
        // Without rsync on the server: a tar stream, which keeps links as links (scp follows them).
        let fb = steps[0].fallback.as_ref().unwrap();
        assert_eq!(fb.program, "tar");
        let right = fb.pipe_to.as_ref().unwrap();
        assert!(right.args.windows(2).any(|w| w == ["-p", "2222"]), "{:?}", right.args);
        assert!(right.args.last().unwrap().starts_with("sh -c "), "{:?}", right.args);
    }

    #[test]
    fn download_and_server_to_server() {
        let k = known();
        let src = parse_endpoint("a:/etc/hosts", &k).unwrap();
        let steps =
            plan(std::slice::from_ref(&src), &Endpoint::Local("/tmp".into()), &opts, &with_rsync(false)).unwrap();
        assert_eq!(steps[0].program, "ssh");
        assert_eq!(crate::shell::unwrap_sh_c(steps[0].args.last().unwrap()), "tar cf - -C /etc ./hosts");
        assert_eq!(steps[0].pipe_to.as_ref().unwrap().program, "sh");
        let dest = parse_endpoint("b:/srv/in", &k).unwrap();
        let steps = plan(std::slice::from_ref(&src), &dest, &opts, &with_rsync(true)).unwrap();
        // Two ssh piped together by kraal (no shell, so no `pipefail` portability issue).
        assert_eq!(steps[0].program, "ssh");
        assert_eq!(crate::shell::unwrap_sh_c(steps[0].args.last().unwrap()), "tar cf - -C /etc ./hosts");
        let right = steps[0].pipe_to.as_ref().unwrap();
        assert!(right.args.windows(2).any(|w| w == ["-p", "2222"]), "{:?}", right.args);
        // One file to a name that is not a directory there: it takes that name, as with rsync.
        assert!(right.args.last().unwrap().contains("if [ -d /srv/in ]"), "{:?}", right.args);
        // Relative sources are taken from home, each on its own (a relative -C would chain).
        let rel = [parse_endpoint("a:logs/x", &k).unwrap(), parse_endpoint("a:conf/y", &k).unwrap()];
        let steps = plan(&rel, &dest, &opts, &with_rsync(true)).unwrap();
        assert_eq!(
            crate::shell::unwrap_sh_c(steps[0].args.last().unwrap()),
            "tar cf - -C \"$HOME\"/logs ./x -C \"$HOME\"/conf ./y"
        );
        let same = parse_endpoint("a:/tmp/", &k).unwrap();
        let steps = plan(&[src], &same, &opts, &with_rsync(true)).unwrap();
        assert_eq!(steps[0].program, "ssh");
        assert_eq!(
            crate::shell::unwrap_sh_c(steps[0].args.last().unwrap()),
            "mkdir -p -- /tmp/ || exit 1; cp -RP -- /etc/hosts /tmp/"
        );
    }

    #[test]
    fn what_must_not_be_replaced_is_staged_then_put_in_place() {
        let k = known();
        let a = parse_endpoint("a:/x/keep", &k).unwrap();
        let b = parse_endpoint("a:/x/swap", &k).unwrap();
        let only = |names: &[&str], rsync| Options {
            rsync,
            replace: Some(names.iter().map(|n| n.to_string()).collect()),
            ..Default::default()
        };
        let steps =
            plan(&[a.clone(), b.clone()], &Endpoint::Local("/tmp/d/".into()), &opts, &only(&["swap"], true)).unwrap();
        // What may be replaced is copied in place; what may not goes through a staging directory.
        assert_eq!(steps.len(), 4, "{steps:#?}");
        assert!(steps[0].args.contains(&"u@a.lan:/x/swap".to_string()) && steps[0].args.last().unwrap() == "/tmp/d/");
        assert_eq!(steps[1].program, "sh");
        assert!(steps[1].args[1].contains("mkdir -m 700"), "{:?}", steps[1].args);
        assert!(steps[2].args.contains(&"u@a.lan:/x/keep".to_string()));
        let target = steps[2].args.last().unwrap();
        assert!(target.starts_with("/tmp/d/.kraal-stage-") && target.ends_with("/payload/"), "{:?}", steps[2].args);
        assert!(!steps[2].args.iter().any(|a| a.starts_with("--partial-dir")), "a new stage holds no partial files");
        // Here, kraal puts things in place itself, with a rename that never replaces.
        assert!(steps[3].here.is_some() && steps[3].program == "kraal", "{:?}", steps[3]);
        // Each run stages anew.
        let again =
            plan(&[a.clone(), b.clone()], &Endpoint::Local("/tmp/d/".into()), &opts, &only(&["swap"], true)).unwrap();
        assert_ne!(again[2].args.last(), steps[2].args.last());
        // The same on a server: the staging directory is there, in the destination.
        let same = plan(&[a, b], &parse_endpoint("a:/y", &k).unwrap(), &opts, &only(&[], true)).unwrap();
        assert_eq!(same.len(), 3);
        let copy = same[1].args.last().unwrap();
        assert!(copy.contains("cp -RP -- /x/keep /x/swap /y/.kraal-stage-") && copy.contains("/payload/"), "{copy}");
        assert!(same[2].args.last().unwrap().contains("kraal_put"));
        assert_eq!(local_operand(Path::new("-x")), "./-x", "a name is never an option");
        assert_eq!(local_operand(Path::new("/tmp/-x")), "/tmp/-x");
    }

    #[test]
    fn moves_delete_only_what_rsync_copied() {
        let k = known();
        let moving = |rsync| Options { rsync, moving: true, ..Default::default() };
        let src = parse_endpoint("a:/x/f", &k).unwrap();
        let steps = plan(std::slice::from_ref(&src), &Endpoint::Local("/tmp".into()), &opts, &moving(true)).unwrap();
        assert!(steps[0].args.contains(&"--remove-source-files".to_string()));
        assert!(steps[0].args.contains(&"--checksum".to_string()), "a skipped file must be identical, not just alike");
        assert!(steps[0].fallback.is_none(), "no fallback deletes what it did not check");
        assert!(plan(std::slice::from_ref(&src), &Endpoint::Local("/tmp".into()), &opts, &moving(false)).is_err());
        assert!(plan(std::slice::from_ref(&src), &parse_endpoint("b:/y", &k).unwrap(), &opts, &moving(true)).is_err());
        let same = plan(&[src], &parse_endpoint("a:/y/", &k).unwrap(), &opts, &moving(true)).unwrap();
        assert!(same[0].args.last().unwrap().contains("mv -- /x/f /y/"), "{:?}", same[0].args);
    }

    #[test]
    fn ipv6_hosts_and_root_destinations() {
        let v6 = Server { name: "v6".into(), host: "::1".into(), user: Some("me".into()), ..Default::default() };
        assert_eq!(rsync_host(&v6), "me@[::1]");
        assert_eq!(trim_dir("/"), "/", "the root is not home");
        assert_eq!(trim_dir("/srv//"), "/srv");
        assert!(rsync_version_ok("rsync  version 3.2.7  protocol version 31\n"));
        assert!(!rsync_version_ok("rsync  version 2.6.9  protocol version 29\n"));
        assert!(!rsync_version_ok("openrsync: protocol version 29\nrsync version 2.6.9 compatible\n"));
    }

    #[test]
    fn mixed_sources_are_rejected() {
        let k = known();
        let a = parse_endpoint("a:/x", &k).unwrap();
        let b = parse_endpoint("b:/y", &k).unwrap();
        assert!(plan(&[a.clone(), b], &Endpoint::Local("/tmp".into()), &opts, &with_rsync(true)).is_err());
        assert!(
            plan(
                &[a, Endpoint::Local(std::env::temp_dir())],
                &Endpoint::Local("/tmp".into()),
                &opts,
                &with_rsync(true)
            )
            .is_err()
        );
    }

    struct Tmp {
        dir: crate::testing::TempDir,
    }

    impl Tmp {
        fn new(name: &str) -> Tmp {
            Tmp { dir: crate::testing::TempDir::new(name) }
        }

        fn write(&self, rel: &str, text: &str) -> PathBuf {
            let p = self.dir.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, text).unwrap();
            p
        }

        fn read(&self, rel: &str) -> String {
            std::fs::read_to_string(self.dir.0.join(rel)).unwrap_or_default()
        }
    }

    #[test]
    fn local_copies_keep_what_appeared_in_the_destination() {
        let t = Tmp::new("keep");
        let a = t.write("src/a", "new a");
        let b = t.write("src/b", "new b");
        t.write("src/d/inner", "new d");
        let keep = |rsync| Options { rsync, replace: Some(vec!["a".into()]), ..Default::default() };
        let rsyncs = if rsync_capable() { vec![false, true] } else { vec![false] };
        for rsync in rsyncs {
            let _ = std::fs::remove_dir_all(t.dir.0.join("dst"));
            t.write("dst/a", "the old a");
            let sources =
                [Endpoint::Local(a.clone()), Endpoint::Local(b.clone()), Endpoint::Local(t.dir.0.join("src/d"))];
            let steps = plan(&sources, &Endpoint::Local(t.dir.0.join("dst/")), &opts, &keep(rsync)).unwrap();
            // The race itself: `b` and `d` appear in the destination after the copy started (here,
            // once the files are staged), just before they are put in place.
            let (install, before) = steps.split_last().unwrap();
            run(before).unwrap();
            t.write("dst/b", "theirs");
            t.write("dst/d/mine", "theirs too");
            assert!(run(std::slice::from_ref(install)).is_err(), "rsync {rsync}: a taken name is an error");
            assert_eq!((t.read("dst/a").as_str(), t.read("dst/b").as_str()), ("new a", "theirs"), "rsync {rsync}");
            assert_eq!(t.read("dst/d/mine"), "theirs too");
            assert!(!t.dir.0.join("dst/d/inner").exists(), "a directory is not merged into one that appeared");
            // What was not put in place waits in the staging directory.
            let stage = std::fs::read_dir(t.dir.0.join("dst"))
                .unwrap()
                .map(|e| e.unwrap().path())
                .find(|p| p.file_name().unwrap().to_string_lossy().starts_with(".kraal-stage-"))
                .expect("the staging directory stays");
            assert_eq!(std::fs::read_to_string(stage.join("payload/b")).unwrap(), "new b");
            assert!(stage.join("payload/d/inner").exists());
            // Without anything in the way, what waits is put in place and the staging goes.
            std::fs::remove_file(t.dir.0.join("dst/b")).unwrap();
            std::fs::remove_dir_all(t.dir.0.join("dst/d")).unwrap();
            run(std::slice::from_ref(install)).unwrap();
            assert_eq!((t.read("dst/b").as_str(), t.read("dst/d/inner").as_str()), ("new b", "new d"));
            assert!(!stage.exists());
        }
    }

    /// Whether this machine's `mv` puts directories in place without replacing (see [`MV_KEEPS`]).
    fn mv_keeps_here() -> bool {
        let script = format!("{MV_KEEPS}mv_keeps");
        std::process::Command::new("sh").args(["-c", &script]).status().unwrap().success()
    }

    #[test]
    fn which_mv_puts_a_directory_in_place_safely() {
        // GNU coreutils renames without replacing from 8.30 on; anything else is not trusted.
        let keeps = |version: &str| {
            let script = format!("mv() {{ printf '%s\\n' {}; }}; {MV_KEEPS}mv_keeps", crate::shell::quote(version));
            std::process::Command::new("sh").args(["-c", &script]).status().unwrap().success()
        };
        for ok in ["mv (GNU coreutils) 8.30", "mv (GNU coreutils) 8.32", "mv (GNU coreutils) 9.4"] {
            assert!(keeps(ok), "{ok}");
        }
        for not in ["mv (GNU coreutils) 8.28", "mv (GNU coreutils) 7.9", "BusyBox v1.36.1", ""] {
            assert!(!keeps(not), "{not:?}");
        }
    }

    #[test]
    fn putting_in_place_never_replaces() {
        use std::os::unix::fs::MetadataExt;
        let t = Tmp::new("put");
        let stage = t.dir.0.join("stage");
        let dest = t.dir.0.join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let put = |name: &str| {
            let word = |p: &Path| crate::shell::quote_always(&p.display().to_string());
            let script = format!("{DEV}{MV_KEEPS}{PUT}kraal_put {} {}", word(&stage.join(name)), word(&dest));
            std::process::Command::new("sh").args(["-c", &script]).status().unwrap().success()
        };
        let mode = |p: &Path| std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(p).unwrap().permissions());
        let set_mode =
            |p: &Path, m: u32| std::fs::set_permissions(p, std::os::unix::fs::PermissionsExt::from_mode(m)).unwrap();
        t.write("stage/file", "new");
        t.write("stage/dir/x", "new");
        t.write("stage/dir/.hidden", "new");
        t.write("stage/empty/x", "new");
        std::os::unix::fs::symlink("somewhere", stage.join("link")).unwrap();
        t.write("dest/file", "theirs");
        t.write("dest/dir/y", "theirs");
        // An empty directory, theirs: it holds nothing, but it is not ours to replace either.
        std::fs::create_dir(dest.join("empty")).unwrap();
        set_mode(&dest.join("empty"), 0o750);
        std::os::unix::fs::symlink("theirs", dest.join("link")).unwrap();
        for name in ["file", "dir", "empty", "link"] {
            assert!(!put(name), "{name} is taken");
            assert!(stage.join(name).symlink_metadata().is_ok(), "{name} stays staged");
        }
        assert_eq!((t.read("dest/file").as_str(), t.read("dest/dir/y").as_str()), ("theirs", "theirs"));
        assert_eq!(std::fs::read_link(dest.join("link")).unwrap(), Path::new("theirs"));
        assert!(std::fs::read_dir(dest.join("empty")).unwrap().next().is_none(), "their directory is left as it was");
        assert_eq!(mode(&dest.join("empty")) & 0o7777, 0o750);
        // Free names: the file is linked in place and the link stays a link; a directory is
        // renamed, so it stays the very same directory (permissions, owner, group, ACLs).
        std::fs::remove_file(dest.join("file")).unwrap();
        std::fs::remove_dir_all(dest.join("dir")).unwrap();
        std::fs::remove_dir(dest.join("empty")).unwrap();
        std::fs::remove_file(dest.join("link")).unwrap();
        set_mode(&stage.join("dir"), 0o751);
        let inode = std::fs::metadata(stage.join("dir")).unwrap().ino();
        for name in ["file", "link"] {
            assert!(put(name), "{name}");
            assert!(stage.join(name).symlink_metadata().is_err(), "{name} left the staging directory");
        }
        assert_eq!(t.read("dest/file"), "new");
        assert_eq!(std::fs::read_link(dest.join("link")).unwrap(), Path::new("somewhere"));
        if !mv_keeps_here() {
            // Without an `mv` that renames without replacing, a directory is not put in place.
            assert!(!put("dir") && stage.join("dir/x").exists() && !dest.join("dir").exists());
            return;
        }
        for name in ["dir", "empty"] {
            assert!(put(name), "{name}");
            assert!(stage.join(name).symlink_metadata().is_err(), "{name} left the staging directory");
        }
        assert_eq!((t.read("dest/dir/x").as_str(), t.read("dest/dir/.hidden").as_str()), ("new", "new"));
        assert_eq!(t.read("dest/empty/x"), "new");
        assert_eq!(std::fs::metadata(dest.join("dir")).unwrap().ino(), inode, "the same directory, renamed");
        assert_eq!(mode(&dest.join("dir")) & 0o7777, 0o751);
    }

    #[test]
    fn a_directory_put_in_place_here_stays_what_it_was() {
        use std::os::unix::fs::MetadataExt;
        let t = Tmp::new("put-here");
        t.write("src/tree/a", "a");
        std::fs::set_permissions(t.dir.0.join("src/tree"), std::os::unix::fs::PermissionsExt::from_mode(0o750))
            .unwrap();
        std::fs::create_dir_all(t.dir.0.join("out")).unwrap();
        let keep = Options { replace: Some(Vec::new()), ..Default::default() };
        let steps =
            plan(&[Endpoint::Local(t.dir.0.join("src/tree"))], &Endpoint::Local(t.dir.0.join("out/")), &opts, &keep)
                .unwrap();
        let (install, before) = steps.split_last().unwrap();
        run(before).unwrap();
        let [stage] = &stages(&t.dir.0.join("out"))[..] else { panic!("one staging directory") };
        let staged = std::fs::metadata(stage.join("payload/tree")).unwrap();
        // An empty directory takes the name meanwhile: it is not replaced.
        std::fs::create_dir(t.dir.0.join("out/tree")).unwrap();
        assert!(run(std::slice::from_ref(install)).is_err());
        assert!(std::fs::read_dir(t.dir.0.join("out/tree")).unwrap().next().is_none());
        std::fs::remove_dir(t.dir.0.join("out/tree")).unwrap();
        run(std::slice::from_ref(install)).unwrap();
        let placed = std::fs::metadata(t.dir.0.join("out/tree")).unwrap();
        assert_eq!((placed.ino(), placed.mode() & 0o7777, placed.gid()), (staged.ino(), 0o750, staged.gid()));
        assert_eq!(t.read("out/tree/a"), "a");
        assert!(stages(&t.dir.0.join("out")).is_empty());
    }

    #[test]
    fn tar_unpacking_renames_like_rsync() {
        let t = Tmp::new("untar");
        t.write("src/f", "file");
        t.write("src/d/x", "inner");
        let unpack = |dest: &Path, single: Option<&str>, members: &[&str]| {
            let mut tar = std::process::Command::new("tar")
                .args(["cf", "-", "-C"])
                .arg(t.dir.0.join("src"))
                .args(members)
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let script = unpack_script(&crate::shell::quote_always(&dest.display().to_string()), single);
            let status = std::process::Command::new("sh")
                .args(["-c", &script])
                .stdin(tar.stdout.take().unwrap())
                .status()
                .unwrap();
            tar.wait().unwrap();
            status.success()
        };
        // One file to a new name: renamed.
        assert!(unpack(&t.dir.0.join("out/renamed"), Some("f"), &["./f"]));
        assert_eq!(t.read("out/renamed"), "file");
        // To an existing directory: inside it.
        assert!(unpack(&t.dir.0.join("out"), Some("f"), &["./f"]));
        assert_eq!(t.read("out/f"), "file");
        // A directory to a new name: created, with the directory inside, as rsync does.
        assert!(unpack(&t.dir.0.join("tree"), Some("d"), &["./d"]));
        assert_eq!(t.read("tree/d/x"), "inner");
        assert!(
            std::fs::read_dir(&t.dir.0).unwrap().all(|e| !e
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".kraal"))
        );
    }

    #[test]
    fn local_moves_rename_and_never_lose_the_source() {
        let t = Tmp::new("move");
        let f = t.write("data/report", "only copy");
        std::os::unix::fs::symlink(t.dir.0.join("data"), t.dir.0.join("alias")).unwrap();
        let src = [Endpoint::Local(f.clone())];
        // The same directory, through a link: nothing to do, and nothing deleted.
        assert!(check_move(&src, &Endpoint::Local(t.dir.0.join("alias"))).is_err());
        assert!(check_move(&src, &Endpoint::Local(t.dir.0.join("data/"))).is_err());
        let dir = t.dir.0.join("data");
        assert!(check_move(&[Endpoint::Local(dir.clone())], &Endpoint::Local(dir.join("sub"))).is_err());
        // A rename where it can: atomic, and refused rather than replacing.
        t.write("taken/report", "theirs");
        let keep = Options { replace: Some(Vec::new()), moving: true, ..Default::default() };
        assert!(rename_local(&src, &Endpoint::Local(t.dir.0.join("taken")), &keep).is_err());
        assert_eq!((t.read("data/report").as_str(), t.read("taken/report").as_str()), ("only copy", "theirs"));
        let left = rename_local(&src, &Endpoint::Local(t.dir.0.join("dest/")), &keep).unwrap();
        assert_eq!(left, src, "a destination to create is left to the copy");
        std::fs::create_dir(t.dir.0.join("dest")).unwrap();
        assert!(rename_local(&src, &Endpoint::Local(t.dir.0.join("dest")), &keep).unwrap().is_empty());
        assert_eq!(t.read("dest/report"), "only copy");
        assert!(!f.exists());
        // Empty directories go after rsync deleted their files; one with a file left stays.
        t.write("tree/a/b/keep", "late");
        std::fs::create_dir_all(t.dir.0.join("tree/c/d")).unwrap();
        let kept = prune_moved(&[Endpoint::Local(t.dir.0.join("tree"))], &opts).unwrap();
        assert_eq!(kept, [t.dir.0.join("tree").display().to_string()]);
        assert!(t.dir.0.join("tree/a/b/keep").exists() && !t.dir.0.join("tree/c").exists());
    }

    #[test]
    fn cleaning_up_a_move_never_reads_a_name_as_find_options() {
        let home = Tmp::new("prune-home");
        home.write("keep/precious", "data");
        std::fs::create_dir_all(home.dir.0.join("-delete/empty/deeper")).unwrap();
        home.write("-delete/left", "late");
        let status = std::process::Command::new("sh")
            .args(["-c", &prune_script(&["-delete".to_string()])])
            .env("HOME", &home.dir.0)
            .current_dir(&home.dir.0)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(home.read("keep/precious"), "data", "nothing outside the moved directory is touched");
        assert!(!home.dir.0.join("-delete/empty").exists() && home.dir.0.join("-delete/left").exists());
    }

    /// A fake ssh that runs the command here: a server that is this machine.
    fn fake_ssh(t: &Tmp) -> String {
        let path = t.dir.0.join("fake-ssh");
        crate::testing::write_script(&path, "#!/bin/sh\nshift\nexec sh -c \"$*\"\n");
        path.display().to_string()
    }

    /// Steps for a server that is this machine, run here: an ssh step runs its command, rsync
    /// goes through the fake ssh.
    fn run_here(t: &Tmp, steps: &[Step]) -> Result<()> {
        run_on_server_like(t, steps, "")
    }

    /// [`run_here`] on a server whose shell first runs `server` (say, a PATH where an older `mv`
    /// comes first).
    fn run_on_server_like(t: &Tmp, steps: &[Step], server: &str) -> Result<()> {
        let fake = fake_ssh(t);
        run(&steps.iter().map(|s| here(s, &fake, server)).collect::<Vec<_>>())
    }

    /// `s` run on this machine as if on the server: ssh commands go to a shell, rsync's to the
    /// fake ssh.
    fn here(s: &Step, fake: &str, server: &str) -> Step {
        let mut s = s.clone();
        if s.program == "ssh" {
            s.args = vec!["-c".into(), format!("{server}{}", s.args.last().unwrap())];
            s.program = "sh".into();
        } else if let Some(e) = s.args.iter().position(|a| a == "-e") {
            s.args[e + 1] = fake.to_string();
        }
        s.pipe_to = s.pipe_to.map(|p| Box::new(here(&p, fake, server)));
        if let Some(p) = &mut s.partial {
            p.runner = Step::new("sh", vec!["-c".into()]);
            p.remote = false;
        }
        s
    }

    #[test]
    fn a_server_that_cannot_put_directories_in_place_is_told_before_anything_moves() {
        let t = Tmp::new("old-mv");
        t.write("in/tree/a", "a");
        t.write("in/note", "note");
        std::fs::create_dir_all(t.dir.0.join("out")).unwrap();
        // The server's `mv` is one from before coreutils 8.30.
        let real = std::process::Command::new("sh").args(["-c", "command -v mv"]).output().unwrap();
        let real = String::from_utf8_lossy(&real.stdout).trim().to_string();
        std::fs::create_dir_all(t.dir.0.join("bin")).unwrap();
        crate::testing::write_script(
            &t.dir.0.join("bin/mv"),
            &format!(
                "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'mv (GNU coreutils) 8.28'; exit 0; fi\nexec {} \"$@\"\n",
                crate::shell::quote(&real)
            ),
        );
        let old =
            format!("PATH={}:$PATH; export PATH; ", crate::shell::quote(&t.dir.0.join("bin").display().to_string()));
        let moving = Options { moving: true, replace: Some(Vec::new()), dest_dir: Some(true), ..Default::default() };
        let dest = here_at(&t.dir.0.join("out/"));
        let tree = here_at(&t.dir.0.join("in/tree"));
        let err = run_on_server_like(&t, &plan(std::slice::from_ref(&tree), &dest, &opts, &moving).unwrap(), &old);
        assert!(err.is_err(), "refused");
        assert_eq!(t.read("in/tree/a"), "a", "the directory is where it was");
        assert!(stages(&t.dir.0.join("out")).is_empty() && !t.dir.0.join("out/tree").exists(), "nothing was staged");
        // A file needs no directory put in place: it goes as before.
        let note = here_at(&t.dir.0.join("in/note"));
        run_on_server_like(&t, &plan(std::slice::from_ref(&note), &dest, &opts, &moving).unwrap(), &old).unwrap();
        assert_eq!(t.read("out/note"), "note");
        assert!(!t.dir.0.join("in/note").exists());
        // A local directory too is known to need it before anything is sent.
        let local = Options { replace: Some(Vec::new()), dest_dir: Some(true), ..Default::default() };
        let up = Endpoint::Local(t.dir.0.join("in/tree"));
        assert!(run_on_server_like(&t, &plan(std::slice::from_ref(&up), &dest, &opts, &local).unwrap(), &old).is_err());
        assert!(stages(&t.dir.0.join("out")).is_empty() && !t.dir.0.join("out/tree").exists());
        // With an mv that can, the directory moves, the same directory.
        if mv_keeps_here() {
            run_here(&t, &plan(std::slice::from_ref(&tree), &dest, &opts, &moving).unwrap()).unwrap();
            assert_eq!(t.read("out/tree/a"), "a");
            assert!(!t.dir.0.join("in/tree").exists());
        }
    }

    fn here_at(path: &Path) -> Endpoint {
        let here = Server { name: "here".into(), host: "localhost".into(), ..Default::default() };
        Endpoint::Remote { server: here, path: path.display().to_string() }
    }

    /// The staging directories in `dir`.
    fn stages(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .map(|d| d.map(|e| e.unwrap().path()).filter(|p| p.to_string_lossy().contains(".kraal-stage-")).collect())
            .unwrap_or_default()
    }

    /// An owner, on this machine and boot, that no longer runs (`None` where the system cannot
    /// name this machine).
    fn gone_owner() -> Option<String> {
        let me = owner_token()?;
        let words: Vec<&str> = me.split(' ').collect();
        Some(format!("{} {} 999999999 1\n", words[0], words[1]))
    }

    /// A staging directory as this version makes them, with `kind` and `owner`.
    fn made_stage(dir: &Path, name: &str, kind: &str, owner: &str) -> PathBuf {
        let stage = dir.join(name);
        std::fs::create_dir_all(stage.join("payload")).unwrap();
        std::fs::create_dir_all(stage.join("work")).unwrap();
        std::fs::write(stage.join("payload/data"), "staged").unwrap();
        std::fs::write(stage.join("format"), format!("{STAGE_FORMAT}\n")).unwrap();
        std::fs::write(stage.join("kind"), format!("{kind}\n")).unwrap();
        std::fs::write(stage.join("owner"), owner).unwrap();
        stage
    }

    #[test]
    fn staging_left_by_others_is_never_removed_unless_its_copy_is_over() {
        let t = Tmp::new("others");
        let src = Endpoint::Local(t.write("in/report", "the report"));
        let out = t.dir.0.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let dest = Endpoint::Local(t.dir.0.join("out/"));
        let prefix = stage_prefix(std::slice::from_ref(&src), &dest);
        let hex = |c: char| std::iter::repeat_n(c, 32).collect::<String>();
        // An older kraal's staging, without a run of its own in its name: what it holds is the
        // user's (here a file named `kind` saying `copy`).
        let legacy = out.join(&prefix);
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("kind"), "copy\n").unwrap();
        // A staging of an earlier format, and one not finished being made (no `format` yet).
        let older = out.join(format!("{prefix}-{}", hex('a')));
        std::fs::create_dir_all(older.join("payload")).unwrap();
        std::fs::write(older.join("kind"), "copy\n").unwrap();
        // A copy still running (this process made it), and a move that did not finish.
        let me = owner_token();
        let running = made_stage(&out, &format!("{prefix}-{}", hex('b')), "copy", me.as_deref().unwrap_or("unknown"));
        let moved = made_stage(&out, &format!("{prefix}-{}", hex('c')), "move", me.as_deref().unwrap_or("unknown"));
        // Copies on another machine, and one whose maker is gone.
        let elsewhere = made_stage(&out, &format!("{prefix}-{}", hex('d')), "copy", "another-machine - 1 1\n");
        let gone = gone_owner().map(|o| made_stage(&out, &format!("{prefix}-{}", hex('e')), "copy", &o));
        let keep = Options { replace: Some(Vec::new()), ..Default::default() };
        run(&plan(std::slice::from_ref(&src), &dest, &opts, &keep).unwrap()).unwrap();
        assert_eq!(t.read("out/report"), "the report");
        for kept in [&legacy, &older, &running, &moved, &elsewhere] {
            assert!(kept.exists(), "{} was left as it is", kept.display());
        }
        assert_eq!(std::fs::read_to_string(legacy.join("kind")).unwrap(), "copy\n");
        assert_eq!(std::fs::read_to_string(running.join("payload/data")).unwrap(), "staged");
        if let Some(gone) = gone {
            assert!(!gone.exists(), "a copy that is over leaves nothing behind");
        }
        // A move refuses while staging it cannot tell is over is there, and touches none of it.
        let moving = Options { rsync: true, moving: true, replace: Some(Vec::new()), ..Default::default() };
        let back = Endpoint::Local(t.write("in/report", "the report"));
        std::fs::remove_file(t.dir.0.join("out/report")).unwrap();
        assert!(run(&plan(std::slice::from_ref(&back), &dest, &opts, &moving).unwrap()).is_err());
        assert_eq!(t.read("in/report"), "the report");
        for kept in [&legacy, &older, &running, &moved, &elsewhere] {
            assert!(kept.exists(), "{} was left as it is", kept.display());
        }
    }

    #[test]
    fn a_copy_that_may_still_run_keeps_its_staging() {
        let Some(me) = owner_token() else { return };
        let words: Vec<&str> = me.split(' ').collect();
        let (machine, boot) = (words[0], words[1]);
        let t = Tmp::new("maybe-running");
        let src = Endpoint::Local(t.write("in/report", "the report"));
        let out = t.dir.0.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let dest = Endpoint::Local(t.dir.0.join("out/"));
        let prefix = stage_prefix(std::slice::from_ref(&src), &dest);
        let hex = |c: char| std::iter::repeat_n(c, 32).collect::<String>();
        let keep = Options { replace: Some(Vec::new()), ..Default::default() };
        // Planned first: the list of what runs is made now.
        let steps = plan(std::slice::from_ref(&src), &dest, &opts, &keep).unwrap();
        // Another kraal, started after this one, stages the same copy meanwhile.
        let mut later = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let start = crate::process::start_time(later.id()).unwrap();
        let theirs = made_stage(
            &out,
            &format!("{prefix}-{}", hex('1')),
            "copy",
            &format!("{machine} {boot} {} {start}\n", later.id()),
        );
        // One that started long before this one and still runs, whatever its program (pid 1).
        let init = crate::process::start_time(1)
            .map(|s| made_stage(&out, &format!("{prefix}-{}", hex('2')), "copy", &format!("{machine} {boot} 1 {s}\n")));
        run(&steps).unwrap();
        assert_eq!(t.read("out/report"), "the report");
        assert!(theirs.exists(), "a copy started after the list was made may be running");
        assert!(init.as_ref().is_none_or(|p| p.exists()), "a running maker, whatever its name");
        // Even once it ended, a maker that started after this kraal is not judged over by it.
        let _ = later.kill();
        let _ = later.wait();
        std::fs::remove_file(out.join("report")).unwrap();
        run(&plan(std::slice::from_ref(&src), &dest, &opts, &keep).unwrap()).unwrap();
        assert!(theirs.exists());
    }

    #[test]
    fn a_user_directory_named_like_rsync_partial_files_is_copied() {
        if !rsync_capable() {
            return;
        }
        let t = Tmp::new("partial-name");
        t.write("in/project/.kraal-partial/photos", "mine");
        t.write("in/project/.kraal-partial-0123/notes", "mine too");
        std::fs::create_dir_all(t.dir.0.join("out")).unwrap();
        let replace = Options { rsync: true, ..Default::default() };
        let steps =
            plan(&[Endpoint::Local(t.dir.0.join("in/project"))], &here_at(&t.dir.0.join("out/")), &opts, &replace);
        run_here(&t, &steps.unwrap()).unwrap();
        assert_eq!(t.read("out/project/.kraal-partial/photos"), "mine");
        assert_eq!(t.read("out/project/.kraal-partial-0123/notes"), "mine too");
    }

    #[test]
    fn a_retried_copy_takes_up_what_an_interrupted_one_left() {
        if !rsync_capable() {
            return;
        }
        let t = Tmp::new("resume");
        let whole: String = (0..20000).map(|i| format!("line {i}\n")).collect();
        let src = Endpoint::Local(t.write("in/big", &whole));
        std::fs::create_dir_all(t.dir.0.join("out")).unwrap();
        let dest = here_at(&t.dir.0.join("out/"));
        let replace = Options { rsync: true, ..Default::default() };
        // What an interrupted run left: the first part, in its partial directory.
        let interrupted = plan(std::slice::from_ref(&src), &dest, &opts, &replace).unwrap();
        let flag = interrupted[0].args.iter().find(|a| a.starts_with("--partial-dir=")).unwrap();
        let partial = t.dir.0.join("out").join(flag.trim_start_matches("--partial-dir="));
        std::fs::create_dir_all(&partial).unwrap();
        std::fs::write(partial.join("big"), &whole[..whole.len() / 2]).unwrap();
        // Run again.
        run_here(&t, &plan(std::slice::from_ref(&src), &dest, &opts, &replace).unwrap()).unwrap();
        assert_eq!(t.read("out/big"), whole);
        assert!(!partial.exists(), "used, then gone: nothing piles up");
    }

    /// The bookkeeping of the copy `steps` (its rsync step's).
    fn partial_of(steps: &[Step]) -> &Partial {
        steps.iter().find_map(|s| s.partial.as_deref()).expect("a step with partial files")
    }

    /// What a run that was killed leaves: its note, its lock free. (Written directly: a lock
    /// opened here could be held on by a process another test starts meanwhile.)
    fn leave_note(partial: &Partial) {
        let dir = notes_dir();
        crate::paths::ensure_private_dir(&dir).unwrap();
        std::fs::write(dir.join(format!("{:016x}.note", partial.key)), note_text(partial)).unwrap();
    }

    #[test]
    fn a_second_run_of_a_copy_waits_for_the_first() {
        let t = Tmp::new("one-run");
        let src = Endpoint::Local(t.write("in/big", "data"));
        let dest = here_at(&t.dir.0.join("out/"));
        let steps = plan(std::slice::from_ref(&src), &dest, &opts, &with_rsync(true)).unwrap();
        let partial = partial_of(&steps).clone();
        let first = PartialRun::start(&partial).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let second = partial.clone();
        // Named like this test's thread, it shares the test's notes and locks.
        let name = std::thread::current().name().unwrap_or_default().to_string();
        let waiter = std::thread::Builder::new()
            .name(name)
            .spawn(move || {
                let run = PartialRun::start(&second).unwrap();
                tx.send(()).unwrap();
                run.finish();
            })
            .unwrap();
        assert!(rx.recv_timeout(std::time::Duration::from_millis(300)).is_err(), "the second run waits");
        first.finish();
        rx.recv_timeout(std::time::Duration::from_secs(5)).expect("it starts once the first is done");
        waiter.join().unwrap();
        assert_eq!(locks(), 0, "and neither leaves its lock");
    }

    /// The lock files in this test's notes directory.
    fn locks() -> usize {
        let files = std::fs::read_dir(notes_dir()).into_iter().flatten().flatten();
        files.filter(|e| e.file_name().to_string_lossy().ends_with(".lock")).count()
    }

    #[test]
    fn copies_leave_no_lock_behind() {
        let t = Tmp::new("locks");
        let partial = |key: u64, dir: &str| {
            let dir = t.dir.0.join(dir);
            std::fs::create_dir_all(&dir).unwrap();
            Partial {
                key,
                place: format!("here {}", dir.display()),
                tops: vec![crate::shell::quote_always(&dir.display().to_string())],
                runner: Step::new("sh", vec!["-c".into()]),
                remote: false,
            }
        };
        // Copies to many places over the weeks, some failing: each holds a lock while it runs.
        for i in 0..6 {
            let p = partial(i, &format!("out{i}"));
            let run = PartialRun::start(&p).unwrap();
            assert_eq!(locks(), 1, "held while it runs");
            if i % 2 == 0 {
                run.finish();
            } else {
                drop(run);
            }
        }
        assert_eq!(locks(), 0);
        // The lock an interrupted run left, found by a copy to the same place, goes too.
        leave_note(&partial(10, "out0"));
        std::fs::write(notes_dir().join(format!("{:016x}.lock", 10)), "").unwrap();
        PartialRun::start(&partial(11, "out0")).unwrap().finish();
        assert_eq!(locks(), 0);
    }

    #[test]
    fn what_interrupted_copies_left_goes_once_a_copy_to_the_same_place_succeeds() {
        if !rsync_capable() {
            return;
        }
        let t = Tmp::new("leftovers");
        std::fs::create_dir_all(t.dir.0.join("out/sub")).unwrap();
        let dest = here_at(&t.dir.0.join("out/"));
        let replace = with_rsync(true);
        let fake = fake_ssh(&t);
        let interrupted = |name: &str| {
            let src = Endpoint::Local(t.write(&format!("in/{name}"), "whole"));
            let steps = plan(std::slice::from_ref(&src), &dest, &opts, &replace).unwrap();
            // As run here, cleanup included: its note keeps that cleanup.
            let steps: Vec<Step> = steps.iter().map(|s| here(s, &fake, "")).collect();
            let partial = partial_of(&steps).clone();
            // Started, killed: its note stays, and so do its partial files, here in a
            // subdirectory as rsync puts them for files there.
            leave_note(&partial);
            let dir = t.dir.0.join("out/sub").join(partial_name(partial.key));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(name), "half").unwrap();
            (src, partial, dir)
        };
        // A copy interrupted, whose source was then renamed: the retry is another transfer, and
        // never uses what the first left.
        let (_, _, renamed) = interrupted("report");
        // Another one, still running.
        let (_, running, kept) = interrupted("other");
        let held = PartialRun::start(&running).unwrap();
        // The same copy as a third one, interrupted with a partial file it will not use.
        let (retried, _, own) = interrupted("again");
        std::fs::write(own.join("gone"), "half of a file no longer copied").unwrap();
        run_here(&t, &plan(std::slice::from_ref(&retried), &dest, &opts, &replace).unwrap()).unwrap();
        assert_eq!(t.read("out/again"), "whole");
        assert!(!renamed.exists(), "what the renamed copy left is gone");
        assert!(!own.exists(), "and what this copy left and did not use");
        assert!(kept.exists(), "what a running copy holds stays");
        let notes: Vec<String> = std::fs::read_dir(notes_dir())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".note"))
            .collect();
        assert!(notes.contains(&format!("{:016x}.note", running.key)), "{notes:?}");
        drop(held);
    }

    #[test]
    fn what_an_interrupted_copy_left_is_removed_where_it_put_it() {
        let t = Tmp::new("own-places");
        let place = format!("here {}", t.dir.0.display());
        let q = |p: &str| crate::shell::quote_always(&t.dir.0.join(p).display().to_string());
        let partial = |key: u64, tops: Vec<String>| Partial {
            key,
            place: place.clone(),
            tops,
            runner: Step::new("sh", vec!["-c".into()]),
            remote: false,
        };
        // Interrupted, into backup/current, a link to a directory outside backup: its partial
        // files are behind the link.
        std::fs::create_dir_all(t.dir.0.join("backup")).unwrap();
        t.write(&format!("elsewhere/{}/half", partial_name(61)), "half");
        std::os::unix::fs::symlink(t.dir.0.join("elsewhere"), t.dir.0.join("backup/current")).unwrap();
        leave_note(&partial(61, vec![q("backup/current"), q("backup")]));
        // A copy into backup/ succeeds: it looks where the interrupted one went, through its own
        // connection.
        PartialRun::start(&partial(62, vec![q("backup")])).unwrap().finish();
        assert!(!t.dir.0.join("elsewhere").join(partial_name(61)).exists(), "removed behind the link");
        assert!(!notes_dir().join(format!("{:016x}.note", 61)).exists(), "and its note goes");
        // A note that names only its place (older) is looked for where the finishing copy looks.
        t.write(&format!("backup/{}/half", partial_name(63)), "half");
        std::fs::write(notes_dir().join(format!("{:016x}.note", 63)), format!("{place}\n")).unwrap();
        PartialRun::start(&partial(64, vec![q("backup")])).unwrap().finish();
        assert!(!t.dir.0.join("backup").join(partial_name(63)).exists());
    }

    #[test]
    fn a_copy_to_a_name_and_one_into_its_directory_know_each_other() {
        let t = Tmp::new("same-place");
        std::fs::create_dir_all(t.dir.0.join("out")).unwrap();
        let src = Endpoint::Local(t.write("in/report", "data"));
        let place = |dest: &Endpoint| {
            let steps = plan(std::slice::from_ref(&src), dest, &opts, &with_rsync(true)).unwrap();
            partial_of(&steps).place.clone()
        };
        // Copied as out/report, or into out/: both runs receive in out, and what one left the
        // other cleans up.
        assert_eq!(place(&here_at(&t.dir.0.join("out/report"))), place(&here_at(&t.dir.0.join("out/"))));
    }

    #[test]
    fn each_machine_and_user_keeps_its_own_partial_files() {
        let t = Tmp::new("clients");
        let src = Endpoint::Local(t.write("in/big", "data"));
        let dest = here_at(&t.dir.0.join("out/"));
        let key = |client: &str| partial_key(client, std::slice::from_ref(&src), &dest);
        let ours = key("machine-a 1000");
        assert_eq!(ours, key("machine-a 1000"), "a retry here takes up what it left");
        let theirs = key("machine-b 1000");
        assert_ne!(ours, theirs, "another machine copying the same way never shares them");
        assert_ne!(ours, key("machine-a 1001"), "nor does another user");
        let steps = plan(std::slice::from_ref(&src), &dest, &opts, &with_rsync(true)).unwrap();
        let mine = partial_of(&steps).key;
        assert_eq!(mine, key(&client_id()));
        let flag = format!("--partial-dir={}", partial_name(mine));
        assert!(steps.iter().any(|s| s.args.contains(&flag)), "{steps:?}");
        // A cleanup removes only the names it is given, which come from this machine's notes:
        // what another machine is still copying stays.
        for k in [ours, theirs] {
            t.write(&format!("out/{}/big", partial_name(k)), "half");
        }
        let out = crate::shell::quote_always(&t.dir.0.join("out").display().to_string());
        run_with_input(
            &Step::new("sh", vec!["-c".into(), cleanup_script(std::slice::from_ref(&out))]),
            &partial_name(ours),
        )
        .unwrap();
        assert!(!t.dir.0.join("out").join(partial_name(ours)).exists());
        assert!(t.dir.0.join("out").join(partial_name(theirs)).exists());
    }

    #[test]
    fn a_linked_destination_is_cleaned_where_the_copy_went() {
        let t = Tmp::new("linked-dest");
        let name = partial_name(42);
        t.write(&format!("real/sub/{name}/big"), "half");
        std::os::unix::fs::symlink(t.dir.0.join("real"), t.dir.0.join("current")).unwrap();
        let cleanup = |top: &str| {
            let top = crate::shell::quote_always(&t.dir.0.join(top).display().to_string());
            Step::new("sh", vec!["-c".into(), cleanup_script(&[top])])
        };
        run_with_input(&cleanup("current"), &name).unwrap();
        assert!(!t.dir.0.join("real/sub").join(&name).exists(), "found through the link, as the copy went");
        // Named without `/`, a linked directory is where rsync put things, not its parent: both
        // are looked at.
        t.write(&format!("real/{name}/big"), "half");
        let both: Vec<String> = ["current", ""]
            .iter()
            .map(|p| crate::shell::quote_always(&t.dir.0.join(p).display().to_string()))
            .collect();
        run_with_input(&Step::new("sh", vec!["-c".into(), cleanup_script(&both)]), &name).unwrap();
        assert!(!t.dir.0.join("real").join(&name).exists(), "found in the linked destination named without /");
        // Where nothing can be looked at, the cleanup fails and the notes stay for a later run.
        std::os::unix::fs::symlink(t.dir.0.join("gone"), t.dir.0.join("dangling")).unwrap();
        let partial = Partial {
            key: 43,
            place: format!("here {}", t.dir.0.display()),
            tops: vec![crate::shell::quote_always(&t.dir.0.join("dangling").display().to_string())],
            runner: Step::new("sh", vec!["-c".into()]),
            remote: false,
        };
        drop(PartialRun::start(&partial).unwrap());
        PartialRun::start(&partial).unwrap().finish();
        assert!(notes_dir().join(format!("{:016x}.note", 43)).exists(), "the note stays");
    }

    #[test]
    fn only_a_directory_from_another_server_needs_the_directory_installer() {
        let t = Tmp::new("probe");
        t.write("in/note", "note");
        t.write("in/tree/a", "a");
        let other = Server { name: "other".into(), host: "other.lan".into(), ..Default::default() };
        let from = |p: &str| Endpoint::Remote { server: other.clone(), path: t.dir.0.join(p).display().to_string() };
        let dest = here_at(&t.dir.0.join("out/"));
        // The other server, asked here.
        let ask = |_: &Server, script: &str| -> Result<String> {
            let out = std::process::Command::new("sh").args(["-c", script]).output()?;
            Ok(String::from_utf8_lossy(&out.stdout).into_owned())
        };
        assert!(!directory_test(&[from("in/note")], &dest, &ask).unwrap().contains("need=1"));
        assert!(directory_test(&[from("in/tree")], &dest, &ask).unwrap().contains("need=1"));
        assert!(directory_test(&[from("in/")], &dest, &ask).unwrap().contains("need=1"), "in/ holds a directory");
        // Not answering counts as maybe.
        let silent = |_: &Server, _: &str| -> Result<String> { bail!("no answer") };
        assert!(directory_test(&[from("in/note")], &dest, &silent).unwrap().contains("need=1"));
    }

    #[test]
    fn an_interrupted_move_is_never_written_over() {
        let t = Tmp::new("pending-move");
        t.write("in/report", "the original");
        std::fs::create_dir_all(t.dir.0.join("out")).unwrap();
        let keep = Options { moving: true, replace: Some(Vec::new()), dest_dir: Some(true), ..Default::default() };
        let (src, dest) = (here_at(&t.dir.0.join("in/report")), here_at(&t.dir.0.join("out/")));
        let steps = plan(std::slice::from_ref(&src), &dest, &opts, &keep).unwrap();
        // Interrupted once the source is staged, before it is put in place.
        let (_, before) = steps.split_last().unwrap();
        run_here(&t, before).unwrap();
        assert!(!t.dir.0.join("in/report").exists());
        let found = stages(&t.dir.0.join("out"));
        let [stage] = &found[..] else { panic!("one staging directory: {found:?}") };
        // The report comes back to where it was (its producer wrote it again); moving again must
        // not touch the staged one, the only copy of the original.
        t.write("in/report", "a new one");
        let again = plan(std::slice::from_ref(&src), &dest, &opts, &keep).unwrap();
        assert!(run_here(&t, &again).is_err(), "the earlier move is named, nothing is done");
        assert_eq!(std::fs::read_to_string(stage.join("payload/report")).unwrap(), "the original");
        assert_eq!(t.read("in/report"), "a new one");
        assert!(!t.dir.0.join("out/report").exists());
        // An interrupted copy is another matter: its sources are still there, and once the copy
        // that made the staging no longer runs, it is started over.
        let copy = Options { replace: Some(Vec::new()), dest_dir: Some(true), ..Default::default() };
        std::fs::remove_dir_all(stage).unwrap();
        let steps = plan(std::slice::from_ref(&src), &dest, &opts, &copy).unwrap();
        run_here(&t, &steps[..steps.len() - 1]).unwrap();
        let Some(dead) = gone_owner() else { return };
        let [stage] = &stages(&t.dir.0.join("out"))[..] else { panic!("one staging directory") };
        std::fs::write(stage.join("owner"), dead).unwrap();
        run_here(&t, &plan(std::slice::from_ref(&src), &dest, &opts, &copy).unwrap()).unwrap();
        assert_eq!(t.read("out/report"), "a new one");
        assert!(stages(&t.dir.0.join("out")).is_empty(), "the interrupted copy's staging is gone");
    }

    #[test]
    fn a_retried_copy_never_writes_through_what_an_earlier_one_staged() {
        let t = Tmp::new("stale-link");
        t.write("victim", "not to be touched");
        std::fs::create_dir_all(t.dir.0.join("in")).unwrap();
        std::fs::create_dir_all(t.dir.0.join("out")).unwrap();
        std::os::unix::fs::symlink(t.dir.0.join("victim"), t.dir.0.join("in/item")).unwrap();
        let keep = Options { replace: Some(Vec::new()), dest_dir: Some(true), ..Default::default() };
        let (src, dest) = (here_at(&t.dir.0.join("in/item")), here_at(&t.dir.0.join("out")));
        let steps = plan(std::slice::from_ref(&src), &dest, &opts, &keep).unwrap();
        run_here(&t, &steps[..steps.len() - 1]).unwrap();
        // The source is now a file of its own; the copy is done again.
        std::fs::remove_file(t.dir.0.join("in/item")).unwrap();
        t.write("in/item", "the file asked for");
        run_here(&t, &plan(std::slice::from_ref(&src), &dest, &opts, &keep).unwrap()).unwrap();
        assert_eq!(t.read("victim"), "not to be touched");
        assert!(std::fs::symlink_metadata(t.dir.0.join("out/item")).unwrap().is_file());
        assert_eq!(t.read("out/item"), "the file asked for");
    }

    #[test]
    fn staging_names_are_never_taken_from_the_sources() {
        let t = Tmp::new("names");
        let names = [".kraal-partial", ".as", "payload", "work", "kind", "ref", "format", "owner"];
        let sources: Vec<Endpoint> = names.iter().map(|n| here_at(&t.write(&format!("in/{n}"), n))).collect();
        std::fs::create_dir_all(t.dir.0.join("out")).unwrap();
        let keep = Options { replace: Some(Vec::new()), ..Default::default() };
        run_here(&t, &plan(&sources, &here_at(&t.dir.0.join("out/")), &opts, &keep).unwrap()).unwrap();
        for n in names {
            assert_eq!(t.read(&format!("out/{n}")), n);
        }
        // One source renamed to a name of kraal's own staging: it takes it.
        let one = here_at(&t.dir.0.join("in/.as"));
        let to = here_at(&t.dir.0.join("out/.as-renamed"));
        let keep_new = Options { dest_dir: Some(false), ..keep.clone() };
        run_here(&t, &plan(&[one], &to, &opts, &keep_new).unwrap()).unwrap();
        assert_eq!(t.read("out/.as-renamed"), ".as");
        // A move delivers them too.
        let moving = Options { moving: true, ..keep };
        let partial = here_at(&t.write("in2/.kraal-partial", "moved"));
        std::fs::create_dir_all(t.dir.0.join("out2")).unwrap();
        run_here(&t, &plan(&[partial], &here_at(&t.dir.0.join("out2/")), &opts, &moving).unwrap()).unwrap();
        assert_eq!(t.read("out2/.kraal-partial"), "moved");
        assert!(!t.dir.0.join("in2/.kraal-partial").exists());
        assert!(stages(&t.dir.0.join("out")).is_empty() && stages(&t.dir.0.join("out2")).is_empty());
    }

    #[test]
    fn a_retried_directory_copy_takes_its_name_once() {
        // Without GNU mv 8.30 (macOS, BSD) a directory is refused before anything is put.
        if !mv_keeps_here() {
            return;
        }
        let t = Tmp::new("retry-dir");
        t.write("in/tree/a", "a");
        t.write("in/tree/sub/b", "b");
        std::fs::create_dir_all(t.dir.0.join("out")).unwrap();
        let keep = Options { replace: Some(Vec::new()), dest_dir: Some(false), ..Default::default() };
        let (src, dest) = (here_at(&t.dir.0.join("in/tree")), here_at(&t.dir.0.join("out/new")));
        let steps = plan(std::slice::from_ref(&src), &dest, &opts, &keep).unwrap();
        let (install, before) = steps.split_last().unwrap();
        run_here(&t, before).unwrap();
        // A file takes the name meanwhile: nothing is put in place.
        t.write("out/new", "theirs");
        assert!(run_here(&t, std::slice::from_ref(install)).is_err());
        assert_eq!(t.read("out/new"), "theirs");
        // It goes; the copy is done again, and takes the name with what the source holds.
        std::fs::remove_file(t.dir.0.join("out/new")).unwrap();
        run_here(&t, &plan(std::slice::from_ref(&src), &dest, &opts, &keep).unwrap()).unwrap();
        assert_eq!((t.read("out/new/a").as_str(), t.read("out/new/sub/b").as_str()), ("a", "b"));
        assert!(!t.dir.0.join("out/new/tree").exists(), "not one level too deep");
    }

    #[test]
    fn a_destination_directory_holds_its_own_staging() {
        let t = Tmp::new("stage-where");
        let f = t.write("file", "x");
        let home = t.dir.0.join("home");
        let alice = home.join("alice");
        std::fs::create_dir_all(&alice).unwrap();
        // What holds the destination cannot be written; the destination itself can.
        std::fs::set_permissions(&home, std::os::unix::fs::PermissionsExt::from_mode(0o555)).unwrap();
        let keep = Options { replace: Some(Vec::new()), ..Default::default() };
        let local = run(&plan(&[Endpoint::Local(f.clone())], &Endpoint::Local(alice.clone()), &opts, &keep).unwrap());
        let on_server = Options { dest_dir: Some(true), ..keep.clone() };
        let g = t.write("other", "y");
        let remote = run_here(&t, &plan(&[here_at(&g)], &here_at(&alice), &opts, &on_server).unwrap());
        std::fs::set_permissions(&home, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        local.unwrap();
        remote.unwrap();
        assert_eq!((t.read("home/alice/file").as_str(), t.read("home/alice/other").as_str()), ("x", "y"));
        assert!(stages(&alice).is_empty() && stages(&home).is_empty());
    }

    #[test]
    fn remote_names_are_not_patterns() {
        if !rsync_capable() {
            return;
        }
        let t = Tmp::new("literal");
        t.write("src/backup[1]", "the one asked for");
        t.write("src/backup1", "a neighbour");
        std::fs::create_dir_all(t.dir.0.join("dst")).unwrap();
        let here = Server { name: "here".into(), host: "localhost".into(), ..Default::default() };
        let src = Endpoint::Remote { server: here, path: format!("{}/src/backup[1]", t.dir.0.display()) };
        let moving = Options { rsync: true, moving: true, ..Default::default() };
        let mut steps = plan(&[src], &Endpoint::Local(t.dir.0.join("dst/")), &opts, &moving).unwrap();
        let e = steps[0].args.iter().position(|a| a == "-e").unwrap();
        steps[0].args[e + 1] = fake_ssh(&t);
        run(&steps).unwrap();
        assert_eq!(t.read("dst/backup[1]"), "the one asked for");
        assert!(!t.dir.0.join("dst/backup1").exists() && !t.dir.0.join("src/backup[1]").exists());
        assert_eq!(t.read("src/backup1"), "a neighbour", "a neighbouring name is neither copied nor deleted");
        assert_eq!(rsync_literal("plain\\name"), "plain\\name");
        assert_eq!(rsync_literal("a*[b]?"), "a\\*\\[b\\]\\?");
    }

    #[test]
    fn local_tar_sources_are_found_whatever_came_before() {
        let src = [Endpoint::Local("a/one".into()), Endpoint::Local("b/two".into())];
        let step = pack_local(&src).unwrap();
        let dirs: Vec<&String> = step.args.windows(2).filter(|w| w[0] == "-C").map(|w| &w[1]).collect();
        assert_eq!(dirs.len(), 2, "{:?}", step.args);
        assert!(
            dirs.iter().all(|d| Path::new(d).is_absolute()),
            "a relative -C follows the previous one: {:?}",
            step.args
        );
    }

    #[test]
    fn a_directory_can_be_copied_by_its_contents() {
        assert!(source_name(&Endpoint::Local(".".into())).unwrap().1);
        assert!(source_name(&Endpoint::Local("dir/.".into())).unwrap().1);
        assert!(source_name(&Endpoint::Local("..".into())).is_err());
        let k = known();
        let steps =
            plan(&[Endpoint::Local(".".into())], &parse_endpoint("b:/backup/", &k).unwrap(), &opts, &with_rsync(true));
        assert!(steps.unwrap()[0].args.contains(&".".to_string()));
        let v6 = parse_endpoint("[::1]:/tmp/x", &k).unwrap();
        assert!(matches!(&v6, Endpoint::Remote { server, path } if server.host == "::1" && path == "/tmp/x"), "{v6:?}");
    }

    #[test]
    fn moves_on_a_server_are_renames_only() {
        let t = Tmp::new("samefs");
        t.write("a/f", "x");
        let here = Server { name: "here".into(), host: "localhost".into(), ..Default::default() };
        let moving = Options { rsync: true, moving: true, ..Default::default() };
        let script = |from: &Path, to: &Path| {
            let src = Endpoint::Remote { server: here.clone(), path: from.display().to_string() };
            let dest = Endpoint::Remote { server: here.clone(), path: format!("{}/", to.display()) };
            let steps = plan(&[src], &dest, &opts, &moving).unwrap();
            let line = steps[0].args.last().unwrap().clone();
            std::process::Command::new("sh").args(["-c", &line]).status().unwrap().success()
        };
        assert!(script(&t.dir.0.join("a/f"), &t.dir.0.join("b")));
        assert_eq!(t.read("b/f"), "x");
        // Another file system: mv would copy, then delete the whole source; it is refused.
        let shm = Path::new("/dev/shm");
        let dev = |p: &Path| std::os::unix::fs::MetadataExt::dev(&std::fs::metadata(p).unwrap());
        if shm.is_dir() && dev(shm) != dev(&t.dir.0) {
            let other = shm.join(format!("kraal-samefs-{}", std::process::id()));
            assert!(!script(&t.dir.0.join("b/f"), &other));
            assert_eq!(t.read("b/f"), "x");
            // An existing directory named without a trailing slash receives the source too: its
            // own file system counts, not that of what holds it.
            std::fs::create_dir_all(&other).unwrap();
            let src = Endpoint::Remote { server: here.clone(), path: t.dir.0.join("b").display().to_string() };
            let dest = Endpoint::Remote { server: here.clone(), path: other.display().to_string() };
            let steps = plan(&[src], &dest, &opts, &moving).unwrap();
            let line = steps[0].args.last().unwrap().clone();
            assert!(!std::process::Command::new("sh").args(["-c", &line]).status().unwrap().success());
            assert_eq!(t.read("b/f"), "x");
            // The same through a link to it: where the link leads counts, not where it is.
            std::os::unix::fs::symlink(&other, t.dir.0.join("usb")).unwrap();
            let src = Endpoint::Remote { server: here.clone(), path: t.dir.0.join("b").display().to_string() };
            let dest = Endpoint::Remote { server: here.clone(), path: t.dir.0.join("usb").display().to_string() };
            let steps = plan(&[src], &dest, &opts, &moving).unwrap();
            let line = steps[0].args.last().unwrap().clone();
            assert!(!std::process::Command::new("sh").args(["-c", &line]).status().unwrap().success());
            assert_eq!(t.read("b/f"), "x");
            assert!(!other.join("b").exists());
            let _ = std::fs::remove_dir_all(other);
        }
    }

    #[test]
    fn a_server_that_is_this_machine_is_seen_before_a_move() {
        let t = Tmp::new("self");
        let f = t.write("data/report", "only copy");
        let dir = t.dir.0.join("data");
        let marks = Marks { next_to: ".kraal-move-test-by".into(), inside: ".kraal-move-test-in".into() };
        // This machine to "a server" that is this machine (the script runs here).
        let seen = |dest: &Path, into: bool| {
            let quoted = crate::shell::quote_always(&dest.display().to_string());
            let out =
                std::process::Command::new("sh").args(["-c", &seen_script(&quoted, into, &marks)]).output().unwrap();
            String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect::<Vec<_>>()
        };
        let mut placed = LocalMarkers(Vec::new());
        placed.put(&f, false, &marks).unwrap();
        placed.put(&dir, false, &marks).unwrap();
        assert!(seen(&dir, true).contains(&"next to".to_string()), "into the source's own directory");
        assert!(seen(&dir, false).contains(&"into".to_string()), "an existing directory receives the sources");
        assert!(seen(&dir.join("new/deeper"), true).contains(&"inside".to_string()), "inside a source");
        assert_eq!(seen(&t.dir.0.join("elsewhere"), true), ["into"], "a sibling is another place");
        drop(placed);
        assert!(
            std::fs::read_dir(&dir).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().starts_with(".kraal"))
        );
        // From "a server" that is this machine: what is marked there is seen here.
        let paths = [(crate::shell::quote_always(&dir.display().to_string()), false)];
        let ok = std::process::Command::new("sh").args(["-c", &place_script(&paths, &marks)]).status().unwrap();
        assert!(ok.success());
        assert_eq!(local_seen(&dir.join("sub"), true, &marks), (false, true));
        assert_eq!(local_seen(&t.dir.0, true, &marks), (true, false));
        assert_eq!(local_seen(&t.dir.0.join("other"), true, &marks), (false, false), "a sibling is another place");
        std::process::Command::new("sh").args(["-c", &unplace_script(&paths, &marks)]).status().unwrap();
        assert!(!dir.join(&marks.inside).exists() && !t.dir.0.join(&marks.next_to).exists());
    }

    #[test]
    fn a_move_check_sees_through_odd_names_and_needs_no_more_than_the_source() {
        let t = Tmp::new("odd");
        let marks = Marks { next_to: ".kraal-move-odd-by".into(), inside: ".kraal-move-odd-in".into() };
        let seen = |dest: &Path, into: bool| {
            let quoted = crate::shell::quote_always(&dest.display().to_string());
            let out =
                std::process::Command::new("sh").args(["-c", &seen_script(&quoted, into, &marks)]).output().unwrap();
            String::from_utf8_lossy(&out.stdout).lines().map(str::to_string).collect::<Vec<_>>()
        };
        // A directory whose name ends in a newline: `$(dirname …)` would cut it and look elsewhere.
        let dir = t.dir.0.join("data\n");
        let f = dir.join("report");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&f, "only copy").unwrap();
        let mut placed = LocalMarkers(Vec::new());
        placed.put(&f, false, &marks).unwrap();
        assert!(seen(&f, false).contains(&"next to".to_string()), "the report onto itself");
        drop(placed);
        let paths = [(crate::shell::quote_always(&f.display().to_string()), false)];
        assert!(
            std::process::Command::new("sh").args(["-c", &place_script(&paths, &marks)]).status().unwrap().success()
        );
        assert!(dir.join(&marks.next_to).exists());
        std::process::Command::new("sh").args(["-c", &unplace_script(&paths, &marks)]).status().unwrap();
        assert!(!dir.join(&marks.next_to).exists());
        // Moving what a directory holds needs that directory, not the one holding it.
        let home = t.dir.0.join("home");
        let mine = home.join("alice");
        std::fs::create_dir_all(&mine).unwrap();
        std::fs::write(mine.join("notes"), "x").unwrap();
        std::fs::set_permissions(&home, std::os::unix::fs::PermissionsExt::from_mode(0o555)).unwrap();
        let mut placed = LocalMarkers(Vec::new());
        let put = placed.put(&PathBuf::from(format!("{}/", mine.display())), true, &marks);
        let mine_word = [(crate::shell::quote_always(&mine.display().to_string()), true)];
        let remote_put = std::process::Command::new("sh").args(["-c", &place_script(&mine_word, &marks)]).status();
        std::fs::set_permissions(&home, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        put.unwrap();
        assert!(remote_put.unwrap().success());
        assert!(seen(&mine, true).contains(&"inside".to_string()), "onto itself");
        assert_eq!(seen(&home.join("bob"), true), ["into"], "elsewhere");
        drop(placed);
        std::process::Command::new("sh").args(["-c", &unplace_script(&mine_word, &marks)]).status().unwrap();
        assert!(!mine.join(&marks.inside).exists());
    }

    #[test]
    fn the_root_of_a_server_is_the_root() {
        assert_eq!(pack_remote(&["/".into()]), "tar cf - -C / .");
        assert_eq!(pack_remote(&["/.".into()]), "tar cf - -C / .");
        assert_eq!(pack_remote(&["".into()]), "tar cf - -C \"$HOME\" .");
        assert_eq!(pack_remote(&["srv/".into()]), "tar cf - -C \"$HOME\"/srv .");
        assert_eq!(pack_remote(&["/srv/.".into()]), "tar cf - -C /srv .");
        assert_eq!(pack_remote(&["/etc/hosts".into()]), "tar cf - -C /etc ./hosts");
        let k = known();
        let steps = plan(
            &[parse_endpoint("a:/", &k).unwrap()],
            &parse_endpoint("b:/backup/", &k).unwrap(),
            &opts,
            &with_rsync(true),
        )
        .unwrap();
        assert_eq!(crate::shell::unwrap_sh_c(steps[0].args.last().unwrap()), "tar cf - -C / .");
    }

    #[test]
    fn dropped_paths_handle_local_uris_quotes_and_unique_raw_names_without_shell_expansion() {
        let t = Tmp::new("dropped-paths");
        let names =
            ["notes client.txt", "single'quote", "double\"quote", "back\\slash", "$literal", "brackets[1]", "café.png"];
        for name in names {
            let path = t.dir.0.join(name);
            std::fs::write(&path, b"x").unwrap();
            assert_eq!(dropped_paths(&crate::shell::quote(path.to_str().unwrap())), Some(vec![path.clone()]));
        }
        let path = t.dir.0.join("notes client.txt");
        assert_eq!(dropped_paths(path.to_str().unwrap()), Some(vec![path.clone()]));
        let uri = format!("file://{}", path.display().to_string().replace(' ', "%20"));
        assert_eq!(dropped_paths(&uri), Some(vec![path.clone()]));
        assert_eq!(
            dropped_paths(&format!("  '{uri}'\r\n'{}'\t", path.display())),
            Some(vec![path.clone(), path.clone()])
        );
        for text in [
            format!("cat {}", path.display()),
            format!("{}\\", path.display()),
            format!("'{}", path.display()),
            format!("file://remote{}", path.display()),
            format!("{uri}%"),
            format!("{uri}%00"),
            format!("{uri}?query"),
            format!("{uri}#fragment"),
            format!("{}/$literal", t.dir.0.display()),
            format!("{}/brackets[1]", t.dir.0.display()),
            "/$(touch /tmp/no)".into(),
            "relative".into(),
            String::new(),
        ] {
            assert_eq!(dropped_paths(&text), None, "{text}");
        }
        let a = t.dir.0.join("a");
        let b = t.dir.0.join("b");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        let text = format!("{} {}", a.display(), b.display());
        let raw = PathBuf::from(&text);
        std::fs::create_dir_all(raw.parent().unwrap()).unwrap();
        std::fs::write(&raw, b"ambiguous").unwrap();
        assert_eq!(dropped_paths(&text), None);
        assert_eq!(dropped_paths(&format!("'{}' '{}'", a.display(), b.display())), Some(vec![a, b]));
        assert_eq!(dropped_paths(&format!("'{text}'")), Some(vec![raw]));
        let c = t.dir.0.join("c");
        std::fs::write(&c, b"c").unwrap();
        assert_eq!(dropped_paths(&format!("{text} {}", c.display())), None);
        assert_eq!(dropped_paths(&format!("'{}' {text}", c.display())), None);
        let backslash = t.dir.0.join("back\\slash");
        assert_eq!(dropped_paths(&format!("\"{}\"", backslash.display())), Some(vec![backslash]));
    }

    #[test]
    fn dropped_paths_parsing() {
        let dir = std::env::temp_dir().join(format!("kraal drop {}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("it's here.txt");
        std::fs::write(&f, "x").unwrap();
        let escaped = f.display().to_string().replace(' ', "\\ ").replace('\'', "\\'");
        assert_eq!(dropped_paths(&escaped), Some(vec![f.clone()]));
        let quoted = format!("'{}' {}", dir.display(), escaped);
        assert_eq!(dropped_paths(&quoted), Some(vec![dir.clone(), f.clone()]));
        assert_eq!(dropped_paths("echo hello"), None);
        assert_eq!(dropped_paths("relative/path"), None);
        assert_eq!(
            dropped_paths(&format!("file://{}", dir.display().to_string().replace(' ', "\\ "))),
            Some(vec![dir.clone()])
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn quoting() {
        assert_eq!(crate::shell::quote("plain/path"), "plain/path");
        assert_eq!(crate::shell::quote(""), "''");
        assert_eq!(remote_path("~/x"), "x");
        assert_eq!(remote_path("~"), "");
        assert_eq!(split_parent("/etc/hosts"), ("/etc".into(), "hosts".into()));
        assert_eq!(split_parent("dir/"), ("".into(), "dir".into()));
    }
}
