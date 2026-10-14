//! Bounded attachment batches, published only when every declared byte arrived.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

pub const IMAGE_BYTES: usize = 16 << 20;
pub const CHUNK: usize = 64 * 1024;
pub const RESULT_RESERVE: u64 = 4096;
pub const TIMEOUT: Duration = Duration::from_secs(60);
const RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub file_bytes: u64,
    pub batch_bytes: u64,
    pub files: usize,
    pub session_bytes: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self { file_bytes: 64 << 20, batch_bytes: 128 << 20, files: 32, session_bytes: 1 << 30 }
    }
}

impl Settings {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.file_bytes > 0 && self.batch_bytes > 0 && self.session_bytes > 0,
            "attachments byte limits must be positive"
        );
        ensure!((1..=1024).contains(&self.files), "attachments.files must be between 1 and 1024");
        Ok(())
    }

    pub fn check(&self, files: &[FileSpec]) -> Result<u64> {
        ensure!(
            !files.is_empty() && files.len() <= self.files,
            "too many files; use kraal cp or raise attachments.files"
        );
        let mut total = 0u64;
        for file in files {
            ensure!(file.size <= self.file_bytes, "file too large; use kraal cp or raise attachments.file_bytes");
            total = total.checked_add(file.size).context("batch size overflow")?;
        }
        ensure!(total <= self.batch_bytes, "batch too large; use kraal cp or raise attachments.batch_bytes");
        Ok(total)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSpec {
    pub name: String,
    pub size: u64,
}

pub fn directory(session: &str) -> PathBuf {
    crate::paths::state_dir().join("attachments-v1").join(session)
}

pub fn remove(session: &str) -> std::io::Result<bool> {
    match fs::remove_dir_all(directory(session)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

pub fn token_valid(token: &str) -> bool {
    !token.is_empty() && token.len() <= 80 && token.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
}

fn safe_name(name: &str, index: usize) -> String {
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.len() > 200
        || name.contains('/')
        || name.chars().any(char::is_control)
    {
        format!("attachment-{}", index + 1)
    } else {
        name.to_string()
    }
}

pub struct Source {
    pub path: PathBuf,
    pub spec: FileSpec,
    file: File,
}

impl Source {
    pub fn open(path: PathBuf) -> Result<Self> {
        // O_NONBLOCK also keeps a FIFO swapped in before open from hanging the client.
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        let meta = file.metadata()?;
        ensure!(meta.is_file(), "only regular files can be attached; use kraal cp for directories");
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("").to_string();
        Ok(Self { path, spec: FileSpec { name, size: meta.len() }, file })
    }

    pub fn chunk(&mut self, offset: u64) -> Result<Vec<u8>> {
        ensure!(self.file.metadata()?.len() == self.spec.size, "file size changed; attach it again");
        let mut bytes = vec![0; (self.spec.size.saturating_sub(offset)).min(CHUNK as u64) as usize];
        self.file.read_exact(&mut bytes).context("file changed while reading; attach it again")?;
        Ok(bytes)
    }
}

pub struct Batch {
    temp: PathBuf,
    final_dir: PathBuf,
    files: Vec<(File, PathBuf, u64, u64)>,
    pub total: u64,
    pub received: u64,
    published: bool,
}

impl Batch {
    pub fn start(root: &Path, token: &str, specs: &[FileSpec], settings: &Settings, reserved: u64) -> Result<Self> {
        ensure!(token_valid(token), "invalid attachment token");
        let total = settings.check(specs)?;
        crate::paths::ensure_private_dir(root.parent().context("missing attachment parent")?)?;
        crate::paths::ensure_private_dir(root)?;
        let mut used = reserved;
        let mut batches = 0;
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() && !entry.file_name().to_string_lossy().starts_with('.') {
                batches += 1;
                used = used.saturating_add(tree_size(&entry.path())?);
            }
        }
        ensure!(batches < 4096, "attachment storage has too many batches; use Purge attachments");
        ensure!(
            used.checked_add(total)
                .and_then(|n| n.checked_add(RESULT_RESERVE))
                .is_some_and(|n| n <= settings.session_bytes),
            "session attachment quota reached; use Purge attachments or raise attachments.session_bytes"
        );
        let temp = root.join(format!(".{token}"));
        let final_dir = root.join(token);
        ensure!(!final_dir.exists(), "attachment already completed; check its result");
        fs::DirBuilder::new().mode(0o700).create(&temp)?;
        let mut batch = Self { temp, final_dir, files: Vec::new(), total, received: 0, published: false };
        crate::paths::ensure_private_dir(&batch.temp)?;
        for (i, spec) in specs.iter().enumerate() {
            let sub = batch.temp.join(i.to_string());
            crate::paths::ensure_private_dir(&sub)?;
            let relative = PathBuf::from(i.to_string()).join(safe_name(&spec.name, i));
            let file = OpenOptions::new().write(true).create_new(true).mode(0o600).open(batch.temp.join(&relative))?;
            batch.files.push((file, relative, spec.size, 0));
        }
        Ok(batch)
    }

    pub fn chunk(&mut self, file: usize, offset: u64, data: &[u8]) -> Result<()> {
        ensure!(!data.is_empty() && data.len() <= CHUNK, "invalid attachment chunk size");
        let (out, _, size, received) = self.files.get_mut(file).context("unknown attachment file")?;
        ensure!(
            offset == *received && data.len() as u64 <= size.saturating_sub(*received),
            "attachment chunk is out of order or exceeds its declared size"
        );
        out.write_all(data)?;
        *received += data.len() as u64;
        self.received += data.len() as u64;
        Ok(())
    }

    pub fn finish(mut self) -> Result<String> {
        ensure!(self.received == self.total, "attachment batch is incomplete");
        for (file, path, size, received) in &self.files {
            ensure!(size == received, "attachment file is incomplete");
            file.sync_all()?;
            File::open(self.temp.join(path).parent().unwrap())?.sync_all()?;
        }
        let paths = self
            .files
            .iter()
            .map(|(_, path, _, _)| {
                let path = self.final_dir.join(path);
                let text = path.to_str().context("attachment storage path is not UTF-8")?;
                ensure!(!text.chars().any(char::is_control), "attachment storage path contains control characters");
                Ok(crate::shell::quote(text))
            })
            .collect::<Result<Vec<_>>>()?
            .join(" ");
        // Record uncertainty before publication: a crash must never cause an automatic second paste.
        crate::paths::write_private(
            &self.temp.join("result.json"),
            &serde_json::to_vec(&serde_json::json!({
                "message": "Files kept; paste outcome unavailable. Inspect the pane before attaching again.",
                "kind": crate::theme::NoticeKind::Warning,
            }))?,
        )?;
        File::open(&self.temp)?.sync_all()?;
        fs::rename(&self.temp, &self.final_dir)?;
        File::open(self.final_dir.parent().unwrap())?.sync_all()?;
        self.published = true;
        Ok(paths)
    }
}

impl Drop for Batch {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_dir_all(&self.temp);
        }
    }
}

