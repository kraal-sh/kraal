//! Filesystem locations: sockets, persisted state, config and logs.

use std::path::{Path, PathBuf};

/// Directory holding one control socket per running session.
pub fn runtime_dir() -> PathBuf {
    let fallback = || std::env::temp_dir().join(format!("kraal-{}", unsafe { libc::getuid() }));
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|p| p.is_dir()).unwrap_or_else(fallback);
    // Unix socket paths are limited to about 100 bytes; leave room for a session name.
    let base = if base.as_os_str().len() > 60 { fallback() } else { base };
    base.join("kraal")
}

/// Create `dir` (and missing parents) and make sure it is a real directory owned by us and
/// private (0700). Refuses symlinks and directories owned by someone else: a socket in such a
/// place could be read or replaced by another user.
pub fn ensure_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let meta = std::fs::symlink_metadata(dir)?;
        let uid = unsafe { libc::getuid() };
        if !meta.is_dir() || meta.uid() != uid {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!("{} is not a directory owned by you; refusing to use it", dir.display()),
            ));
        }
        if meta.mode() & 0o077 != 0 {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    Ok(())
}

/// The runtime directory, checked (and its fallback parent in /tmp, which other users can write).
pub fn private_runtime_dir() -> std::io::Result<PathBuf> {
    let dir = runtime_dir();
    if let Some(parent) = dir.parent()
        && parent.starts_with(std::env::temp_dir())
    {
        ensure_private_dir(parent)?;
    }
    ensure_private_dir(&dir)?;
    Ok(dir)
}

/// Write a file only we can read (state can hold paths and commands).
pub fn write_private(path: &std::path::Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let mut f = opts.open(path)?;
    f.write_all(data)?;
    f.sync_all()
}

pub fn socket_path(session: &str) -> PathBuf {
    runtime_dir().join(format!("{session}.sock"))
}

/// ControlPath for shared SSH connections. Unix socket paths are limited to ~104 bytes and ssh
/// expands `%C` to 40 characters, so fall back to a short directory when the runtime dir is long.
pub fn ssh_control_path() -> Option<PathBuf> {
    let mut dir = runtime_dir();
    if dir.as_os_str().len() > 50 {
        dir = PathBuf::from(format!("/tmp/kraal-{}", unsafe { libc::getuid() }));
    }
    std::fs::create_dir_all(&dir).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // Never put sockets in a directory someone else controls.
        let meta = std::fs::symlink_metadata(&dir).ok()?;
        if !meta.is_dir() || meta.uid() != unsafe { libc::getuid() } {
            return None;
        }
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    Some(dir.join("ssh-%C"))
}

/// `~/.local/state/kraal`
pub fn state_dir() -> PathBuf {
    dirs::state_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/state")))
        .unwrap_or_else(std::env::temp_dir)
        .join("kraal")
}

pub fn session_state_path(session: &str) -> PathBuf {
    state_dir().join("sessions").join(format!("{session}.json"))
}

pub fn log_path(session: &str) -> PathBuf {
    state_dir().join(format!("server-{session}.log"))
}

/// `~/.config/kraal`
pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))
        .unwrap_or_else(std::env::temp_dir)
        .join("kraal")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn workspaces_dir() -> PathBuf {
    config_dir().join("workspaces")
}

/// Expand a leading `~` to the home directory.
pub fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from(path));
    }
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

/// Replace the home directory prefix with `~` for display.
pub fn shorten_home(path: &Path) -> String {
    if let Some(home) = dirs::home_dir()
        && let Ok(rest) = path.strip_prefix(&home)
    {
        if rest.as_os_str().is_empty() {
            return "~".into();
        }
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

/// Session names end up in file names; keep them tame.
pub fn valid_session_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.starts_with('.')
}
