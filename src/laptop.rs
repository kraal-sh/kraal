//! Files served by the attached laptop, within its own configured scope.

use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::files::Entry;

pub mod copy;
pub mod stage;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Identity {
    pub id: String,
    pub host: String,
    pub os: String,
}

impl Identity {
    pub fn new() -> Result<Self> {
        let mut bytes = [0u8; 16];
        File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        Ok(Self {
            id: bytes.iter().map(|b| format!("{b:02x}")).collect(),
            host: crate::context::hostname().unwrap_or_else(|| "laptop".into()).split('.').next().unwrap().into(),
            os: std::env::consts::OS.into(),
        })
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    #[default]
    Home,
    All,
    Off,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub scope: Scope,
}

pub type Listing = Result<(String, Vec<Entry>), String>;

fn excluded(name: &str, macos: bool) -> bool {
    name.starts_with('.') || (macos && name.eq_ignore_ascii_case("Library"))
}

fn directory(parent: i32, name: &CStr) -> io::Result<OwnedFd> {
    // SAFETY: name is NUL terminated; the returned descriptor is owned exactly once.
    let fd = unsafe {
        libc::openat(parent, name.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
    };
    if fd < 0 { Err(io::Error::last_os_error()) } else { Ok(unsafe { OwnedFd::from_raw_fd(fd) }) }
}

fn open_directory(path: &Path) -> Result<OwnedFd> {
    let mut fd = directory(libc::AT_FDCWD, c"/")?;
    for part in path.components() {
        match part {
            Component::RootDir | Component::CurDir => {}
            Component::Normal(name) => {
                let name = CString::new(name.as_bytes())?;
                fd = directory(fd.as_raw_fd(), &name).context("cannot open directory without following links")?;
            }
            _ => bail!("parent components are refused: choose a directory inside the laptop scope"),
        }
    }
    Ok(fd)
}

struct Directory(*mut libc::DIR);
impl Drop for Directory {
    fn drop(&mut self) {
        // SAFETY: fdopendir gave this object sole ownership of the stream.
        unsafe { libc::closedir(self.0) };
    }
}

#[cfg(target_os = "linux")]
fn errno() -> *mut libc::c_int {
    // SAFETY: libc returns the current thread's errno storage.
    unsafe { libc::__errno_location() }
}

#[cfg(target_os = "macos")]
fn errno() -> *mut libc::c_int {
    // SAFETY: libc returns the current thread's errno storage.
    unsafe { libc::__error() }
}

fn scoped_path(path: &str, home: &Path, scope: Scope, macos: bool) -> Result<(PathBuf, PathBuf)> {
    ensure!(scope != Scope::Off, "Laptop files are off: enable them in config.toml on the laptop");
    let home = home.canonicalize().context("cannot resolve the laptop home directory")?;
    let requested = Path::new(path);
    let path = if let Ok(rest) = requested.strip_prefix("~") { home.join(rest) } else { PathBuf::from(path) };
    ensure!(path.is_absolute(), "choose an absolute directory inside the laptop scope");
    ensure!(
        !path.components().any(|c| matches!(c, Component::ParentDir)),
        "parent components are refused: choose a directory inside the laptop scope"
    );
    if scope == Scope::Home {
        let rest = path.strip_prefix(&home).context("outside the laptop home: choose a directory inside it")?;
        if let Some(Component::Normal(first)) = rest.components().next() {
            ensure!(
                !excluded(&first.to_string_lossy(), macos),
                "this top-level directory is excluded: choose another directory"
            );
        }
    }
    Ok((home, path))
}

pub fn listing_case_sensitive(path: &str, home: &Path, scope: Scope) -> Result<bool> {
    let (_, path) = scoped_path(path, home, scope, cfg!(target_os = "macos"))?;
    stage::case_sensitive(&open_directory(&path)?)
}

pub fn list(path: &str, home: &Path, scope: Scope, macos: bool) -> Result<(String, Vec<Entry>)> {
    list_directory(path, home, scope, macos, false)
}

fn list_directory(path: &str, home: &Path, scope: Scope, macos: bool, strict: bool) -> Result<(String, Vec<Entry>)> {
    ensure!(scope != Scope::Off, "Laptop files are off: change laptop_files.scope on the laptop to enable listing");
    ensure!(!path.is_empty(), "choose a directory inside the laptop scope");
    let (home, path) = scoped_path(path, home, scope, macos)?;
    let fd = open_directory(&path)?;
    // SAFETY: fd is a live directory descriptor. Ownership passes only on success.
    let raw = unsafe { libc::fdopendir(fd.as_raw_fd()) };
    ensure!(!raw.is_null(), "cannot read directory: {}", io::Error::last_os_error());
    let _ = fd.into_raw_fd();
    let directory = Directory(raw);
    // SAFETY: the stream remains live until the end of this function.
    let dirfd = unsafe { libc::dirfd(directory.0) };
    let mut entries = Vec::new();
    let mut bytes = 0usize;
    loop {
        // SAFETY: errno is thread local; readdir's entry lives until the next call.
        let entry = unsafe {
            *errno() = 0;
            libc::readdir(directory.0)
        };
        if entry.is_null() {
            ensure!(unsafe { *errno() } == 0, "cannot finish listing: {}", io::Error::last_os_error());
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let text = name.to_string_lossy();
        if scope == Scope::Home && path == home && excluded(&text, macos) {
            continue;
        }
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: name belongs to this directory stream and stat has space for libc::stat.
        let ok = unsafe { libc::fstatat(dirfd, name.as_ptr(), stat.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
        if ok != 0 {
            ensure!(!strict, "cannot inspect source item: {}", io::Error::last_os_error());
            continue;
        }
        let stat = unsafe { stat.assume_init() };
        let kind = stat.st_mode & libc::S_IFMT;
        if ![libc::S_IFDIR, libc::S_IFREG, libc::S_IFLNK].contains(&kind) {
            continue;
        }
        // JSON can escape each byte into six bytes; leave room for metadata too.
        bytes = bytes.saturating_add(name.to_bytes().len().saturating_mul(6) + 256);
        ensure!(bytes <= 16 << 20 && entries.len() < 100_000, "too many entries: open this directory on the laptop");
        entries.push(Entry {
            name: text.into_owned(),
            is_dir: kind == libc::S_IFDIR,
            is_link: kind == libc::S_IFLNK,
            size: stat.st_size.max(0) as u64,
            mtime: stat.st_mtime,
            exact: name.to_str().is_ok(),
        });
    }
    let shown = if scope == Scope::Home {
        let rest = path.strip_prefix(&home)?;
        if rest.as_os_str().is_empty() {
            "~".into()
        } else {
            format!("~/{}", rest.to_str().context("directory path is not UTF-8")?)
        }
    } else {
        path.to_str().context("directory path is not UTF-8")?.into()
    };
    Ok((shown, entries))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;
    use std::os::unix::fs::symlink;

    #[test]
    fn home_excludes_top_level_secrets_but_keeps_nested_hidden_files() {
        let home = TempDir::new("laptop-home");
        home.mkdir("Documents");
        home.mkdir(".ssh");
        home.mkdir("Library");
        assert!(excluded("lIbRaRy", true));
        assert!(!excluded("Library", false));
        home.write(".secret", "secret");
        home.write("Documents/.nested", "nested");
        for macos in [false, true] {
            let (path, entries) = list("~", &home.0, Scope::Home, macos).unwrap();
            assert_eq!(path, "~");
            assert!(entries.iter().any(|e| e.name == "Documents" && e.is_dir));
            assert!(!entries.iter().any(|e| e.name.starts_with('.')));
            assert_eq!(entries.iter().any(|e| e.name == "Library"), !macos);
            assert!(list("~/.ssh", &home.0, Scope::Home, macos).is_err());
            assert_eq!(list("~/Library", &home.0, Scope::Home, macos).is_err(), macos);
            assert_eq!(list("~/Documents", &home.0, Scope::Home, macos).unwrap().1[0].name, ".nested");
        }
    }

    #[test]
    fn scopes_and_component_walk_refuse_escape_and_links() {
        let home = TempDir::new("laptop-scope");
        let outside = TempDir::new("laptop-outside");
        outside.write("private", "not listed");
        home.mkdir("inside");
        symlink(&outside.0, home.0.join("escape")).unwrap();
        symlink(home.0.join("inside"), home.0.join("internal")).unwrap();
        assert!(list(&outside.text(), &home.0, Scope::Home, false).is_err());
        for path in
            ["~/escape", "~/escape/child", "~/internal", "~/../", "~/inside/../../", "relative", "~other", "~/a\0b"]
        {
            assert!(list(path, &home.0, Scope::Home, false).is_err(), "{path}");
        }
        let entries = list("~", &home.0, Scope::Home, false).unwrap().1;
        assert!(entries.iter().any(|e| e.name == "escape" && e.is_link && !e.is_dir));
        assert!(list("~", &home.0, Scope::Off, false).is_err());
        assert_eq!(list(&outside.text(), &home.0, Scope::All, false).unwrap().1[0].name, "private");
        assert!(list("~/escape", &home.0, Scope::All, false).is_err());
    }

    #[test]
    fn special_files_are_skipped() {
        let home = TempDir::new("laptop-entries");
        let fifo = CString::new(home.0.join("fifo").as_os_str().as_bytes()).unwrap();
        // SAFETY: the test owns this directory and passes a NUL terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        home.write("file", b"data");
        let entries = list("~", &home.0, Scope::Home, false).unwrap().1;
        assert_eq!(entries.len(), 1);
        assert!(entries[0].exact);
        assert_eq!(entries[0].size, 4);
    }

    // APFS refuses file names that are not UTF-8.
    #[cfg(target_os = "linux")]
    #[test]
    fn non_utf8_names_stay_inexact() {
        let home = TempDir::new("laptop-non-utf8");
        std::fs::write(home.0.join(std::ffi::OsStr::from_bytes(b"bad\xff")), b"data").unwrap();
        let entries = list("~", &home.0, Scope::Home, false).unwrap().1;
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].exact);
    }

    #[test]
    fn configuration_defaults_to_home_and_rejects_unknown_scopes() {
        assert_eq!(crate::config::Config::default().laptop_files.scope, Scope::Home);
        for (name, scope) in [("home", Scope::Home), ("all", Scope::All), ("off", Scope::Off)] {
            let cfg = crate::config::Config::from_user_toml(&format!("[laptop_files]\nscope = {name:?}")).unwrap();
            assert_eq!(cfg.laptop_files.scope, scope);
        }
        assert!(crate::config::Config::from_user_toml("[laptop_files]\nscope = 'outside'").is_err());
    }

    #[test]
    fn each_client_process_has_a_new_identity() {
        let first = Identity::new().unwrap();
        assert_ne!(first.id, Identity::new().unwrap().id);
        assert!(crate::attachments::token_valid(&first.id));
        assert!(!first.host.contains('.'));
        assert_eq!(first.os, std::env::consts::OS);
    }
    #[test]
    fn oversized_listings_fail_without_returning_a_partial_directory() {
        let home = TempDir::new("laptop-large-list");
        for index in 0..10_000 {
            home.write(&format!("{index:05}{}", "a".repeat(245)), b"");
        }
        let error = list("~", &home.0, Scope::Home, false).unwrap_err();
        assert!(error.to_string().contains("too many entries"));
    }
}