fn tree_size(path: &Path) -> std::io::Result<u64> {
    let mut size = 0u64;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        if ty.is_dir() {
            size = size.saturating_add(tree_size(&entry.path())?);
        } else if ty.is_file() {
            size = size.saturating_add(entry.metadata()?.len());
        }
    }
    Ok(size)
}

pub fn result(session: &str, token: &str) -> Result<(String, crate::theme::NoticeKind)> {
    ensure!(token_valid(token), "invalid attachment token");
    let path = directory(session).join(token).join("result.json");
    let file = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
    let mut bytes = Vec::new();
    file.take(4096).read_to_end(&mut bytes)?;
    read_result(&bytes)
}

fn read_result(bytes: &[u8]) -> Result<(String, crate::theme::NoticeKind)> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Stored {
        Legacy(String),
        Current { message: String, kind: crate::theme::NoticeKind },
    }
    Ok(match serde_json::from_slice(bytes)? {
        Stored::Legacy(message) => (message, crate::theme::NoticeKind::Notice),
        Stored::Current { message, kind } => (message, kind),
    })
}

pub fn record(session: &str, token: &str, message: &str, kind: crate::theme::NoticeKind) -> Result<()> {
    ensure!(token_valid(token), "invalid attachment token");
    let bytes = serde_json::to_vec(&serde_json::json!({ "message": message, "kind": kind }))?;
    ensure!(bytes.len() as u64 <= RESULT_RESERVE, "attachment result is too large");
    crate::paths::write_private(&directory(session).join(token).join("result.json"), &bytes)?;
    Ok(())
}

