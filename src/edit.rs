//! Editing a file of a server in the local editor: what runs on the server to read the file and
//! to write a new version of it, and the private local copy the editor works on. The session
//! itself (the editor's pane, each save sent back, the questions) is the server's
//! (`server::edits`).
//!
//! Nothing is written blindly: the server's version is identified by a hash made there when it
//! is read, and a new version replaces it only while that hash is still the same. A new version
//! is written next to the file, then renamed over it, with the file's mode (and owner, when the
//! user may give it): an interrupted save leaves the old file whole.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

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

/// What reads the file at `path` (absolute, `~/…`, or relative to the home directory): it
/// follows symbolic links, refuses what is not a regular file it may read and replace, or is
/// over [`MAX_SIZE`], and prints [`OPENED`], the file's directory, name, identity and size, its
/// content, then its identity again (a file that changed while it was read is told apart).
pub fn open_script(path: &str) -> String {
    let f = crate::files::remote_dir_arg(path);
    let lines = [
        IDENTIFY.to_string(),
        format!("f={f}"),
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
        "if [ ! -r \"$n\" ]; then echo 'you may not read it' >&2; exit 3; fi".into(),
        "if [ ! -w \"$n\" ]; then \
         o=$(ls -ld -- \"$n\" | awk '{print $3}'); \
         echo \"you may not write it (it belongs to $o), and kraal does not use sudo\" >&2; exit 3; fi"
            .into(),
        "if [ ! -w . ]; then echo 'you may not write in its directory, where its new versions are written before they replace it' >&2; exit 3; fi"
            .into(),
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
    ];
    lines.join("\n")
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
        (Ok(d), Ok(n)) if !d.is_empty() && !n.is_empty() => (d.to_string(), n.to_string()),
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

/// What writes the bytes it reads as the new version of `file`, if the file is still the one
/// `expected` identifies, and has `len` bytes once there: a new file next to it, with its mode
/// (and owner when allowed), renamed over it. Prints [`SAVED`] and the new identity, or
/// [`CHANGED`] and the identity of what is there now.
pub fn save_script(file: &RemoteFile, expected: &str, len: usize) -> String {
    let q = crate::shell::quote_always;
    let lines = [
        IDENTIFY.to_string(),
        format!("cd -- {} || exit 3", q(&file.dir)),
        format!("n={}", q(&file.name)),
        "if [ -L \"$n\" ] || [ ! -f \"$n\" ]; then echo 'it was removed, or replaced by something that is not a regular file' >&2; exit 3; fi"
            .into(),
        "h=$(k \"$n\") || exit 3".into(),
        format!("if [ \"$h\" != {} ]; then printf 'KRAAL:CHANGED\\0%s\\0' \"$h\"; exit 0; fi", q(expected)),
        "t=$(mktemp ./.kraal-edit-XXXXXX) || exit 3".into(),
        "trap 'rm -f -- \"$t\"' EXIT".into(),
        "trap 'exit 1' HUP INT TERM".into(),
        // A copy first, for the mode, owner, group and ACLs; then the new content in it.
        "cp -p -- \"$n\" \"$t\" || exit 3".into(),
        "cat > \"$t\" || exit 3".into(),
        "s=$(wc -c < \"$t\") || exit 3".into(),
        "set -- $s".into(),
        format!(
            "if [ \"$1\" != {len} ]; then echo 'the new version did not arrive whole: the file was left as it was' >&2; exit 3; fi"
        ),
        "h=$(k \"$t\") || exit 3".into(),
        "mv -f -- \"$t\" \"$n\" || exit 3".into(),
        "trap - EXIT".into(),
        "printf 'KRAAL:SAVED\\0%s\\0' \"$h\"".into(),
    ];
    lines.join("\n")
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

/// Put `content` in a new private directory (0700) under `base`, as a file named `name` that
/// only the user may read (0600): what the editor works on. Returns the file.
pub fn local_copy(base: &Path, name: &str, content: &[u8]) -> std::io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    crate::paths::ensure_private_dir(base)?;
    let stamp = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or_default();
    for n in 0..100 {
        let dir = base.join(format!("{}-{stamp:x}-{n}", std::process::id()));
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => {
                let file = dir.join(name);
                if let Err(e) = write_new(&file, content) {
                    let _ = std::fs::remove_dir_all(&dir);
                    return Err(e);
                }
                return Ok(file);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other("no free name for a private directory"))
}

/// Keep `content` in `dir` as `name`, or `name-1`, `name-2`… (before its extension) when that
/// is taken: nothing there is replaced. Only the user may read it (0600).
pub fn keep(dir: &Path, name: &str, content: &[u8]) -> std::io::Result<PathBuf> {
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
