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

/// Whether file `short` holds the start of file `long`.
fn is_prefix(short: &Path, long: &Path) -> std::io::Result<bool> {
    use std::io::Read;
    let (mut a, mut b) = (std::fs::File::open(short)?, std::fs::File::open(long)?);
    if a.metadata()?.len() > b.metadata()?.len() {
        return Ok(false);
    }
    let (mut x, mut y) = (vec![0u8; 64 << 10], vec![0u8; 64 << 10]);
    loop {
        let n = a.read(&mut x)?;
        if n == 0 {
            return Ok(true);
        }
        b.read_exact(&mut y[..n])?;
        if x[..n] != y[..n] {
            return Ok(false);
        }
    }
}

/// What continuing conversation `session` on account `to` needs (Claude: accounts that keep
/// their conversations apart each have a copy of the transcript, which only ever grows).
enum Carry {
    /// This agent cannot move a conversation between accounts.
    Nothing,
    /// `to` sees the newest transcript already (the same file, or a copy holding all of it).
    There,
    /// `to` needs the newest transcript, `from`'s, at that place.
    Copy(PathBuf, PathBuf),
}

fn carry_plan(from: &Account, to: &Account, session: &str) -> Result<Carry> {
    if from.agent != "claude" || to.agent != "claude" {
        return Ok(Carry::Nothing);
    }
    anyhow::ensure!(
        !session.is_empty() && session.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'),
        "could not verify the Claude conversation id"
    );
    let (src, dst) = (transcript(from, session)?, transcript(to, session)?);
    let src = src.with_context(|| format!("the Claude transcript for {session} is missing on {}", label(from)))?;
    // Even a shared file must be readable before stopping the process that owns it.
    let readable = |path: &Path| -> Result<u64> {
        let mut file = std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
        std::io::copy(&mut file, &mut std::io::sink()).with_context(|| format!("reading {}", path.display()))
    };
    anyhow::ensure!(readable(&src)? > 0, "the Claude transcript for {session} is empty on {}", label(from));
    let Some(dst) = dst else {
        let project = src.parent().and_then(|d| d.file_name()).context("an odd transcript path")?;
        let dst = paths::expand_tilde(&to.home).join("projects").join(project).join(src.file_name().unwrap());
        return Ok(Carry::Copy(src, dst));
    };
    readable(&dst)?;
    let same = match (std::fs::canonicalize(&src), std::fs::canonicalize(&dst)) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    };
    if same || is_prefix(&src, &dst)? {
        return Ok(Carry::There);
    }
    if is_prefix(&dst, &src)? {
        return Ok(Carry::Copy(src, dst));
    }
    bail!(
        "{} and {} each went on with this conversation on their own: continuing on one would lose \
         what was said on the other",
        label(from),
        label(to)
    )
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
    let (src, dst) = match carry_plan(from, to, session)? {
        Carry::Nothing => return Ok(false),
        Carry::There => return Ok(true),
        Carry::Copy(src, dst) => (src, dst),
    };
    let dir = dst.parent().context("an odd transcript path")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let data = std::fs::read(&src).with_context(|| format!("reading {}", src.display()))?;
    paths::write_private(&dst, &data).with_context(|| format!("copying the conversation to {}", dst.display()))?;
    Ok(true)
}