/// Called while holding this session's lock. Other sessions are inspected only under their lock.
pub fn cleanup(session: &str) -> Result<()> {
    let root = directory(session);
    if fs::symlink_metadata(&root).is_ok() {
        crate::paths::ensure_private_dir(root.parent().unwrap())?;
        crate::paths::ensure_private_dir(&root)?;
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() && entry.file_name().to_string_lossy().starts_with('.') {
                fs::remove_dir_all(entry.path())?;
            }
        }
    }
    let Some(parent) = root.parent().filter(|p| p.is_dir()) else { return Ok(()) };
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == session || !crate::paths::valid_session_name(&name) || !entry.file_type()?.is_dir() {
            continue;
        }
        let age = SystemTime::now().duration_since(entry.metadata()?.modified()?).unwrap_or_default();
        if age < RETENTION {
            continue;
        }
        let Some(_lock) = crate::paths::SessionLock::acquire(&name)? else { continue };
        if crate::paths::session_state_path(&name).exists()
            || crate::paths::legacy_session_state_path(&name).exists()
            || (crate::paths::socket_path(&name).exists()
                && !crate::paths::nobody_listens(&crate::paths::socket_path(&name)))
        {
            continue;
        }
        fs::remove_dir_all(entry.path())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};
    use std::sync::atomic::{AtomicU64, Ordering};

    fn root() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir()
            .join(format!("kraal-attachment-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)))
            .join("session")
    }

    fn spec(name: &str, size: u64) -> FileSpec {
        FileSpec { name: name.into(), size }
    }

    #[test]
    fn result_records_keep_kinds_and_read_older_strings() {
        use crate::theme::NoticeKind;

        assert_eq!(read_result(br#""Files received.""#).unwrap(), ("Files received.".into(), NoticeKind::Notice));
        for kind in [NoticeKind::Notice, NoticeKind::Warning, NoticeKind::Error] {
            let bytes = serde_json::to_vec(&serde_json::json!({"message": "unchanged text", "kind": kind})).unwrap();
            assert_eq!(read_result(&bytes).unwrap(), ("unchanged text".into(), kind));
        }
        assert!(read_result(br#"{"kind":"notice"}"#).is_err());
    }

    #[test]
    fn batch_is_private_atomic_and_keeps_duplicate_names_and_safe_aliases() {
        let root = root();
        let specs = [
            spec("notes 'client'.txt", 3),
            spec("notes 'client'.txt", 0),
            spec("../../outside", 1),
            spec("bad\nname", 0),
        ];
        let mut batch = Batch::start(&root, "abc", &specs, &Settings::default(), 0).unwrap();
        assert!(!root.join("abc").exists());
        assert_eq!(fs::metadata(root.join(".abc")).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(fs::metadata(root.join(".abc/0/notes 'client'.txt")).unwrap().permissions().mode() & 0o777, 0o600);
        batch.chunk(0, 0, b"abc").unwrap();
        batch.chunk(2, 0, b"d").unwrap();
        let paths = batch.finish().unwrap();
        let result = read_result(&fs::read(root.join("abc/result.json")).unwrap()).unwrap();
        assert_eq!(result.1, crate::theme::NoticeKind::Warning);
        assert!(paths.contains("'\\''client'\\''"));
        assert!(paths.contains("attachment-3"));
        assert!(!root.join(".abc").exists());
        assert_eq!(fs::read(root.join("abc/0/notes 'client'.txt")).unwrap(), b"abc");
        assert_eq!(fs::read(root.join("abc/1/notes 'client'.txt")).unwrap(), b"");
        assert_eq!(fs::read(root.join("abc/2/attachment-3")).unwrap(), b"d");
        assert!(Batch::start(&root, "abc", &specs, &Settings::default(), 0).is_err());
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[test]
    fn quotas_include_reservations_and_completed_bytes_and_all_limits_are_configurable() {
        let settings = Settings { file_bytes: 3, batch_bytes: 4, files: 2, session_bytes: RESULT_RESERVE + 5 };
        assert!(settings.check(&[]).is_err());
        assert!(settings.check(&[spec("a", 4)]).is_err());
        assert!(settings.check(&[spec("a", 3), spec("b", 2)]).is_err());
        assert!(settings.check(&[spec("a", 0), spec("b", 0), spec("c", 0)]).is_err());
        let root = root();
        assert!(Batch::start(&root, "ab", &[spec("a", 3)], &settings, 3).is_err());
        let mut batch = Batch::start(&root, "ab", &[spec("a", 3)], &settings, 2).unwrap();
        batch.chunk(0, 0, b"abc").unwrap();
        batch.finish().unwrap();
        assert!(Batch::start(&root, "cd", &[spec("a", 1)], &settings, 0).is_err());
        let raised = Settings { session_bytes: RESULT_RESERVE * 2, ..settings };
        assert!(Batch::start(&root, "cd", &[spec("a", 1)], &raised, 0).is_ok());
        assert!(Settings { files: 0, ..Settings::default() }.validate().is_err());
        assert!(Settings { file_bytes: 0, ..Settings::default() }.validate().is_err());
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[test]
    fn bad_chunks_incomplete_finish_and_drop_leave_no_published_files() {
        let root = root();
        let mut batch = Batch::start(&root, "ab", &[spec("a", 3), spec("b", 2)], &Settings::default(), 0).unwrap();
        for (file, offset, bytes) in [(2, 0, b"x".as_slice()), (0, 1, b"x"), (0, 0, b"abcd"), (0, 0, b"")] {
            assert!(batch.chunk(file, offset, bytes).is_err());
        }
        assert!(batch.chunk(0, 0, &vec![0; CHUNK + 1]).is_err());
        batch.chunk(0, 0, b"abc").unwrap();
        assert!(batch.chunk(0, 0, b"a").is_err());
        assert!(batch.finish().is_err());
        assert!(!root.join("ab").exists());
        assert!(!root.join(".ab").exists());
        let batch = Batch::start(&root, "ab", &[spec("a", 3)], &Settings::default(), 0).unwrap();
        drop(batch);
        assert!(!root.join(".ab").exists());
        assert!(Batch::start(&root, "../escape", &[spec("a", 0)], &Settings::default(), 0).is_err());
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[test]
    fn source_refuses_symlinks_directories_special_files_and_size_changes() {
        let root = root();
        fs::create_dir_all(&root).unwrap();
        let path = root.join("source");
        fs::write(&path, b"abc").unwrap();
        symlink(&path, root.join("link")).unwrap();
        assert!(Source::open(root.join("link")).is_err());
        assert!(Source::open(root.clone()).is_err());
        let fifo = std::ffi::CString::new(root.join("fifo").to_str().unwrap()).unwrap();
        // SAFETY: a valid nul-terminated path in this test's directory.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(Source::open(root.join("fifo")).is_err());
        let mut source = Source::open(path.clone()).unwrap();
        assert_eq!(source.chunk(0).unwrap(), b"abc");
        fs::write(path, b"ab").unwrap();
        assert!(source.chunk(3).is_err());
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[test]
    fn cleanup_retains_saved_and_locked_sessions_and_removes_only_old_orphans() {
        if !crate::testing::in_own_process(
            "attachments::tests::cleanup_retains_saved_and_locked_sessions_and_removes_only_old_orphans",
        ) {
            return;
        }
        for session in ["current", "saved", "locked", "old", "fresh"] {
            let path = directory(session);
            fs::create_dir_all(path.join(".partial")).unwrap();
            fs::create_dir_all(path.join("ab")).unwrap();
            let time = if session == "fresh" {
                SystemTime::now()
            } else {
                SystemTime::now() - RETENTION - Duration::from_secs(1)
            };
            File::open(path).unwrap().set_times(fs::FileTimes::new().set_modified(time)).unwrap();
        }
        fs::create_dir_all(crate::paths::session_state_path("saved").parent().unwrap()).unwrap();
        crate::paths::write_private(&crate::paths::session_state_path("saved"), b"{}").unwrap();
        let _held = crate::paths::SessionLock::acquire("locked").unwrap().unwrap();
        cleanup("current").unwrap();
        assert!(!directory("current").join(".partial").exists());
        assert!(directory("current").join("ab").exists());
        assert!(!directory("old").exists());
        for session in ["saved", "locked", "fresh"] {
            assert!(directory(session).exists());
        }
        assert!(remove("current").unwrap());
        assert!(!remove("current").unwrap());
    }

    #[test]
    fn configuration_defaults_and_validation_preserve_older_configs() {
        let default = crate::config::Config::from_user_toml("").unwrap();
        assert_eq!(default.attachments.file_bytes, 64 << 20);
        assert_eq!(default.attachments.batch_bytes, 128 << 20);
        assert_eq!(default.attachments.files, 32);
        assert_eq!(default.attachments.session_bytes, 1 << 30);
        let changed = crate::config::Config::from_user_toml(
            "[attachments]\nfile_bytes=9\nbatch_bytes=12\nfiles=3\nsession_bytes=50",
        )
        .unwrap();
        assert_eq!(
            (
                changed.attachments.file_bytes,
                changed.attachments.batch_bytes,
                changed.attachments.files,
                changed.attachments.session_bytes
            ),
            (9, 12, 3, 50)
        );
        assert!(crate::config::Config::from_user_toml("[attachments]\nfiles=0").is_err());
    }
}
