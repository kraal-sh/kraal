use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::{Account, label};
use crate::paths;

/// Whether conversation `session` exists for account `a`, when the agent's files tell (Claude
/// keeps a transcript per conversation, once something was said in it); `None` when they do not
/// (another agent, an odd id). An error when they could not be read: that is not "no".
pub fn conversation_exists(a: &Account, session: &str) -> Result<Option<bool>> {
    if a.agent != "claude" || !session.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return Ok(None);
    }
    Ok(Some(transcript(a, session)?.is_some()))
}

/// Verify the saved conversation before promising a return to it.
pub fn check_resume(a: &Account, session: &str) -> Result<()> {
    anyhow::ensure!(
        !session.is_empty() && session.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
        "could not verify the conversation id"
    );
    if a.agent == "claude" {
        return check_carry(a, a, session);
    }
    fn find(dir: &Path, session: &str, depth: usize) -> Result<bool> {
        use std::io::{BufRead, Read};
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(e) => return Err(e.into()),
        };
        for entry in entries {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() && depth < 3 && find(&entry.path(), session, depth + 1)? {
                return Ok(true);
            }
            if kind.is_file() && entry.file_name().to_str().is_some_and(|n| n.ends_with(&format!("-{session}.jsonl"))) {
                let file = std::fs::File::open(entry.path())?;
                let mut first = String::new();
                std::io::BufReader::new(file.take(64 * 1024)).read_line(&mut first)?;
                if let Ok(meta) = serde_json::from_str::<serde_json::Value>(&first)
                    && meta["type"] == "session_meta"
                    && meta["payload"]["id"] == session
                {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
    anyhow::ensure!(
        a.agent == "codex" && find(&paths::expand_tilde(&a.home).join("sessions"), session, 0)?,
        "could not find the saved conversation on {}",
        label(a)
    );
    Ok(())
}

/// Where conversation `session`'s transcript is in account `a`'s home, if anywhere. An error
/// when a directory where it may be could not be read.
fn transcript(a: &Account, session: &str) -> Result<Option<PathBuf>> {
    let file = format!("{session}.jsonl");
    let projects = paths::expand_tilde(&a.home).join("projects");
    let dirs = match std::fs::read_dir(&projects) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", projects.display())),
    };
    for dir in dirs {
        let path = dir.with_context(|| format!("reading {}", projects.display()))?.path().join(&file);
        match std::fs::metadata(&path) {
            Ok(m) if m.is_file() => return Ok(Some(path)),
            Ok(_) => {}
            // A project directory without this conversation, or not a directory at all.
            Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT) | Some(libc::ENOTDIR)) => {}
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }
    Ok(None)
}

// Bound each snapshot, including files that grow after the metadata check.
const MAX_TRANSCRIPT_BYTES: u64 = 16 * 1024 * 1024;

struct Revision {
    metadata: std::fs::Metadata,
    data: Vec<u8>,
}

impl Revision {
    fn read(path: &Path) -> Result<Self> {
        use std::io::Read;
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(path)?;
        let metadata = file.metadata()?;
        anyhow::ensure!(metadata.is_file(), "{} is not a regular transcript", path.display());
        anyhow::ensure!(metadata.len() <= MAX_TRANSCRIPT_BYTES, "{} is too large to transfer safely", path.display());
        let mut data = Vec::new();
        (&file).take(MAX_TRANSCRIPT_BYTES + 1).read_to_end(&mut data)?;
        anyhow::ensure!(
            data.len() as u64 <= MAX_TRANSCRIPT_BYTES,
            "{} is too large to transfer safely",
            path.display()
        );
        let after = file.metadata()?;
        anyhow::ensure!(
            same_revision(&metadata, &after)
                && metadata.ctime() == after.ctime()
                && metadata.ctime_nsec() == after.ctime_nsec(),
            "{} changed while reading it; try again",
            path.display()
        );
        Ok(Self { metadata, data })
    }

    fn matches(&self, path: &Path) -> Result<bool> {
        let now = Self::read(path)?;
        Ok(same_revision(&self.metadata, &now.metadata) && self.data == now.data)
    }
}

fn same_revision(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
}

enum Carry {
    Nothing,
    There,
    Copy { data: Vec<u8>, dst: PathBuf, previous: Option<Box<Revision>> },
}

fn valid_session(session: &str) -> Result<()> {
    anyhow::ensure!(
        !session.is_empty() && session.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
        "could not verify the Claude conversation id"
    );
    Ok(())
}

fn destination_idle(to: &Account, dst: Option<&Path>) -> Result<()> {
    let home = paths::expand_tilde(&to.home);
    let home = home.canonicalize().unwrap_or(home);
    // Agents can close the transcript between turns, so an open-file check alone is not enough.
    let in_home = crate::process::pids().into_iter().any(|pid| {
        if pid == std::process::id() {
            return false;
        }
        let Some((argv, env)) = crate::process::command_env(pid, &["CLAUDE_CONFIG_DIR", "HOME"]) else { return false };
        let path = if let Some(value) = env.get("CLAUDE_CONFIG_DIR") {
            paths::expand_tilde(value)
        } else if crate::agent::detect_agent(&argv) == Some("claude") {
            let Some(value) = env.get("HOME") else { return false };
            Path::new(value).join(".claude")
        } else {
            return false;
        };
        path.canonicalize().unwrap_or(path) == home
    });
    anyhow::ensure!(
        !in_home && dst.is_none_or(|path| crate::process::working_in(path).is_empty()),
        "{}: the destination conversation is in use; stop its agent and try again",
        label(to)
    );
    Ok(())
}

fn carry_plan(from: &Account, to: &Account, session: &str) -> Result<Carry> {
    if from.agent != "claude" || to.agent != "claude" {
        return Ok(Carry::Nothing);
    }
    valid_session(session)?;
    let (src, dst) = (transcript(from, session)?, transcript(to, session)?);
    let src = src.with_context(|| format!("the Claude transcript for {session} is missing on {}", label(from)))?;
    let source = Revision::read(&src).with_context(|| format!("reading {}", src.display()))?;
    anyhow::ensure!(!source.data.is_empty(), "the Claude transcript for {session} is empty on {}", label(from));
    let Some(dst) = dst else {
        destination_idle(to, None)?;
        let project = src.parent().and_then(|d| d.file_name()).context("an odd transcript path")?;
        let dst = paths::expand_tilde(&to.home).join("projects").join(project).join(src.file_name().unwrap());
        return Ok(Carry::Copy { data: source.data, dst, previous: None });
    };
    let dst = dst.canonicalize()?;
    let previous = Revision::read(&dst).with_context(|| format!("reading {}", dst.display()))?;
    use std::os::unix::fs::MetadataExt;
    if source.metadata.dev() == previous.metadata.dev() && source.metadata.ino() == previous.metadata.ino() {
        if !from.same_home(to) {
            destination_idle(to, None)?;
        }
        return Ok(Carry::There);
    }
    destination_idle(to, Some(&dst))?;
    if previous.data.starts_with(&source.data) {
        return Ok(Carry::There);
    }
    if source.data.starts_with(&previous.data) {
        return Ok(Carry::Copy { data: source.data, dst, previous: Some(Box::new(previous)) });
    }
    bail!(
        "{} and {} each went on with this conversation on their own: continuing on one would lose \
         what was said on the other",
        label(from),
        label(to)
    )
}

// Lock both stores so a reverse transfer, another project or a home alias cannot bypass it.
fn transfer_locks(from: &Account, to: &Account, session: &str) -> Result<Vec<std::fs::File>> {
    use std::os::unix::fs::OpenOptionsExt;
    valid_session(session)?;
    let mut dirs = Vec::new();
    for account in [from, to] {
        let dir = paths::expand_tilde(&account.home).join("projects");
        std::fs::create_dir_all(&dir)?;
        dirs.push(dir.canonicalize()?);
    }
    dirs.sort();
    dirs.dedup();
    dirs.into_iter()
        .map(|dir| {
            let lock = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(dir.join(format!(".kraal-transfer-{session}.lock")))?;
            anyhow::ensure!(lock.metadata()?.is_file(), "the conversation transfer lock is not a regular file");
            anyhow::ensure!(paths::try_lock(&lock)?, "another kraal is transferring this conversation; try again");
            Ok(lock)
        })
        .collect()
}

/// Refuse early, before an agent is stopped for a switch, when its conversation cannot follow
/// to `to` whole.
pub fn check_carry(from: &Account, to: &Account, session: &str) -> Result<()> {
    carry_plan(from, to, session).map(|_| ())
}

/// Give account `to` the newest transcript of conversation `session` of account `from` (call it
/// once `from`'s agent stopped writing it). True when `to` can then resume it; an error when it
/// cannot without losing part of it.
pub fn carry_conversation(from: &Account, to: &Account, session: &str) -> Result<bool> {
    carry_with(from, to, session, |_| {})
}

#[derive(Clone, Copy)]
enum TransferStep {
    Compared,
    Preserved,
}

fn carry_with(from: &Account, to: &Account, session: &str, mut step: impl FnMut(TransferStep)) -> Result<bool> {
    if from.agent != "claude" || to.agent != "claude" {
        return Ok(false);
    }
    let _locks = transfer_locks(from, to, session)?;
    let (data, dst, previous) = match carry_plan(from, to, session)? {
        Carry::Nothing => return Ok(false),
        Carry::There => return Ok(true),
        Carry::Copy { data, dst, previous } => (data, dst, previous),
    };
    step(TransferStep::Compared);
    let dir = dst.parent().context("an odd transcript path")?;
    std::fs::create_dir_all(dir)?;
    use std::os::unix::fs::DirBuilderExt;
    let recovery = dir.join(format!(".kraal-transfer-{session}-{}", crate::identity::new_uuid()));
    std::fs::DirBuilder::new().mode(0o700).create(&recovery)?;
    let incoming = recovery.join("incoming.jsonl");
    let saved = recovery.join("previous.jsonl");
    let result = (|| -> Result<()> {
        paths::write_private(&incoming, &data)?;
        destination_idle(to, Some(&dst))?;
        if let Some(previous) = &previous {
            // Verify the inode actually moved, not just the path before a replacing rename.
            std::fs::rename(&dst, &saved)?;
            paths::sync_dir(&recovery);
            paths::sync_dir(dir);
            step(TransferStep::Preserved);
            anyhow::ensure!(previous.matches(&saved)?, "the destination changed during transfer; try again");
        }
        // link is exclusive on Linux and macOS. A newly appeared name must never be replaced.
        std::fs::hard_link(&incoming, &dst).context("the destination appeared during transfer; try again")?;
        paths::sync_dir(dir);
        Ok(())
    })();
    if let Err(error) = result {
        if saved.exists() {
            let _ = std::fs::hard_link(&saved, &dst);
            paths::sync_dir(dir);
            return Err(error).with_context(|| format!("destination version kept at {}", saved.display()));
        }
        let _ = std::fs::remove_file(&incoming);
        let _ = std::fs::remove_dir(&recovery);
        return Err(error);
    }
    let _ = std::fs::remove_file(incoming);
    if previous.is_none() {
        let _ = std::fs::remove_dir(recovery);
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Write};
    use std::process::{Command, Stdio};

    struct Transfer {
        dir: crate::testing::TempDir,
        from: Account,
        to: Account,
        dst: PathBuf,
    }

    impl Transfer {
        fn new() -> Self {
            let dir = crate::testing::TempDir::new("conversation-transfer");
            dir.mkdir("from/projects/work");
            dir.mkdir("to/projects/work");
            dir.write("from/projects/work/session.jsonl", "first\nsource\n");
            let dst = dir.write("to/projects/work/session.jsonl", "first\n");
            let account = |name| Account::new("claude", name, dir.0.join(name).display().to_string(), None, false);
            let (from, to) = (account("from"), account("to"));
            Self { dir, from, to, dst }
        }

        fn saved(&self) -> PathBuf {
            std::fs::read_dir(self.dst.parent().unwrap())
                .unwrap()
                .map(|e| e.unwrap().path().join("previous.jsonl"))
                .find(|p| p.exists())
                .expect("the replaced inode remains recoverable")
        }
    }

    #[test]
    fn oversized_sparse_transcripts_are_rejected_before_reading() {
        let t = Transfer::new();
        let src = t.dir.0.join("from/projects/work/session.jsonl");
        std::fs::OpenOptions::new().write(true).open(&src).unwrap().set_len(1 << 40).unwrap();
        for to in [&t.from, &t.to] {
            let error = format!("{:#}", check_carry(&t.from, to, "session").unwrap_err());
            assert!(error.contains("too large to transfer safely"), "{error}");
        }
        assert!(carry_conversation(&t.from, &t.to, "session").is_err());
        assert_eq!(std::fs::read_to_string(&t.dst).unwrap(), "first\n");
    }

    #[test]
    fn fifo_transfer_locks_are_refused_without_waiting_for_a_reader() {
        let t = Transfer::new();
        let path = Path::new(&t.to.home).join("projects/.kraal-transfer-session.lock");
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let (tx, rx) = std::sync::mpsc::channel();
        let (from, to) = (t.from.clone(), t.to.clone());
        let worker = std::thread::spawn(move || tx.send(carry_conversation(&from, &to, "session")).unwrap());
        let result = rx.recv_timeout(crate::testing::PATIENCE);
        // Unblock a regressed implementation before failing, so no worker is left behind.
        use std::os::unix::fs::OpenOptionsExt;
        let _reader = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(path).unwrap();
        worker.join().unwrap();
        assert!(result.expect("a FIFO lock must not block").is_err());
        // With a reader, opening succeeds, but the regular-file check must still reject it.
        assert!(carry_conversation(&t.from, &t.to, "session").unwrap_err().to_string().contains("not a regular file"));
        assert_eq!(std::fs::read_to_string(&t.dst).unwrap(), "first\n");
    }

    #[test]
    fn transfers_lock_both_stores_including_aliases_and_reverse_transfers() {
        let t = Transfer::new();
        let held = transfer_locks(&t.from, &t.to, "session").unwrap();
        assert!(carry_conversation(&t.from, &t.to, "session").unwrap_err().to_string().contains("another kraal"));
        assert!(carry_conversation(&t.to, &t.from, "session").is_err());
        std::os::unix::fs::symlink(&t.from.home, t.dir.0.join("alias")).unwrap();
        let alias = Account { home: t.dir.0.join("alias").display().to_string(), ..t.from.clone() };
        assert!(carry_conversation(&alias, &t.to, "session").is_err());
        assert!(transfer_locks(&t.from, &t.to, "other").is_ok());
        drop(held);
        assert!(carry_conversation(&t.from, &t.to, "session").unwrap());
        assert_eq!(std::fs::read_to_string(t.saved()).unwrap(), "first\n");
    }

    #[test]
    fn destination_changes_after_comparison_are_preserved_and_refused() {
        for replace in [false, true] {
            let t = Transfer::new();
            let result = carry_with(&t.from, &t.to, "session", |step| {
                if matches!(step, TransferStep::Compared) {
                    if replace {
                        let new = t.dir.write("replacement", "first\ndestination\n");
                        std::fs::rename(new, &t.dst).unwrap();
                    } else {
                        std::fs::OpenOptions::new()
                            .append(true)
                            .open(&t.dst)
                            .unwrap()
                            .write_all(b"destination\n")
                            .unwrap();
                    }
                }
            });
            let error = format!("{:#}", result.unwrap_err());
            assert!(error.contains("destination changed"), "{error}");
            assert!(error.contains(t.saved().to_str().unwrap()));
            assert_eq!(std::fs::read_to_string(&t.dst).unwrap(), "first\ndestination\n");
            assert_eq!(std::fs::read_to_string(t.saved()).unwrap(), "first\ndestination\n");
        }
    }

    #[test]
    fn a_new_destination_name_is_never_overwritten() {
        for existed in [false, true] {
            let t = Transfer::new();
            if !existed {
                std::fs::remove_file(&t.dst).unwrap();
            }
            let result = carry_with(&t.from, &t.to, "session", |step| {
                if matches!((existed, step), (false, TransferStep::Compared) | (true, TransferStep::Preserved)) {
                    std::fs::write(&t.dst, "concurrent\n").unwrap();
                }
            });
            assert!(result.is_err());
            assert_eq!(std::fs::read_to_string(&t.dst).unwrap(), "concurrent\n");
            if existed {
                assert_eq!(std::fs::read_to_string(t.saved()).unwrap(), "first\n");
            }
        }
    }

    #[test]
    fn a_writer_on_the_replaced_inode_keeps_a_recoverable_name() {
        let t = Transfer::new();
        let mut writer = std::fs::OpenOptions::new().append(true).open(&t.dst).unwrap();
        assert!(carry_conversation(&t.from, &t.to, "session").unwrap());
        writer.write_all(b"late message\n").unwrap();
        assert_eq!(std::fs::read_to_string(t.saved()).unwrap(), "first\nlate message\n");
        assert_eq!(std::fs::read_to_string(&t.dst).unwrap(), "first\nsource\n");
    }

    #[test]
    fn destination_use_is_refused_with_and_without_an_open_transcript() {
        for kind in ["open", "configured", "default", "shared"] {
            let t = Transfer::new();
            let mut command = Command::new("sh");
            command
                .args([
                    "-c",
                    if kind == "open" {
                        "exec 3>>\"$1\"; printf 'ready\\n'; read answer"
                    } else {
                        "printf 'ready\\n'; read answer"
                    },
                    "sh",
                ])
                .arg(&t.dst)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped());
            if kind == "default" {
                let home = t.dir.mkdir("user");
                std::os::unix::fs::symlink(&t.to.home, home.join(".claude")).unwrap();
                let program = t.dir.0.join("claude");
                crate::testing::write_script(&program, "#!/bin/sh\nprintf 'ready\\n'\nread answer\n");
                command = Command::new(program);
                command.env_remove("CLAUDE_CONFIG_DIR").env("HOME", home).stdin(Stdio::piped()).stdout(Stdio::piped());
            } else if kind != "open" {
                command.env("CLAUDE_CONFIG_DIR", &t.to.home);
            }
            if kind == "shared" {
                std::fs::remove_file(&t.dst).unwrap();
                std::fs::hard_link(t.dir.0.join("from/projects/work/session.jsonl"), &t.dst).unwrap();
            }
            let mut child = command.spawn().unwrap();
            let mut ready = String::new();
            std::io::BufReader::new(child.stdout.take().unwrap()).read_line(&mut ready).unwrap();
            assert_eq!(ready, "ready\n");
            let checked = check_carry(&t.from, &t.to, "session");
            let carried = carry_conversation(&t.from, &t.to, "session");
            child.stdin.take().unwrap().write_all(b"done\n").unwrap();
            assert!(child.wait().unwrap().success());
            assert!(checked.unwrap_err().to_string().contains("in use"));
            assert!(carried.unwrap_err().to_string().contains("in use"));
            assert_eq!(
                std::fs::read_to_string(&t.dst).unwrap(),
                if kind == "shared" { "first\nsource\n" } else { "first\n" }
            );
        }
    }
}
