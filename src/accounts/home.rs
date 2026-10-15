use std::path::PathBuf;

use anyhow::{Context, Result};

use super::{Account, agent_kind, label};
use crate::paths;

/// `home` as an account keeps it: a relative path means where the command was typed, and the
/// server, which runs elsewhere, must find the same directory. `~/…` stays as written.
pub fn absolute_home(home: &str) -> Result<String> {
    if !paths::expand_tilde(home).is_relative() {
        return Ok(home.to_string());
    }
    Ok(std::path::absolute(home).with_context(|| format!("locating {home}"))?.display().to_string())
}

/// Where a new account's home goes by default.
pub fn default_home_for(agent: &str, name: &str) -> PathBuf {
    paths::data_home().join("kraal/accounts").join(format!("{agent}-{name}"))
}

/// Create an account home: shared entries are symlinked from the agent's default home, a few
/// files are copied. Credentials are never touched; the agent's login creates them.
pub fn prepare_home(a: &Account) -> Result<Vec<String>> {
    use std::os::unix::fs::PermissionsExt;
    let kind = agent_kind(&a.agent).context("unknown agent")?;
    let home = paths::expand_tilde(&a.home);
    let default = paths::expand_tilde(kind.default_home);
    let mut notes = Vec::new();
    if home == default {
        return Ok(notes);
    }
    std::fs::create_dir_all(&home).with_context(|| format!("creating {}", home.display()))?;
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("making {} private", home.display()))?;
    for entry in kind.shared {
        let src = default.join(entry);
        let dst = home.join(entry);
        // A shared directory not created yet (no conversation so far) is created now: linked
        // later, it would be too late (the account would have started its own).
        if !entry.contains('.') && !src.exists() {
            std::fs::create_dir_all(&src).with_context(|| format!("creating {}", src.display()))?;
        }
        if !src.exists() || dst.symlink_metadata().is_ok() {
            continue;
        }
        std::os::unix::fs::symlink(&src, &dst).with_context(|| format!("linking {}", dst.display()))?;
        notes.push(format!("shared {entry}"));
    }
    for entry in kind.copied {
        let src = default.join(entry);
        let dst = home.join(entry);
        if src.is_file() && !dst.exists() {
            std::fs::copy(&src, &dst).with_context(|| format!("copying {}", src.display()))?;
            notes.push(format!("copied {entry}"));
        }
    }
    Ok(notes)
}

/// The lock lives in the home so aliases and other kraal sessions share it.
pub fn lock_login(a: &Account) -> Result<std::sync::Arc<std::fs::File>> {
    use std::os::unix::fs::OpenOptionsExt;
    let home = paths::expand_tilde(&a.home);
    std::fs::create_dir_all(&home)?;
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(home.join(format!(".kraal-{}-login.lock", a.agent)))?;
    anyhow::ensure!(paths::try_lock(&lock)?, "{}: sign-in already running", label(a));
    Ok(std::sync::Arc::new(lock))
}
