//! The laptop reader and the receiving job. The session server only relays their messages.

use std::collections::{HashSet, VecDeque};
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::net::UnixStream;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};

use super::Scope;
use crate::protocol::{self, ClientMsg, ServerMsg};

pub const CHUNK: usize = 64 * 1024;
pub const WINDOW: usize = 256 * 1024 / CHUNK;
pub const TIMEOUT: Duration = Duration::from_secs(30);
const ENTRIES: usize = 100_000;
pub const TOKEN_ENV: &str = "KRAAL_LAPTOP_COPY_TOKEN";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Spec {
    pub sources: Vec<String>,
    pub destination: PathBuf,
    pub allow: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum FromClient {
    Open { version: u32 },
    Data { sequence: u64, packet: Packet },
    Ack(u64),
    Progress,
    Finish(Result<(), String>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ToClient {
    Ready(Spec),
    Start { sources: Vec<String> },
    Data { sequence: u64, packet: Packet },
    Ack(u64),
    Finish(Result<(), String>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Kind {
    File,
    Directory,
    Link(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub path: String,
    pub kind: Kind,
    pub size: u64,
    pub mode: u32,
    pub seconds: i64,
    pub nanos: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Packet {
    Begin { bytes: u64, entries: usize },
    Entry(Entry),
    Chunk(String),
    End,
    Done,
}

impl Packet {
    pub fn bounded(&self) -> bool {
        match self {
            Self::Begin { entries, .. } => *entries <= ENTRIES,
            Self::Entry(e) => e.path.len() <= 16 * 1024 && !matches!(&e.kind, Kind::Link(to) if to.len() > 16 * 1024),
            Self::Chunk(data) => data.len() <= CHUNK.div_ceil(3) * 4,
            _ => true,
        }
    }
}

struct Source {
    path: PathBuf,
    entry: Entry,
    metadata: Option<fs::Metadata>,
}

fn same(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    a.dev() == b.dev()
        && a.ino() == b.ino()
        && a.mode() == b.mode()
        && a.len() == b.len()
        && a.mtime() == b.mtime()
        && a.mtime_nsec() == b.mtime_nsec()
        && a.ctime() == b.ctime()
        && a.ctime_nsec() == b.ctime_nsec()
}

fn open_file(path: &Path) -> Result<File> {
    let parent = super::open_directory(path.parent().context("missing parent directory")?)?;
    let name = CString::new(path.file_name().context("missing file name")?.as_bytes())?;
    // Nonblocking also refuses a file swapped for a FIFO without waiting for a writer.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    ensure!(fd >= 0, "cannot open laptop file: {}", std::io::Error::last_os_error());
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn manifest(
    paths: &[String],
    home: &Path,
    scope: Scope,
    macos: bool,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<Source>> {
    ensure!(!paths.is_empty() && paths.len() <= ENTRIES, "choose between 1 and 100,000 items");
    let mut pending = Vec::new();
    let mut tops = HashSet::new();
    for path in paths.iter().rev() {
        let (_, path) = super::scoped_path(path, home, scope, macos)?;
        let name = path.file_name().and_then(|p| p.to_str()).context("choose an item with a UTF-8 name")?.to_owned();
        ensure!(tops.insert(name.clone()), "two selected items have the same name: copy them separately");
        pending.push((path, name));
    }
    let mut sources = Vec::new();
    while let Some((path, relative)) = pending.pop() {
        check()?;
        ensure!(
            sources.len() < ENTRIES && relative.len() <= 16 * 1024,
            "too many entries or path too long: copy fewer items"
        );
        // Keep the parent descriptor open while inspecting the final component, too.
        let parent = super::open_directory(path.parent().context("missing parent")?)?;
        let name = CString::new(path.file_name().unwrap().as_bytes())?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        let ok =
            unsafe { libc::fstatat(parent.as_raw_fd(), name.as_ptr(), stat.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
        ensure!(ok == 0, "cannot inspect laptop item: {}", std::io::Error::last_os_error());
        let stat = unsafe { stat.assume_init() };
        let kind = stat.st_mode & libc::S_IFMT;
        if ![libc::S_IFREG, libc::S_IFDIR, libc::S_IFLNK].contains(&kind) {
            continue;
        }
        let (kind, metadata) = if kind == libc::S_IFLNK {
            let mut bytes = vec![0u8; 16 * 1024 + 1];
            let len =
                unsafe { libc::readlinkat(parent.as_raw_fd(), name.as_ptr(), bytes.as_mut_ptr().cast(), bytes.len()) };
            ensure!(len >= 0 && (len as usize) < bytes.len(), "cannot read symbolic link: copy it on the laptop");
            bytes.truncate(len as usize);
            let target = String::from_utf8(bytes).context("symbolic link is not UTF-8")?;
            let resolved = path
                .parent()
                .unwrap()
                .join(&target)
                .canonicalize()
                .context("cannot resolve symbolic link inside the laptop scope")?;
            super::scoped_path(resolved.to_str().context("link target is not UTF-8")?, home, scope, macos)?;
            (Kind::Link(target), None)
        } else {
            let file =
                if kind == libc::S_IFDIR { File::from(super::open_directory(&path)?) } else { open_file(&path)? };
            let metadata = file.metadata()?;
            // macOS uses a signed 32-bit device id; MetadataExt exposes it as u64.
            #[allow(clippy::unnecessary_cast)]
            let device = stat.st_dev as u64;
            ensure!(metadata.dev() == device && metadata.ino() == stat.st_ino, "laptop item changed: retry the copy");
            if metadata.is_dir() {
                let (_, entries) =
                    super::list_directory(path.to_str().context("path is not UTF-8")?, home, scope, macos, true)?;
                ensure!(pending.len() + sources.len() + entries.len() <= ENTRIES, "too many entries: copy fewer items");
                for entry in entries.into_iter().rev() {
                    ensure!(entry.exact, "a file name is not UTF-8: copy it on the laptop");
                    pending.push((path.join(&entry.name), format!("{relative}/{}", entry.name)));
                }
                (Kind::Directory, Some(metadata))
            } else {
                ensure!(metadata.is_file(), "laptop item changed: retry the copy");
                (Kind::File, Some(metadata))
            }
        };
        let size = if matches!(kind, Kind::File) { metadata.as_ref().unwrap().len() } else { 0 };
        // mode_t is 16 bits on macOS and 32 bits on Linux.
        #[allow(clippy::unnecessary_cast)]
        let mode = stat.st_mode as u32;
        let entry = Entry {
            path: relative,
            kind,
            size,
            mode: mode & 0o1777,
            seconds: stat.st_mtime,
            nanos: stat.st_mtime_nsec,
        };
        sources.push(Source { path, entry, metadata });
    }
    ensure!(!sources.is_empty(), "no regular files, directories or symbolic links to copy");
    Ok(sources)
}

/// Four bounded packets at most, each acknowledged in order by the spool writer.
pub struct Window {
    next: u64,
    pending: VecDeque<u64>,
}

impl Window {
    pub fn new() -> Self {
        Self { next: 0, pending: VecDeque::new() }
    }
    pub fn sent(&mut self) -> Result<u64> {
        ensure!(self.pending.len() < WINDOW, "laptop copy window exceeded");
        let sequence = self.next;
        self.next += 1;
        self.pending.push_back(sequence);
        Ok(sequence)
    }
    pub fn acknowledge(&mut self, sequence: u64) -> Result<()> {
        ensure!(self.pending.front() == Some(&sequence), "invalid laptop copy acknowledgement");
        self.pending.pop_front();
        Ok(())
    }
    pub fn full(&self) -> bool {
        self.pending.len() == WINDOW
    }
    pub fn empty(&self) -> bool {
        self.pending.is_empty()
    }
}

pub fn send(
    paths: &[String],
    home: &Path,
    scope: Scope,
    mut emit: impl FnMut(Packet) -> Result<()>,
    mut check: impl FnMut() -> Result<()>,
) -> Result<()> {
    let sources = manifest(paths, home, scope, cfg!(target_os = "macos"), &mut check)?;
    let total = sources.iter().try_fold(0u64, |n, s| n.checked_add(s.entry.size).context("copy size overflow"))?;
    emit(Packet::Begin { bytes: total, entries: sources.len() })?;
    let mut bytes = vec![0u8; CHUNK];
    for source in sources {
        check()?;
        let mut file = if matches!(source.entry.kind, Kind::File) {
            let file = open_file(&source.path)?;
            ensure!(same(source.metadata.as_ref().unwrap(), &file.metadata()?), "laptop file changed: retry the copy");
            Some(file)
        } else {
            None
        };
        emit(Packet::Entry(source.entry.clone()))?;
        if let Some(file) = file.as_mut() {
            let mut left = source.entry.size;
            while left != 0 {
                check()?;
                let n = left.min(CHUNK as u64) as usize;
                file.read_exact(&mut bytes[..n]).context("laptop file changed while copying: retry the copy")?;
                emit(Packet::Chunk(STANDARD.encode(&bytes[..n])))?;
                left -= n as u64;
            }
            ensure!(
                same(source.metadata.as_ref().unwrap(), &file.metadata()?),
                "laptop file changed while copying: retry the copy"
            );
        }
        emit(Packet::End)?;
    }
    emit(Packet::Done)
}

fn free_space(path: &Path) -> Result<u64> {
    let path = CString::new(path.as_os_str().as_bytes())?;
    let mut stat = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    let ok = unsafe { libc::statvfs(path.as_ptr(), stat.as_mut_ptr()) };
    ensure!(ok == 0, "cannot check free space: {}", std::io::Error::last_os_error());
    let stat = unsafe { stat.assume_init() };
    // macOS statvfs counts available blocks in u32; Linux uses u64.
    #[allow(clippy::unnecessary_cast)]
    let available = stat.f_bavail as u64;
    Ok(available.saturating_mul(stat.f_frsize))
}

fn enough_space(total: u64, available: u64, place: &Path) -> Result<()> {
    ensure!(
        total <= available,
        "not enough free space in {}: need {total} bytes, have {available}. Free space and retry",
        place.display()
    );
    Ok(())
}

fn relative(path: &str) -> Result<&Path> {
    let path = Path::new(path);
    ensure!(
        !path.as_os_str().is_empty() && path.components().all(|c| matches!(c, Component::Normal(_))),
        "invalid path in laptop copy"
    );
    Ok(path)
}

fn metadata(path: &Path, entry: &Entry) -> Result<()> {
    ensure!((0..1_000_000_000).contains(&entry.nanos), "invalid file timestamp");
    let path = CString::new(path.as_os_str().as_bytes())?;
    let times = [
        libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_OMIT },
        libc::timespec { tv_sec: entry.seconds, tv_nsec: entry.nanos as _ },
    ];
    let ok = unsafe { libc::utimensat(libc::AT_FDCWD, path.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) };
    ensure!(ok == 0, "cannot preserve modification time: {}", std::io::Error::last_os_error());
    if !matches!(entry.kind, Kind::Link(_)) {
        fs::set_permissions(
            Path::new(std::ffi::OsStr::from_bytes(path.to_bytes())),
            fs::Permissions::from_mode(entry.mode & 0o1777),
        )?;
    }
    Ok(())
}

pub struct Receiver {
    root: PathBuf,
    destination: PathBuf,
    tops: HashSet<String>,
    seen: HashSet<String>,
    directories: HashSet<PathBuf>,
    restore: Vec<Entry>,
    announced: Option<(u64, usize)>,
    written: u64,
    current: Option<(Entry, Option<File>, u64)>,
    done: bool,
}

impl Receiver {
    pub fn new(root: PathBuf, spec: &Spec) -> Result<Self> {
        let tops = spec
            .sources
            .iter()
            .map(|p| {
                Path::new(p).file_name().and_then(|p| p.to_str()).map(str::to_owned).context("invalid source name")
            })
            .collect::<Result<HashSet<_>>>()?;
        ensure!(tops.len() == spec.sources.len(), "duplicate source names");
        Ok(Self {
            root,
            destination: spec.destination.clone(),
            tops,
            seen: HashSet::new(),
            directories: HashSet::from([PathBuf::new()]),
            restore: Vec::new(),
            announced: None,
            written: 0,
            current: None,
            done: false,
        })
    }

    pub fn receive(&mut self, packet: Packet) -> Result<bool> {
        ensure!(!self.done && packet.bounded(), "invalid laptop copy packet");
        match packet {
            Packet::Begin { bytes, entries } => {
                ensure!(self.announced.is_none() && entries > 0, "invalid laptop copy total");
                enough_space(bytes, free_space(&self.root)?, &self.root)?;
                enough_space(bytes, free_space(&self.destination)?, &self.destination)?;
                eprintln!("Copying {entries} entries ({bytes} bytes). Close this window to cancel.");
                self.announced = Some((bytes, entries));
            }
            Packet::Entry(entry) => {
                let (_, count) = self.announced.context("missing laptop copy total")?;
                ensure!(self.current.is_none() && self.seen.len() < count, "unexpected laptop copy entry");
                let path = relative(&entry.path)?;
                let top = path.components().next().unwrap().as_os_str().to_str().unwrap();
                ensure!(
                    self.tops.contains(top) && self.directories.contains(path.parent().unwrap()),
                    "unexpected laptop copy path"
                );
                ensure!(self.seen.insert(entry.path.clone()), "duplicate laptop copy entry");
                let target = self.root.join(path);
                let file = match &entry.kind {
                    Kind::File => Some(
                        OpenOptions::new()
                            .write(true)
                            .create_new(true)
                            .mode(0o600)
                            .custom_flags(libc::O_NOFOLLOW)
                            .open(&target)?,
                    ),
                    Kind::Directory => {
                        ensure!(entry.size == 0, "invalid directory size");
                        fs::create_dir(&target)?;
                        self.directories.insert(path.to_path_buf());
                        None
                    }
                    Kind::Link(to) => {
                        ensure!(entry.size == 0, "invalid symbolic link size");
                        std::os::unix::fs::symlink(to, &target)?;
                        None
                    }
                };
                self.current = Some((entry, file, 0));
            }
            Packet::Chunk(data) => {
                let bytes = STANDARD.decode(data).context("invalid laptop copy data")?;
                ensure!(!bytes.is_empty() && bytes.len() <= CHUNK, "invalid laptop copy chunk size");
                let (entry, file, written) = self.current.as_mut().context("data without a file")?;
                let file = file.as_mut().context("data for a non-file")?;
                ensure!(bytes.len() as u64 <= entry.size.saturating_sub(*written), "file exceeds announced size");
                let total = self.announced.unwrap().0;
                ensure!(bytes.len() as u64 <= total.saturating_sub(self.written), "copy exceeds announced total");
                file.write_all(&bytes)?;
                *written += bytes.len() as u64;
                self.written += bytes.len() as u64;
                eprint!("\rReceived {} / {total} bytes", self.written);
            }
            Packet::End => {
                let (entry, file, written) = self.current.take().context("end without an entry")?;
                ensure!(written == entry.size, "laptop file ended early");
                if let Some(file) = file {
                    file.sync_all()?;
                }
                if matches!(entry.kind, Kind::Directory) {
                    self.restore.push(entry);
                } else {
                    metadata(&self.root.join(&entry.path), &entry)?;
                }
            }
            Packet::Done => {
                let (bytes, entries) = self.announced.context("missing laptop copy total")?;
                ensure!(
                    self.current.is_none()
                        && self.written == bytes
                        && self.seen.len() == entries
                        && self.tops.iter().all(|p| self.seen.contains(p)),
                    "incomplete laptop copy"
                );
                for entry in self.restore.iter().rev() {
                    metadata(&self.root.join(&entry.path), entry)?;
                }
                self.done = true;
                eprintln!("\nReceived {bytes} bytes. Installing the copy.");
            }
        }
        Ok(self.done)
    }
}

struct Spool(PathBuf);
impl Drop for Spool {
    fn drop(&mut self) {
        // Copied directories may be read-only; make our private copy removable again.
        let mut pending = vec![self.0.clone()];
        while let Some(path) = pending.pop() {
            if fs::symlink_metadata(&path).is_ok_and(|m| m.is_dir()) {
                let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o700));
                if let Ok(entries) = fs::read_dir(&path) {
                    pending.extend(entries.flatten().map(|entry| entry.path()));
                }
            }
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn spool_root(session: &str) -> PathBuf {
    crate::paths::state_dir().join("laptop-v1").join(session)
}

fn lock_spool(path: &Path) -> Result<Option<File>> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path.join(".lock"))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        return Ok(Some(file));
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::WouldBlock { Ok(None) } else { Err(error.into()) }
}

pub fn cleanup(session: &str) -> Result<()> {
    let root = spool_root(session);
    if !root.exists() {
        return Ok(());
    }
    crate::paths::ensure_private_dir(&root)?;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if crate::attachments::token_valid(&entry.file_name().to_string_lossy())
            && entry.file_type()?.is_dir()
            && let Some(_lock) = lock_spool(&entry.path())?
        {
            drop(Spool(entry.path()));
        }
    }
    Ok(())
}

pub fn install_args(spec: &Spec, root: &Path, rsync: bool) -> Vec<String> {
    let mut args = vec!["cp".into()];
    // The spool is disposable: system cp can merge and cross filesystems without modern rsync.
    if rsync {
        args.push("--move".into());
    }
    args.push("--no-replace".into());
    for name in &spec.allow {
        args.push(format!("--allow={name}"));
    }
    // Both endpoints are local, so --expect has no SSH identity to check.
    args.push("--".into());
    for source in &spec.sources {
        args.push(root.join(Path::new(source).file_name().unwrap()).to_string_lossy().into_owned());
    }
    args.push(format!("{}/", spec.destination.display()));
    args
}

struct Connection {
    stream: UnixStream,
    incoming: crossbeam_channel::Receiver<Result<ToClient, String>>,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    signals: Vec<signal_hook::SigId>,
    token: String,
}

impl Drop for Connection {
    fn drop(&mut self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
        for signal in self.signals.drain(..) {
            signal_hook::low_level::unregister(signal);
        }
    }
}

impl Connection {
    fn new(stream: UnixStream, token: String) -> Result<Self> {
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (outgoing, incoming) = crossbeam_channel::bounded(WINDOW + 1);
        let mut reader = stream.try_clone()?;
        let expected = token.clone();
        let mut connection = Self { stream, incoming, cancelled, signals: Vec::new(), token };
        for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
            connection.signals.push(signal_hook::flag::register(signal, connection.cancelled.clone())?);
        }
        crate::threads::named("laptop-copy-reply").spawn(move || {
            loop {
                let result = (|| -> Result<ToClient> {
                    let (kind, bytes) =
                        protocol::read_frame(&mut reader)?.context("connection lost: reconnect and retry the copy")?;
                    ensure!(kind == protocol::JSON, "invalid copy response");
                    match serde_json::from_slice(&bytes)? {
                        ServerMsg::LaptopCopy { token, message } if token == expected => Ok(message),
                        _ => bail!("invalid copy response"),
                    }
                })()
                .map_err(|e| format!("{e:#}"));
                let ended = result.is_err();
                if outgoing.send(result).is_err() || ended {
                    break;
                }
            }
        })?;
        Ok(connection)
    }

    fn send(&mut self, message: FromClient) -> Result<()> {
        protocol::write_json(&mut self.stream, &ClientMsg::LaptopCopy { token: self.token.clone(), message })?;
        Ok(())
    }

    fn check(&self) -> Result<()> {
        ensure!(
            !self.cancelled.load(std::sync::atomic::Ordering::Acquire),
            "Laptop copy cancelled: retry from the file manager"
        );
        Ok(())
    }

    fn receive(&self) -> Result<ToClient> {
        self.receive_until(Instant::now() + TIMEOUT)
    }

    fn receive_until(&self, deadline: Instant) -> Result<ToClient> {
        loop {
            self.check()?;
            let left = deadline.saturating_duration_since(Instant::now());
            ensure!(!left.is_zero(), "Laptop copy stalled for 30 seconds: reconnect and retry the copy");
            match self.incoming.recv_timeout(left.min(Duration::from_millis(50))) {
                Ok(Ok(ToClient::Finish(Err(error)))) | Ok(Err(error)) => bail!("{error}"),
                Ok(Ok(message)) => return Ok(message),
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                Err(_) => bail!("Laptop connection lost: reconnect and retry the copy"),
            }
        }
    }

    fn install(&mut self, command: &mut std::process::Command) -> Result<()> {
        self.install_with_timeout(command, TIMEOUT)
    }

    fn install_with_timeout(&mut self, command: &mut std::process::Command, timeout: Duration) -> Result<()> {
        self.check()?;
        // Read both output streams without blocking, including rsync's carriage-return progress.
        let (mut output, writer) = UnixStream::pair()?;
        output.set_nonblocking(true)?;
        command.stdout(std::os::fd::OwnedFd::from(writer.try_clone()?));
        command.stderr(std::os::fd::OwnedFd::from(writer));
        let mut child = crate::exec::own_group(command).spawn()?;
        let mut progress = Instant::now();
        let mut reported = progress;
        let mut bytes = [0; 16 * 1024];
        let result = (|| -> Result<()> {
            loop {
                self.check()?;
                match self.incoming.try_recv() {
                    Ok(Ok(ToClient::Finish(Err(error)))) | Ok(Err(error)) => bail!("{error}"),
                    Err(crossbeam_channel::TryRecvError::Empty) => {}
                    _ => bail!("Laptop connection lost during installation: check the destination and retry the copy"),
                }
                match output.read(&mut bytes) {
                    Ok(0) => {}
                    Ok(n) => {
                        std::io::stderr().write_all(&bytes[..n])?;
                        progress = Instant::now();
                        if progress.duration_since(reported) >= timeout.min(Duration::from_secs(1)) / 2 {
                            self.send(FromClient::Progress)?;
                            reported = progress;
                        }
                        continue;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) => return Err(error.into()),
                }
                if crate::exec::has_exited(&child) {
                    return Ok(());
                }
                ensure!(
                    progress.elapsed() < timeout,
                    "Laptop copy stalled for 30 seconds during installation: check the destination and retry the copy"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        })();
        if let Err(error) = result {
            crate::exec::kill_group_nowait(child);
            return Err(error);
        }
        let status = child.wait()?;
        ensure!(
            status.success(),
            "copy was not fully installed. Check the destination and any staging path reported above, then retry from the file manager"
        );
        Ok(())
    }
}

pub fn run(socket: &Path, session: &str) -> Result<()> {
    let token = std::env::var(TOKEN_ENV)
        .context("laptop-cp is started by the file manager: select laptop files and press c")?;
    ensure!(crate::attachments::token_valid(&token), "invalid laptop copy token");
    let stream = UnixStream::connect(socket)?;
    stream.set_write_timeout(Some(TIMEOUT))?;
    let mut connection = Connection::new(stream, token.clone())?;
    connection.send(FromClient::Open { version: protocol::PROTOCOL_VERSION })?;
    let spec = match connection.receive()? {
        ToClient::Ready(spec) => spec,
        _ => bail!("laptop copy was not authorized"),
    };
    let result = (|| -> Result<()> {
        let root = spool_root(session);
        crate::paths::ensure_private_dir(&crate::paths::state_dir().join("laptop-v1"))?;
        crate::paths::ensure_private_dir(&root)?;
        let path = root.join(&token);
        fs::create_dir(&path)?;
        let spool = Spool(path);
        crate::paths::ensure_private_dir(&spool.0)?;
        let _lock = lock_spool(&spool.0)?.context("laptop spool is already in use")?;
        let payload = spool.0.join("payload");
        crate::paths::ensure_private_dir(&payload)?;
        let mut receiver = Receiver::new(payload.clone(), &spec)?;
        let mut sequence = 0;
        loop {
            match connection.receive()? {
                ToClient::Data { sequence: received, packet } => {
                    ensure!(received == sequence, "out-of-order laptop copy data");
                    let done = receiver.receive(packet)?;
                    connection.send(FromClient::Ack(sequence))?;
                    sequence += 1;
                    if done {
                        break;
                    }
                }
                _ => bail!("unexpected laptop copy response"),
            }
        }
        let mut command = std::process::Command::new(std::env::current_exe()?);
        command.args(install_args(&spec, &payload, crate::transfer::rsync_capable())).env_remove(TOKEN_ENV);
        connection.install(&mut command)
    })();
    let sent = connection.send(FromClient::Finish(result.as_ref().map(|_| ()).map_err(|e| format!("{e:#}"))));
    result.context("Laptop copy failed: check the message above and retry from the file manager")?;
    sent.context("Connection lost after installation: check the destination before retrying")?;
    ensure!(
        matches!(connection.receive()?, ToClient::Finish(Ok(()))),
        "copy completion was not acknowledged: check the destination"
    );
    eprintln!("Laptop copy complete.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;
    use std::os::unix::fs::symlink;

    fn spec(home: &TempDir, destination: &TempDir, name: &str) -> Spec {
        Spec {
            sources: vec![home.0.join(name).to_str().unwrap().into()],
            destination: destination.0.clone(),
            allow: vec![],
        }
    }

    fn entry(path: &str, kind: Kind, size: u64) -> Packet {
        Packet::Entry(Entry { path: path.into(), kind, size, mode: 0o6755, seconds: 1_600_000_000, nanos: 123_456_789 })
    }

    #[test]
    fn recursive_copy_announces_total_preserves_contents_links_modes_and_dates() {
        let home = TempDir::new("copy-source");
        let spool = TempDir::new("copy-spool");
        let destination = TempDir::new("copy-destination");
        let dir = home.mkdir("folder");
        home.mkdir("folder/empty");
        let data = vec![123; CHUNK * 5 + 17];
        let file = home.write("folder/.nested", &data);
        symlink(".nested", dir.join("link")).unwrap();
        metadata(
            &file,
            &match entry("", Kind::File, 0) {
                Packet::Entry(e) => e,
                _ => unreachable!(),
            },
        )
        .unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o6755)).unwrap();
        let before = fs::metadata(&file).unwrap();
        let mut receiver = Receiver::new(spool.0.clone(), &spec(&home, &destination, "folder")).unwrap();
        let mut chunks = 0;
        let mut first = true;
        send(
            &["~/folder".into()],
            &home.0,
            Scope::Home,
            |packet| {
                if first {
                    assert!(matches!(packet, Packet::Begin { bytes, entries: 4 } if bytes == data.len() as u64));
                    first = false;
                }
                if let Packet::Chunk(bytes) = &packet {
                    chunks += 1;
                    assert!(STANDARD.decode(bytes).unwrap().len() <= CHUNK);
                }
                receiver.receive(packet).map(|_| ())
            },
            || Ok(()),
        )
        .unwrap();
        assert!(receiver.done);
        assert_eq!(chunks, 6);
        assert!(spool.0.join("folder/empty").is_dir());
        assert_eq!(fs::read(spool.0.join("folder/.nested")).unwrap(), data);
        assert_eq!(fs::read_link(spool.0.join("folder/link")).unwrap(), Path::new(".nested"));
        let copied = fs::metadata(spool.0.join("folder/.nested")).unwrap();
        assert_eq!(copied.mode() & 0o7777, 0o755);
        assert_eq!((copied.mtime(), copied.mtime_nsec()), (before.mtime(), before.mtime_nsec()));
        assert_eq!(fs::read(file).unwrap(), data, "the laptop source is never moved");
        assert!(fs::read_dir(&destination.0).unwrap().next().is_none(), "receiving never installs anything");
        install_without_rsync(&spec(&home, &destination, "folder"), &spool.0).unwrap();
        let installed = fs::metadata(destination.0.join("folder/.nested")).unwrap();
        assert_eq!(installed.mode() & 0o7777, 0o755);
        assert_eq!((installed.mtime(), installed.mtime_nsec()), (before.mtime(), before.mtime_nsec()));
        assert_eq!(fs::read(destination.0.join("folder/.nested")).unwrap(), data);
        assert_eq!(fs::read_link(destination.0.join("folder/link")).unwrap(), Path::new(".nested"));
    }

    #[test]
    fn recursive_copy_refuses_children_that_cannot_be_inspected() {
        let home = TempDir::new("copy-no-search");
        let dir = home.mkdir("folder");
        home.write("folder/file", "must not be omitted");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o400)).unwrap();
        let result = send(&["~/folder".into()], &home.0, Scope::Home, |_| panic!("nothing may be sent"), || Ok(()));
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(result.unwrap_err().to_string().contains("cannot inspect laptop item"));
    }

    fn install_without_rsync(spec: &Spec, spool: &Path) -> Result<()> {
        let args = install_args(spec, spool, false);
        assert!(!args.iter().any(|arg| arg == "--move"));
        let operands: Vec<_> = args.iter().skip_while(|arg| *arg != "--").skip(1).collect();
        let (destination, sources) = operands.split_last().unwrap();
        let sources: Vec<_> =
            sources.iter().map(|path| crate::transfer::Endpoint::Local(PathBuf::from(path))).collect();
        let options = crate::transfer::Options { replace: Some(spec.allow.clone()), ..Default::default() };
        let steps = crate::transfer::plan(
            &sources,
            &crate::transfer::Endpoint::Local(PathBuf::from(destination)),
            &|_| panic!("installation is local"),
            &options,
        )?;
        crate::transfer::run(&steps)
    }

    #[test]
    fn system_copy_merges_only_approved_directories() {
        let spool = TempDir::new("copy-system-spool");
        let destination = TempDir::new("copy-system-destination");
        spool.mkdir("folder");
        spool.write("folder/file", "new");
        destination.mkdir("folder");
        destination.write("folder/keep", "untouched");
        destination.write("folder/file", "old");
        let mut spec = spec(&spool, &destination, "folder");
        assert!(install_without_rsync(&spec, &spool.0).is_err());
        assert_eq!(fs::read(destination.0.join("folder/file")).unwrap(), b"old");
        assert_eq!(fs::read(destination.0.join("folder/keep")).unwrap(), b"untouched");
        spec.allow.push("folder".into());
        install_without_rsync(&spec, &spool.0).unwrap();
        assert_eq!(fs::read(destination.0.join("folder/file")).unwrap(), b"new");
        assert_eq!(fs::read(destination.0.join("folder/keep")).unwrap(), b"untouched");
        assert_eq!(fs::read(spool.0.join("folder/file")).unwrap(), b"new");
    }

    #[test]
    fn installed_directory_preserves_sticky_bit_and_strips_setgid() {
        let home = TempDir::new("copy-sticky-home");
        let spool = TempDir::new("copy-sticky-spool");
        let destination = TempDir::new("copy-sticky-destination");
        let dir = home.mkdir("shared");
        home.write("shared/file", "contents");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o3777)).unwrap();
        let spec = spec(&home, &destination, "shared");
        let mut receiver = Receiver::new(spool.0.clone(), &spec).unwrap();
        send(&["~/shared".into()], &home.0, Scope::Home, |packet| receiver.receive(packet).map(|_| ()), || Ok(()))
            .unwrap();
        assert_eq!(fs::metadata(spool.0.join("shared")).unwrap().mode() & 0o7777, 0o1777);
        install_without_rsync(&spec, &spool.0).unwrap();
        assert_eq!(fs::metadata(destination.0.join("shared")).unwrap().mode() & 0o7777, 0o1777);
        assert_eq!(fs::read(destination.0.join("shared/file")).unwrap(), b"contents");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn system_copy_installs_across_filesystems_without_replacing_conflicts() {
        let spool = TempDir::new("copy-cross-filesystem");
        let shm = Path::new("/dev/shm");
        if !shm.is_dir() || fs::metadata(shm).unwrap().dev() == fs::metadata(&spool.0).unwrap().dev() {
            return;
        }
        let path = shm.join(spool.0.file_name().unwrap());
        fs::create_dir(&path).unwrap();
        let destination = TempDir(path);
        spool.mkdir("folder");
        spool.write("folder/file", "contents");
        let spec = spec(&spool, &destination, "folder");
        install_without_rsync(&spec, &spool.0).unwrap();
        assert_eq!(fs::read(destination.0.join("folder/file")).unwrap(), b"contents");
        destination.write("folder/file", "keep");
        assert!(install_without_rsync(&spec, &spool.0).is_err());
        assert_eq!(fs::read(destination.0.join("folder/file")).unwrap(), b"keep");
        assert_eq!(fs::read(spool.0.join("folder/file")).unwrap(), b"contents");
    }

    #[test]
    fn scope_and_links_are_checked_before_announcing_or_reading_data() {
        let home = TempDir::new("copy-scope");
        let outside = TempDir::new("copy-outside");
        home.mkdir(".ssh");
        home.write(".ssh/key", "secret");
        outside.write("secret", "secret");
        symlink(&outside.0, home.0.join("escape")).unwrap();
        symlink(".ssh/key", home.0.join("hidden-link")).unwrap();
        for path in ["~/.ssh/key", "~/escape", "~/escape/secret", "~/hidden-link", "~/../file"] {
            let error = send(&[path.into()], &home.0, Scope::Home, |_| panic!("nothing may be sent"), || Ok(()));
            assert!(error.is_err(), "{path}");
        }
        assert!(send(&["~/file".into()], &home.0, Scope::Off, |_| panic!("scope off"), || Ok(())).is_err());
        home.mkdir("Library");
        home.write("Library/file", "private");
        assert!(manifest(&["~/Library/file".into()], &home.0, Scope::Home, true, &mut || Ok(())).is_err());
        assert!(
            manifest(&[outside.0.join("secret").to_str().unwrap().into()], &home.0, Scope::All, false, &mut || Ok(()))
                .is_ok()
        );
    }

    #[test]
    fn special_files_are_skipped_and_empty_files_and_directories_survive() {
        let home = TempDir::new("copy-special");
        home.mkdir("dir/empty");
        home.write("dir/file", "");
        let fifo = CString::new(home.0.join("dir/fifo").as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let sources = manifest(&["~/dir".into()], &home.0, Scope::Home, false, &mut || Ok(())).unwrap();
        assert_eq!(sources.len(), 3);
        assert!(sources.iter().all(|s| s.entry.size == 0));
    }

    #[test]
    fn a_file_or_parent_swapped_after_the_total_is_never_followed() {
        for parent in [false, true] {
            let home = TempDir::new("copy-race");
            let outside = TempDir::new("copy-race-outside");
            home.mkdir("dir");
            home.write("dir/file", "original");
            outside.write("file", "private");
            let result = send(
                &["~/dir/file".into()],
                &home.0,
                Scope::Home,
                |packet| {
                    if matches!(packet, Packet::Begin { .. }) {
                        if parent {
                            fs::rename(home.0.join("dir"), home.0.join("old")).unwrap();
                            symlink(&outside.0, home.0.join("dir")).unwrap();
                        } else {
                            fs::remove_file(home.0.join("dir/file")).unwrap();
                            symlink(outside.0.join("file"), home.0.join("dir/file")).unwrap();
                        }
                    }
                    assert!(!matches!(packet, Packet::Chunk(_)));
                    Ok(())
                },
                || Ok(()),
            );
            assert!(result.is_err());
        }
    }

    #[test]
    fn changing_a_source_during_reading_aborts_before_done() {
        let home = TempDir::new("copy-changing");
        let file = home.write("file", vec![1; CHUNK * 2]);
        let result = send(
            &["~/file".into()],
            &home.0,
            Scope::Home,
            |packet| {
                if matches!(packet, Packet::Chunk(_)) {
                    fs::write(&file, b"shorter").unwrap();
                }
                assert!(!matches!(packet, Packet::Done));
                Ok(())
            },
            || Ok(()),
        );
        assert!(result.is_err());
    }

    #[test]
    fn traversal_is_bounded_without_imposing_an_attachment_file_size_limit() {
        let home = TempDir::new("copy-limits");
        let file = home.write("large", "");
        File::options().write(true).open(file).unwrap().set_len(100 * 1024 * 1024).unwrap();
        let source = manifest(&["~/large".into()], &home.0, Scope::Home, false, &mut || Ok(())).unwrap();
        assert_eq!(source[0].entry.size, 100 * 1024 * 1024);
        assert!(manifest(&vec!["~/large".into(); ENTRIES + 1], &home.0, Scope::Home, false, &mut || Ok(())).is_err());
        assert!(manifest(&["~/large".into(), "~/large".into()], &home.0, Scope::Home, false, &mut || Ok(())).is_err());
    }

    #[test]
    fn receiving_refuses_traversal_duplicates_symlink_parents_and_wrong_tops() {
        for path in ["../escape", "/absolute", "dir/../escape", "other", "dir/child", ""] {
            let spool = TempDir::new("copy-path");
            let dest = TempDir::new("copy-path-dest");
            let mut r = Receiver::new(spool.0.clone(), &spec(&spool, &dest, "dir")).unwrap();
            r.receive(Packet::Begin { bytes: 0, entries: 2 }).unwrap();
            if path == "dir/child" {
                r.receive(entry("dir", Kind::Link(dest.text()), 0)).unwrap();
                r.receive(Packet::End).unwrap();
            }
            assert!(r.receive(entry(path, Kind::File, 0)).is_err(), "{path}");
            assert!(fs::read_dir(&dest.0).unwrap().next().is_none());
        }
        let spool = TempDir::new("copy-duplicate");
        let dest = TempDir::new("copy-duplicate-dest");
        let mut r = Receiver::new(spool.0.clone(), &spec(&spool, &dest, "file")).unwrap();
        r.receive(Packet::Begin { bytes: 0, entries: 2 }).unwrap();
        r.receive(entry("file", Kind::File, 0)).unwrap();
        r.receive(Packet::End).unwrap();
        assert!(r.receive(entry("file", Kind::File, 0)).is_err());
    }

    #[test]
    fn receiving_checks_space_totals_sizes_and_completeness() {
        let spool = TempDir::new("copy-size");
        let dest = TempDir::new("copy-size-dest");
        let spec = spec(&spool, &dest, "file");
        let mut r = Receiver::new(spool.0.clone(), &spec).unwrap();
        assert!(
            r.receive(Packet::Begin { bytes: u64::MAX, entries: 1 }).unwrap_err().to_string().contains("free space")
        );
        assert!(fs::read_dir(&spool.0).unwrap().next().is_none());
        assert!(enough_space(2, 1, &dest.0).is_err());
        enough_space(1, 1, &dest.0).unwrap();
        for packets in [
            vec![Packet::Done],
            vec![entry("file", Kind::File, 0)],
            vec![Packet::Begin { bytes: 1, entries: 1 }, Packet::Done],
            vec![Packet::Begin { bytes: 0, entries: ENTRIES + 1 }],
            vec![Packet::Begin { bytes: 0, entries: 1 }, Packet::Begin { bytes: 0, entries: 1 }],
        ] {
            let mut r = Receiver::new(spool.0.clone(), &spec).unwrap();
            assert!(packets.into_iter().try_for_each(|p| r.receive(p).map(|_| ())).is_err());
        }
        r.receive(Packet::Begin { bytes: 1, entries: 1 }).unwrap();
        r.receive(entry("file", Kind::File, 1)).unwrap();
        assert!(r.receive(Packet::Chunk(STANDARD.encode([1, 2]))).is_err());
        assert!(r.receive(Packet::Chunk(STANDARD.encode(vec![0; CHUNK + 1]))).is_err());
        assert!(r.receive(Packet::Chunk("invalid base64".into())).is_err());
        assert!(r.receive(Packet::End).is_err());
        assert!(!r.done);
    }

    #[test]
    fn window_allows_256_kib_and_only_ordered_acknowledgements_release_it() {
        let mut window = Window::new();
        for sequence in 0..4 {
            assert_eq!(window.sent().unwrap(), sequence);
        }
        assert!(window.full());
        assert!(window.sent().is_err());
        assert!(window.acknowledge(1).is_err());
        window.acknowledge(0).unwrap();
        assert_eq!(window.sent().unwrap(), 4);
        assert!(window.acknowledge(0).is_err());
        for sequence in 1..5 {
            window.acknowledge(sequence).unwrap();
        }
        assert!(window.empty());
    }

    #[test]
    fn install_reuses_move_no_replace_and_only_approved_names_as_literal_arguments() {
        let spec = Spec {
            sources: vec!["~/dir/-file with 'quotes'".into()],
            destination: "/dest space".into(),
            allow: vec!["-file with 'quotes'".into()],
        };
        assert_eq!(
            install_args(&spec, Path::new("/private/spool"), true),
            [
                "cp",
                "--move",
                "--no-replace",
                "--allow=-file with 'quotes'",
                "--",
                "/private/spool/-file with 'quotes'",
                "/dest space/"
            ]
        );
    }

    #[test]
    fn abandoned_spools_are_removed_but_live_and_unrelated_directories_are_kept() {
        if !crate::testing::in_own_process(
            "laptop::copy::tests::abandoned_spools_are_removed_but_live_and_unrelated_directories_are_kept",
        ) {
            return;
        }
        let root = spool_root("test");
        fs::create_dir_all(root.join("abc/payload/read-only")).unwrap();
        fs::set_permissions(root.join("abc/payload/read-only"), fs::Permissions::from_mode(0o500)).unwrap();
        fs::create_dir_all(root.join("def/payload")).unwrap();
        fs::create_dir_all(root.join("unrelated")).unwrap();
        let lock = lock_spool(&root.join("def")).unwrap().unwrap();
        cleanup("test").unwrap();
        assert!(!root.join("abc").exists());
        assert!(root.join("def").exists());
        assert!(root.join("unrelated").exists());
        drop(lock);
        cleanup("test").unwrap();
        assert!(!root.join("def").exists());
        assert_eq!(fs::metadata(&root).unwrap().mode() & 0o777, 0o700);
    }

    #[test]
    fn cancelled_spool_drop_removes_partial_files_without_following_links() {
        let root = TempDir::new("copy-drop");
        let outside = TempDir::new("copy-drop-outside");
        outside.write("keep", "untouched");
        let spool = Spool(root.mkdir("op"));
        fs::write(spool.0.join("partial"), "partial").unwrap();
        symlink(&outside.0, spool.0.join("link")).unwrap();
        drop(spool);
        assert!(!root.0.join("op").exists());
        assert_eq!(fs::read(outside.0.join("keep")).unwrap(), b"untouched");
    }

    #[test]
    fn dropped_connections_deadlines_and_signals_fail_without_waiting_for_more_data() {
        let (stream, peer) = UnixStream::pair().unwrap();
        let connection = Connection::new(stream, "abc".into()).unwrap();
        assert!(connection.receive_until(Instant::now()).unwrap_err().to_string().contains("30 seconds"));
        connection.cancelled.store(true, std::sync::atomic::Ordering::Release);
        assert!(connection.receive().unwrap_err().to_string().contains("cancelled"));
        connection.cancelled.store(false, std::sync::atomic::Ordering::Release);
        drop(peer);
        assert!(connection.receive().unwrap_err().to_string().contains("connection lost"));
    }

    #[test]
    fn stalled_install_child_times_out_while_attachment_stays_connected() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let mut connection = Connection::new(stream, "abc".into()).unwrap();
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "exec sleep 30"]);
        let before = Instant::now();
        let error = connection.install_with_timeout(&mut command, Duration::from_millis(100)).unwrap_err();
        assert!(error.to_string().contains("stalled for 30 seconds during installation"));
        assert!(before.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn install_output_renews_deadline_and_reports_progress_to_server() {
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let mut connection = Connection::new(stream, "abc".into()).unwrap();
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "i=0; while [ \"$i\" -lt 8 ]; do printf 'copied\\r'; sleep 0.1; i=$((i + 1)); done"]);
        let before = Instant::now();
        connection.install_with_timeout(&mut command, Duration::from_millis(500)).unwrap();
        assert!(before.elapsed() >= Duration::from_millis(500));
        peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let (_, bytes) = protocol::read_frame(&mut peer).unwrap().unwrap();
        assert!(
            matches!(serde_json::from_slice::<ClientMsg>(&bytes).unwrap(), ClientMsg::LaptopCopy { token, message: FromClient::Progress } if token == "abc")
        );
    }

    #[test]
    fn connection_loss_stops_the_install_child() {
        let (stream, peer) = UnixStream::pair().unwrap();
        let mut connection = Connection::new(stream, "abc".into()).unwrap();
        drop(peer);
        let mut command = std::process::Command::new("sh");
        command.args(["-c", "exec sleep 30"]);
        let before = Instant::now();
        assert!(connection.install(&mut command).is_err());
        assert!(before.elapsed() < Duration::from_secs(5));
    }
}
