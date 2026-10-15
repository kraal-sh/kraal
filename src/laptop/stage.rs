//! Native staging on the laptop. Directory descriptors anchor every write and rename.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{CStr, CString};
use std::fs::File;
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

use super::Scope;
use super::copy::{Entry, Kind, Packet, Receiver, Spec};

fn checked(rc: i32) -> io::Result<()> {
    if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

pub(super) fn name(path: &Path) -> Result<CString> {
    Ok(CString::new(path.as_os_str().as_bytes())?)
}

pub(super) fn parent(root: &OwnedFd, path: &Path) -> Result<(OwnedFd, CString)> {
    let mut fd = root.try_clone()?;
    for part in path.parent().context("missing parent")?.components() {
        let std::path::Component::Normal(part) = part else { bail!("invalid staged path") };
        fd = super::directory(fd.as_raw_fd(), &CString::new(part.as_bytes())?)?;
    }
    Ok((fd, name(Path::new(path.file_name().context("missing name")?))?))
}

pub(super) fn create(fd: &OwnedFd, name: &CStr) -> Result<File> {
    let raw = unsafe {
        libc::openat(
            fd.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    ensure!(raw >= 0, "cannot create staged file: {}", io::Error::last_os_error());
    Ok(unsafe { File::from_raw_fd(raw) })
}

pub(super) fn mkdir(fd: &OwnedFd, name: &CStr) -> Result<OwnedFd> {
    checked(unsafe { libc::mkdirat(fd.as_raw_fd(), name.as_ptr(), 0o700) })?;
    Ok(super::directory(fd.as_raw_fd(), name)?)
}

pub(super) fn create_entry(root: &OwnedFd, entry: &Entry) -> Result<Option<File>> {
    let (fd, leaf) = parent(root, Path::new(&entry.path))?;
    match &entry.kind {
        Kind::File => Ok(Some(create(&fd, &leaf)?)),
        Kind::Directory => {
            mkdir(&fd, &leaf)?;
            Ok(None)
        }
        Kind::Link(target) => {
            let target = CString::new(target.as_bytes())?;
            checked(unsafe { libc::symlinkat(target.as_ptr(), fd.as_raw_fd(), leaf.as_ptr()) })?;
            Ok(None)
        }
    }
}

pub(super) fn metadata(root: &OwnedFd, entry: &Entry, file: Option<&File>) -> Result<()> {
    ensure!((0..1_000_000_000).contains(&entry.nanos), "invalid file timestamp");
    let times = [
        libc::timespec { tv_sec: 0, tv_nsec: libc::UTIME_OMIT },
        libc::timespec { tv_sec: entry.seconds, tv_nsec: entry.nanos as _ },
    ];
    let (fd, leaf) = parent(root, Path::new(&entry.path))?;
    if matches!(entry.kind, Kind::Link(_)) {
        checked(unsafe { libc::utimensat(fd.as_raw_fd(), leaf.as_ptr(), times.as_ptr(), libc::AT_SYMLINK_NOFOLLOW) })?;
    } else {
        let directory;
        let raw = if let Some(file) = file {
            file.as_raw_fd()
        } else {
            directory = super::directory(fd.as_raw_fd(), &leaf)?;
            directory.as_raw_fd()
        };
        checked(unsafe { libc::futimens(raw, times.as_ptr()) })?;
        checked(unsafe { libc::fchmod(raw, (entry.mode & 0o1777) as _) })?;
    }
    Ok(())
}

pub(super) fn rename(from: &OwnedFd, source: &CStr, to: &OwnedFd, target: &CStr) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            from.as_raw_fd(),
            source.as_ptr(),
            to.as_raw_fd(),
            target.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    } as i32;
    // The descriptor-relative form of renamex_np keeps renamed parent directories anchored.
    #[cfg(target_os = "macos")]
    let rc = unsafe {
        libc::renameatx_np(from.as_raw_fd(), source.as_ptr(), to.as_raw_fd(), target.as_ptr(), libc::RENAME_EXCL)
    };
    checked(rc)
}

pub(super) fn stat(fd: &OwnedFd, name: &CStr) -> io::Result<libc::stat> {
    let mut stat = std::mem::MaybeUninit::uninit();
    checked(unsafe { libc::fstatat(fd.as_raw_fd(), name.as_ptr(), stat.as_mut_ptr(), libc::AT_SYMLINK_NOFOLLOW) })?;
    Ok(unsafe { stat.assume_init() })
}

pub(super) fn same(a: &libc::stat, b: &libc::stat) -> bool {
    a.st_dev == b.st_dev
        && a.st_ino == b.st_ino
        && a.st_mode == b.st_mode
        && a.st_size == b.st_size
        && a.st_mtime == b.st_mtime
        && a.st_mtime_nsec == b.st_mtime_nsec
}

fn children(fd: &OwnedFd, mut visit: impl FnMut(&CStr) -> Result<()>) -> Result<()> {
    let duplicate = fd.try_clone()?;
    let raw = unsafe { libc::fdopendir(duplicate.as_raw_fd()) };
    ensure!(!raw.is_null(), "cannot read directory: {}", io::Error::last_os_error());
    use std::os::fd::IntoRawFd;
    let _ = duplicate.into_raw_fd();
    let directory = super::Directory(raw);
    loop {
        let entry = unsafe {
            *super::errno() = 0;
            libc::readdir(directory.0)
        };
        if entry.is_null() {
            ensure!(unsafe { *super::errno() } == 0, "cannot finish reading directory: {}", io::Error::last_os_error());
            break;
        }
        let leaf = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if leaf != c"." && leaf != c".." {
            visit(leaf)?;
        }
    }
    Ok(())
}

struct Snapshot {
    info: libc::stat,
    children: HashMap<CString, Snapshot>,
}

impl Snapshot {
    fn read(fd: &OwnedFd, name: &CStr, check: &mut impl FnMut() -> Result<()>) -> Result<Self> {
        let info = stat(fd, name)?;
        let mut entries = HashMap::new();
        if info.st_mode & libc::S_IFMT == libc::S_IFDIR {
            let child = super::directory(fd.as_raw_fd(), name)?;
            children(&child, |leaf| {
                check()?;
                entries.insert(leaf.to_owned(), Self::read(&child, leaf, check)?);
                Ok(())
            })?;
        }
        ensure!(same(&info, &stat(fd, name)?), "item changed while inspecting it: retry the copy");
        Ok(Self { info, children: entries })
    }

    fn same(&self, other: &Self) -> bool {
        same(&self.info, &other.info)
            && self.children.len() == other.children.len()
            && self.children.iter().all(|(name, child)| {
                other.children.get(name).is_some_and(|other| {
                    child.info.st_ctime == other.info.st_ctime
                        && child.info.st_ctime_nsec == other.info.st_ctime_nsec
                        && child.same(other)
                })
            })
    }

    fn matches(&self, fd: &OwnedFd, name: &CStr, check: &mut impl FnMut() -> Result<()>) -> Result<bool> {
        Ok(self.same(&Self::read(fd, name, check)?))
    }
}

fn remove(fd: &OwnedFd, name: &CStr) -> Result<()> {
    remove_with(fd, name, &mut || Ok(()))
}

fn remove_with(fd: &OwnedFd, name: &CStr, check: &mut impl FnMut() -> Result<()>) -> Result<()> {
    check()?;
    let info = stat(fd, name)?;
    if info.st_mode & libc::S_IFMT != libc::S_IFDIR {
        checked(unsafe { libc::unlinkat(fd.as_raw_fd(), name.as_ptr(), 0) })?;
        return Ok(());
    }
    // Only our private tree is cleaned. Never follow a substituted link, including on cancellation.
    checked(unsafe { libc::fchmodat(fd.as_raw_fd(), name.as_ptr(), 0o700, libc::AT_SYMLINK_NOFOLLOW) })?;
    let child = super::directory(fd.as_raw_fd(), name)?;
    children(&child, |leaf| remove_with(&child, leaf, check))?;
    check()?;
    checked(unsafe { libc::unlinkat(fd.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) })?;
    Ok(())
}

pub fn case_sensitive(fd: &OwnedFd) -> Result<bool> {
    #[cfg(target_os = "macos")]
    {
        let value = unsafe { libc::fpathconf(fd.as_raw_fd(), libc::_PC_CASE_SENSITIVE) };
        ensure!(value >= 0, "cannot determine filesystem case sensitivity: choose another destination");
        Ok(value != 0)
    }
    #[cfg(target_os = "linux")]
    {
        let _ = fd;
        Ok(true)
    }
}

pub struct Stage {
    destination: PathBuf,
    home: PathBuf,
    scope: Scope,
    fd: OwnedFd,
    stage: OwnedFd,
    kind: File,
    leaf: CString,
    pub path: PathBuf,
    receiver: Receiver,
    names: Vec<String>,
    source_names: HashSet<String>,
    replacements: HashMap<String, Snapshot>,
    case_sensitive: bool,
    link_targets: HashMap<String, String>,
    links: Vec<Entry>,
    links_by_top: HashMap<String, Vec<usize>>,
    keep: bool,
}

impl Stage {
    pub fn new(spec: &Spec, home: &Path, scope: Scope) -> Result<Self> {
        let (home, destination) = super::scoped_path(
            spec.destination.to_str().context("invalid destination")?,
            home,
            scope,
            cfg!(target_os = "macos"),
        )?;
        let fd = super::open_directory(&destination)?;
        let mut names = Vec::new();
        let mut replacements = HashMap::new();
        let sensitive = case_sensitive(&fd)?;
        let mut seen = HashSet::new();
        for source in &spec.sources {
            let top = Path::new(source).file_name().and_then(|s| s.to_str()).context("invalid source name")?;
            super::scoped_path(
                destination.join(top).to_str().context("invalid destination")?,
                &home,
                scope,
                cfg!(target_os = "macos"),
            )?;
            ensure!(
                seen.insert(if sensitive { top.to_owned() } else { top.to_lowercase() }),
                "source names collide on this filesystem: copy them separately"
            );
            let leaf = CString::new(top)?;
            match stat(&fd, &leaf) {
                Ok(_) => {
                    replacements.insert(top.into(), Snapshot::read(&fd, &leaf, &mut || Ok(()))?);
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            names.push(top.into());
        }
        ensure!(!names.is_empty(), "choose files to copy");
        let id = super::Identity::new()?.id;
        let leaf = CString::new(format!(".kraal-stage-{}-{id}", &id[..16]))?;
        let path = destination.join(leaf.to_str()?);
        let stage = mkdir(&fd, &leaf)?;
        let prepared = (|| -> Result<(Receiver, File)> {
            let payload = mkdir(&stage, c"payload")?;
            let work = mkdir(&stage, c"work")?;
            for (name, text) in [
                (c"format", "kraal-stage 2".to_owned()),
                (c"owner", crate::transfer::owner_token().unwrap_or_else(|| "unknown".into())),
            ] {
                writeln!(create(&stage, name)?, "{text}")?;
            }
            probe(&work)?;
            let mut kind = create(&stage, c"kind")?;
            writeln!(kind, "copy")?;
            let receiver = Receiver::at(
                path.join("payload"),
                payload,
                fd.try_clone()?,
                &Spec { destination: destination.clone(), ..spec.clone() },
            )?;
            Ok((receiver, kind))
        })();
        let (mut receiver, kind) = match prepared {
            Ok(receiver) => receiver,
            Err(error) => {
                let _ = remove(&fd, &leaf);
                return Err(error);
            }
        };
        receiver.quiet = true;
        Ok(Self {
            destination,
            home,
            scope,
            fd,
            stage,
            kind,
            leaf,
            path,
            receiver,
            names,
            source_names: seen,
            replacements,
            case_sensitive: sensitive,
            link_targets: HashMap::new(),
            links: Vec::new(),
            links_by_top: HashMap::new(),
            keep: false,
        })
    }

    pub fn replacements(&self) -> Vec<String> {
        self.names.iter().filter(|name| self.replacements.contains_key(*name)).cloned().collect()
    }

    fn path_key(&self, path: &Path) -> String {
        if self.case_sensitive { path.to_string_lossy().into_owned() } else { path.to_string_lossy().to_lowercase() }
    }

    fn projected(&self, path: &Path, top: Option<&str>) -> bool {
        path.strip_prefix(&self.destination).ok().and_then(|path| path.components().next()).is_some_and(|part| {
            let name = self.path_key(Path::new(part.as_os_str()));
            self.source_names.contains(&name) && top.is_none_or(|top| name == self.path_key(Path::new(top)))
        })
    }

    fn validate_link(&self, entry: &Entry, top: Option<&str>) -> Result<()> {
        let Kind::Link(target) = &entry.kind else { return Ok(()) };
        let target = self.destination.join(&entry.path).parent().unwrap().join(target);
        let mut pending: VecDeque<_> = target.components().map(|part| part.as_os_str().to_owned()).collect();
        let mut resolved = PathBuf::new();
        let mut followed = 1;
        while let Some(part) = pending.pop_front() {
            if part == "." {
                continue;
            }
            if part == "/" {
                resolved = PathBuf::from("/");
                continue;
            }
            if part == ".." {
                ensure!(resolved.pop(), "link escapes the laptop scope: copy its target instead");
            } else {
                resolved.push(&part);
            }
            if self.scope == Scope::Home && !self.home.starts_with(&resolved) {
                super::scoped_path(
                    resolved.to_str().context("invalid link")?,
                    &self.home,
                    self.scope,
                    cfg!(target_os = "macos"),
                )?;
            }
            if part == ".." {
                continue;
            }
            let target = if self.projected(&resolved, top) {
                // Only this incoming item's contents replace the old tree during its put.
                self.link_targets.get(&self.path_key(&resolved)).cloned()
            } else {
                match super::open_directory(resolved.parent().unwrap()) {
                    Ok(fd) => {
                        let leaf = CString::new(part.as_bytes())?;
                        match stat(&fd, &leaf) {
                            Ok(info) if info.st_mode & libc::S_IFMT == libc::S_IFLNK => {
                                let mut bytes = vec![0u8; 16 * 1024 + 1];
                                let len = unsafe {
                                    libc::readlinkat(
                                        fd.as_raw_fd(),
                                        leaf.as_ptr(),
                                        bytes.as_mut_ptr().cast(),
                                        bytes.len(),
                                    )
                                };
                                ensure!(
                                    len >= 0 && (len as usize) < bytes.len(),
                                    "link changed or is too long: retry the copy"
                                );
                                bytes.truncate(len as usize);
                                Some(String::from_utf8(bytes).context("link target is not UTF-8")?)
                            }
                            Ok(_) => None,
                            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
                            Err(e) => return Err(e.into()),
                        }
                    }
                    Err(e) if e.downcast_ref::<io::Error>().is_some_and(|e| e.kind() == io::ErrorKind::NotFound) => {
                        None
                    }
                    Err(e) => return Err(e),
                }
            };
            if let Some(target) = target {
                followed += 1;
                ensure!(followed <= 40, "symbolic link loop or chain too long: copy its target instead");
                resolved.pop();
                for component in Path::new(&target).components().rev() {
                    pending.push_front(component.as_os_str().to_owned());
                }
            }
        }
        super::scoped_path(
            resolved.to_str().context("invalid link")?,
            &self.home,
            self.scope,
            cfg!(target_os = "macos"),
        )?;
        Ok(())
    }

    pub fn receive(&mut self, packet: Packet) -> Result<bool> {
        if let Packet::Entry(entry) = &packet {
            let path = super::copy::relative(&entry.path)?;
            if let Kind::Link(target) = &entry.kind {
                let top = path.components().next().unwrap().as_os_str().to_str().unwrap().to_owned();
                self.links_by_top.entry(top).or_default().push(self.links.len());
                self.link_targets.insert(self.path_key(&self.destination.join(&entry.path)), target.clone());
                self.links.push(entry.clone());
            }
        }
        self.receiver.receive(packet)
    }

    pub fn install(&mut self, mut check: impl FnMut() -> Result<()>) -> Result<()> {
        let result = self.put(&mut check).and_then(|()| self.cleanup(&mut check));
        if self.keep {
            result.with_context(|| {
                format!("Copy kept in {}: check payload and work before retrying", self.path.display())
            })
        } else {
            result
        }
    }

    fn cleanup(&mut self, check: &mut impl FnMut() -> Result<()>) -> Result<()> {
        // Once installed, Drop must never retry interrupted cleanup or hide its errors.
        self.keep = true;
        let result = (|| -> Result<()> {
            let work = super::directory(self.stage.as_raw_fd(), c"work")?;
            for (name, expected) in &self.replacements {
                check()?;
                ensure!(
                    expected.matches(&work, &CString::new(name.as_bytes())?, check)?,
                    "previous item {name} changed: retained in work"
                );
            }
            remove_with(&self.fd, &self.leaf, check)
        })();
        result.context("Copy installed, but staging cleanup did not finish")
    }

    fn put(&mut self, mut check: impl FnMut() -> Result<()>) -> Result<()> {
        for entry in &self.links {
            check()?;
            self.validate_link(entry, None)?;
        }
        let current = super::open_directory(&self.destination)?;
        let a = File::from(current).metadata()?;
        let b = File::from(self.fd.try_clone()?).metadata()?;
        use std::os::unix::fs::MetadataExt;
        ensure!(a.dev() == b.dev() && a.ino() == b.ino(), "destination changed: retry the copy");
        let payload = super::directory(self.stage.as_raw_fd(), c"payload")?;
        let work = super::directory(self.stage.as_raw_fd(), c"work")?;
        let mut waiting = Vec::new();
        for name in &self.names {
            check()?;
            let leaf = CString::new(name.as_bytes())?;
            let mut backed_up = false;
            if let Some(expected) = self.replacements.get(name) {
                if !expected.matches(&self.fd, &leaf, &mut check).unwrap_or(false) {
                    self.keep = true;
                    waiting.push(name.clone());
                    continue;
                }
                check()?;
                // A raced item kept in work may be its only copy. Format 2 cleaners must retain it.
                self.kind.write_all_at(b"move\n", 0)?;
                self.kind.sync_all()?;
                if let Err(error) = rename(&self.fd, &leaf, &work, &leaf) {
                    self.keep = true;
                    waiting.push(format!("{name}: {error}"));
                    continue;
                }
                backed_up = true;
                if !expected.matches(&work, &leaf, &mut check).unwrap_or(false) {
                    let _ = rename(&work, &leaf, &self.fd, &leaf);
                    self.keep = true;
                    waiting.push(name.clone());
                    continue;
                }
            }
            if let Err(error) = check() {
                if backed_up && rename(&work, &leaf, &self.fd, &leaf).is_err() {
                    self.keep = true;
                    bail!(
                        "{error}. Previous item {name} waits in {}/work; check it before retrying",
                        self.path.display()
                    );
                }
                return Err(error);
            }
            let result = self
                .links_by_top
                .get(name)
                .into_iter()
                .flatten()
                .try_for_each(|index| self.validate_link(&self.links[*index], Some(name)))
                .and_then(|()| rename(&payload, &leaf, &self.fd, &leaf).map_err(Into::into));
            if backed_up && result.is_ok() {
                // A later item's cancellation must not delete this backup in Drop.
                self.keep = true;
            }
            if let Err(error) = result {
                if backed_up {
                    let _ = rename(&work, &leaf, &self.fd, &leaf);
                }
                self.keep = true;
                waiting.push(format!("{name}: {error}"));
            }
        }
        if !waiting.is_empty() {
            self.keep = true;
            bail!(
                "Not replaced: {}. The copy waits in {}. Check these names and copy again; previous items, if any, are in work",
                waiting.join(", "),
                self.path.display()
            );
        }
        Ok(())
    }
}

fn probe(work: &OwnedFd) -> Result<()> {
    probe_with(work, |source, target| rename(work, source, work, target))
}

fn probe_with(work: &OwnedFd, rename: impl Fn(&CStr, &CStr) -> io::Result<()>) -> Result<()> {
    for directory in [true, false] {
        if directory {
            mkdir(work, c"probe-a")?;
            mkdir(work, c"probe-b")?;
        } else {
            create(work, c"probe-a")?;
            create(work, c"probe-b")?;
        }
        let conflict = rename(c"probe-a", c"probe-b");
        ensure!(
            conflict.is_err_and(|e| e.kind() == io::ErrorKind::AlreadyExists),
            "this filesystem cannot install without replacing: choose another destination"
        );
        rename(c"probe-a", c"probe-c")
            .context("this filesystem cannot install without replacing: choose another destination")?;
        for leaf in [c"probe-b", c"probe-c"] {
            remove(work, leaf)?;
        }
    }
    Ok(())
}

impl Drop for Stage {
    fn drop(&mut self) {
        if !self.keep {
            let _ = remove(&self.fd, &self.leaf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    fn spec(source: &TempDir, destination: &TempDir, item: &str) -> Spec {
        Spec {
            operation: crate::laptop::copy::Operation::Copy,
            to_laptop: true,
            remote: None,
            sources: vec![source.0.join(item).to_str().unwrap().into()],
            destination: destination.0.clone(),
            allow: vec![item.into()],
        }
    }

    fn fill(stage: &mut Stage, spec: &Spec, source: &TempDir) {
        super::super::copy::send(
            &spec.sources,
            &source.0,
            Scope::All,
            |packet| stage.receive(packet).map(|_| ()),
            || Ok(()),
        )
        .unwrap();
    }

    #[test]
    fn native_copy_preserves_tree_links_dates_and_safe_modes_then_cleans_stage() {
        let source = TempDir::new("stage-source");
        let home = TempDir::new("stage-home");
        source.mkdir("tree/empty");
        let file = source.write("tree/.data", b"contents");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o6750)).unwrap();
        symlink(".data", source.0.join("tree/link")).unwrap();
        let before = fs::metadata(&file).unwrap();
        fs::set_permissions(source.0.join("tree"), fs::Permissions::from_mode(0o3770)).unwrap();
        let directory_before = fs::metadata(source.0.join("tree")).unwrap();
        let link_before = fs::symlink_metadata(source.0.join("tree/link")).unwrap();
        let spec = spec(&source, &home, "tree");
        let mut stage = Stage::new(&spec, &home.0, Scope::Home).unwrap();
        let path = stage.path.clone();
        assert_eq!(fs::read_to_string(path.join("format")).unwrap(), "kraal-stage 2\n");
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o700);
        assert!(stage.replacements().is_empty());
        fill(&mut stage, &spec, &source);
        assert!(!home.0.join("tree").exists());
        stage.install(|| Ok(())).unwrap();
        drop(stage);
        assert!(!path.exists());
        assert_eq!(fs::read(home.0.join("tree/.data")).unwrap(), b"contents");
        assert_eq!(fs::read_link(home.0.join("tree/link")).unwrap(), Path::new(".data"));
        assert!(home.0.join("tree/empty").is_dir());
        let after = fs::metadata(home.0.join("tree/.data")).unwrap();
        assert_eq!(after.mode() & 0o7777, 0o750);
        assert_eq!((before.mtime(), before.mtime_nsec()), (after.mtime(), after.mtime_nsec()));
        let directory_after = fs::metadata(home.0.join("tree")).unwrap();
        let link_after = fs::symlink_metadata(home.0.join("tree/link")).unwrap();
        assert_eq!(directory_after.mode() & 0o7777, 0o1770);
        for (before, after) in [(directory_before, directory_after), (link_before, link_after)] {
            assert_eq!((before.mtime(), before.mtime_nsec()), (after.mtime(), after.mtime_nsec()));
        }
        assert!(source.0.join("tree/.data").exists());
    }

    #[test]
    fn nested_edits_and_additions_after_approval_keep_the_previous_tree() {
        for added in [false, true] {
            for after_backup in [false, true] {
                let source = TempDir::new("stage-nested-source");
                let home = TempDir::new("stage-nested-home");
                source.mkdir("project/src");
                source.write("project/src/main.rs", "incoming");
                home.mkdir("project/src");
                home.write("project/src/main.rs", "old work");
                let spec = spec(&source, &home, "project");
                let mut stage = Stage::new(&spec, &home.0, Scope::Home).unwrap();
                let path = stage.path.clone();
                let leaf = if added { "src/new.rs" } else { "src/main.rs" };
                let before = fs::metadata(home.0.join("project")).unwrap();
                let mut changed = false;
                super::super::copy::send(
                    &spec.sources,
                    &source.0,
                    Scope::All,
                    |packet| {
                        if !after_backup && matches!(packet, Packet::Chunk(_)) {
                            fs::write(home.0.join("project").join(leaf), "new work")?;
                            changed = true;
                        }
                        stage.receive(packet).map(|_| ())
                    },
                    || Ok(()),
                )
                .unwrap();
                let error = stage
                    .install(|| {
                        if after_backup && !changed && path.join("work/project").exists() {
                            fs::write(path.join("work/project").join(leaf), "new work").unwrap();
                            changed = true;
                        }
                        Ok(())
                    })
                    .unwrap_err();
                assert!(changed);
                let error = format!("{error:#}");
                assert!(error.contains("project") && error.contains(path.to_str().unwrap()), "{error}");
                drop(stage);
                assert_eq!(fs::read_to_string(home.0.join("project").join(leaf)).unwrap(), "new work");
                assert_eq!(fs::read_to_string(path.join("payload/project/src/main.rs")).unwrap(), "incoming");
                let after = fs::metadata(home.0.join("project")).unwrap();
                assert_eq!((before.mtime(), before.mtime_nsec()), (after.mtime(), after.mtime_nsec()));
            }
        }
    }

    #[test]
    fn cleanup_checks_progress_and_cancellation_and_drop_keeps_the_remaining_backup() {
        for cancel in [false, true] {
            let source = TempDir::new("stage-clean-source");
            let home = TempDir::new("stage-clean-home");
            source.write("project", "incoming");
            home.mkdir("project/src");
            for n in 0..64 {
                home.write(&format!("project/src/{n}"), "previous");
            }
            let spec = spec(&source, &home, "project");
            let mut stage = Stage::new(&spec, &home.0, Scope::Home).unwrap();
            fill(&mut stage, &spec, &source);
            let path = stage.path.clone();
            let mut cleaning = 0;
            let result = stage.install(|| {
                if home.0.join("project").is_file() {
                    cleaning += 1;
                    // Validation and deletion both call back for each descendant.
                    ensure!(!cancel || cleaning < 80, "cleanup cancelled");
                }
                Ok(())
            });
            assert!(cleaning >= 80, "cleanup must keep checking throughout the previous tree");
            assert_eq!(result.is_err(), cancel);
            if let Err(error) = result {
                let error = format!("{error:#}");
                assert!(error.contains("installed") && error.contains("cleanup cancelled"), "{error}");
                assert!(error.contains(path.to_str().unwrap()), "{error}");
            }
            drop(stage);
            assert_eq!(fs::read_to_string(home.0.join("project")).unwrap(), "incoming");
            if cancel {
                assert!(fs::read_dir(path.join("work/project/src")).unwrap().next().is_some());
            } else {
                assert!(!path.exists());
            }
        }
    }

    #[test]
    fn cleanup_errors_are_reported_and_are_not_retried_by_drop() {
        let source = TempDir::new("stage-clean-error-source");
        let home = TempDir::new("stage-clean-error-home");
        source.write("report", "incoming");
        home.write("report", "previous");
        let spec = spec(&source, &home, "report");
        let mut stage = Stage::new(&spec, &home.0, Scope::Home).unwrap();
        fill(&mut stage, &spec, &source);
        stage.put(|| Ok(())).unwrap();
        let path = stage.path.clone();
        // A missing staging name makes the descriptor-relative removal fail.
        let leaf = stage.leaf.clone();
        stage.leaf = CString::new("missing-stage").unwrap();
        let error = stage.cleanup(&mut || Ok(())).unwrap_err();
        assert!(format!("{error:#}").contains("Copy installed, but staging cleanup did not finish"));
        stage.leaf = leaf;
        drop(stage);
        assert_eq!(fs::read_to_string(path.join("work/report")).unwrap(), "previous");
        assert_eq!(fs::read_to_string(home.0.join("report")).unwrap(), "incoming");
    }

    #[test]
    fn cancellation_after_one_replacement_keeps_its_backup() {
        let source = TempDir::new("stage-multiple-source");
        let home = TempDir::new("stage-multiple-home");
        for name in ["first", "second"] {
            source.write(name, "incoming");
            home.write(name, "previous");
        }
        let mut spec = spec(&source, &home, "first");
        spec.sources.push(source.0.join("second").to_str().unwrap().into());
        let mut stage = Stage::new(&spec, &home.0, Scope::Home).unwrap();
        fill(&mut stage, &spec, &source);
        let path = stage.path.clone();
        let error = stage
            .install(|| {
                ensure!(
                    !fs::read(home.0.join("first")).is_ok_and(|bytes| bytes == b"incoming"),
                    "cancelled after first replacement"
                );
                Ok(())
            })
            .unwrap_err();
        assert!(format!("{error:#}").contains(path.to_str().unwrap()));
        drop(stage);
        assert_eq!(fs::read_to_string(home.0.join("first")).unwrap(), "incoming");
        assert_eq!(fs::read_to_string(home.0.join("second")).unwrap(), "previous");
        assert_eq!(fs::read_to_string(path.join("work/first")).unwrap(), "previous");
        assert_eq!(fs::read_to_string(path.join("payload/second")).unwrap(), "incoming");
    }

    #[test]
    fn a_nested_edit_after_installation_is_retained_and_reported_before_cleanup() {
        let source = TempDir::new("stage-late-edit-source");
        let home = TempDir::new("stage-late-edit-home");
        source.write("project", "incoming");
        home.mkdir("project/src");
        home.write("project/src/main.rs", "previous");
        let spec = spec(&source, &home, "project");
        let mut stage = Stage::new(&spec, &home.0, Scope::Home).unwrap();
        fill(&mut stage, &spec, &source);
        let path = stage.path.clone();
        let mut changed = false;
        let error = stage
            .install(|| {
                if !changed && home.0.join("project").is_file() {
                    fs::write(path.join("work/project/src/main.rs"), "new work")?;
                    changed = true;
                }
                Ok(())
            })
            .unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains("project changed") && error.contains(path.to_str().unwrap()), "{error}");
        drop(stage);
        assert_eq!(fs::read_to_string(path.join("work/project/src/main.rs")).unwrap(), "new work");
        assert_eq!(fs::read_to_string(home.0.join("project")).unwrap(), "incoming");
    }

    #[test]
    fn safe_link_chains_with_parent_components_work_in_both_entry_orders() {
        for reverse in [false, true] {
            let source = TempDir::new("stage-order-source");
            let home = TempDir::new("stage-order-home");
            home.write("file", "contents");
            let spec = spec(&source, &home, "tree");
            let mut stage = Stage::new(&spec, &home.0, Scope::Home).unwrap();
            let mut links =
                [("tree/a", Kind::Link("b/../../../file".into())), ("tree/b", Kind::Link("sub/dir".into()))];
            if reverse {
                links.reverse();
            }
            stage.receive(Packet::Begin { bytes: 0, entries: 5 }).unwrap();
            for (path, kind) in
                [("tree", Kind::Directory), ("tree/sub", Kind::Directory), ("tree/sub/dir", Kind::Directory)]
                    .into_iter()
                    .chain(links)
            {
                stage
                    .receive(Packet::Entry(Entry {
                        path: path.into(),
                        kind,
                        size: 0,
                        mode: 0o755,
                        seconds: 0,
                        nanos: 0,
                    }))
                    .unwrap();
                stage.receive(Packet::End).unwrap();
            }
            stage.receive(Packet::Done).unwrap();
            stage.install(|| Ok(())).unwrap();
            assert_eq!(fs::read(home.0.join("tree/a")).unwrap(), b"contents");
            assert_eq!(fs::read_link(home.0.join("tree/a")).unwrap(), Path::new("b/../../../file"));
        }
    }

    #[test]
    fn a_name_taken_during_put_stays_staged_and_is_named() {
        let source = TempDir::new("stage-race-source");
        let home = TempDir::new("stage-race-home");
        source.write("report", "new");
        let spec = spec(&source, &home, "report");
        let mut stage = Stage::new(&spec, &home.0, Scope::Home).unwrap();
        fill(&mut stage, &spec, &source);
        let path = stage.path.clone();
        let mut calls = 0;
        let error = stage
            .install(|| {
                calls += 1;
                if calls == 2 {
                    home.write("report", "arrived meanwhile");
                }
                Ok(())
            })
            .unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains("report") && error.contains(path.to_str().unwrap()), "{error}");
        drop(stage);
        assert_eq!(fs::read_to_string(home.0.join("report")).unwrap(), "arrived meanwhile");
        assert_eq!(fs::read_to_string(path.join("payload/report")).unwrap(), "new");
    }

    #[test]
    fn replacement_rechecks_the_item_moved_aside_and_never_removes_a_racing_link_target() {
        for race in [false, true] {
            let source = TempDir::new("stage-replace-source");
            let home = TempDir::new("stage-replace-home");
            let outside = TempDir::new("stage-replace-outside");
            source.write("report", "new");
            home.write("report", "old");
            let secret = outside.write("secret", "untouched");
            let spec = spec(&source, &home, "report");
            let mut stage = Stage::new(&spec, &home.0, Scope::Home).unwrap();
            assert_eq!(stage.replacements(), ["report"]);
            fill(&mut stage, &spec, &source);
            let path = stage.path.clone();
            let mut calls = 0;
            let result = stage.install(|| {
                calls += 1;
                if race && calls == 2 {
                    fs::rename(home.0.join("report"), home.0.join("original")).unwrap();
                    symlink(&secret, home.0.join("report")).unwrap();
                }
                Ok(())
            });
            assert_eq!(result.is_err(), race);
            drop(stage);
            assert_eq!(fs::read_to_string(&secret).unwrap(), "untouched");
            if race {
                assert_eq!(fs::read_link(home.0.join("report")).unwrap(), secret);
                assert_eq!(fs::read_to_string(path.join("payload/report")).unwrap(), "new");
            } else {
                assert_eq!(fs::read_to_string(home.0.join("report")).unwrap(), "new");
                assert!(!path.exists());
            }
        }
    }

    #[test]
    fn arrival_after_backup_keeps_both_the_new_arrival_and_previous_item() {
        let source = TempDir::new("stage-backup-source");
        let home = TempDir::new("stage-backup-home");
        source.write("report", "new");
        home.write("report", "old");
        let spec = spec(&source, &home, "report");
        let mut stage = Stage::new(&spec, &home.0, Scope::Home).unwrap();
        fill(&mut stage, &spec, &source);
        let path = stage.path.clone();
        let mut calls = 0;
        assert!(
            stage
                .install(|| {
                    calls += 1;
                    if calls == 3 {
                        home.write("report", "racing");
                    }
                    Ok(())
                })
                .is_err()
        );
        drop(stage);
        assert_eq!(fs::read_to_string(home.0.join("report")).unwrap(), "racing");
        assert_eq!(fs::read_to_string(path.join("work/report")).unwrap(), "old");
        assert_eq!(fs::read_to_string(path.join("kind")).unwrap(), "move\n");
        assert_eq!(fs::read_to_string(path.join("payload/report")).unwrap(), "new");
    }

    #[test]
    fn cancellation_removes_partial_data_and_restores_an_approved_replacement() {
        let source = TempDir::new("stage-cancel-source");
        let home = TempDir::new("stage-cancel-home");
        source.write("report", "new");
        home.write("report", "old");
        let spec = spec(&source, &home, "report");
        let mut stage = Stage::new(&spec, &home.0, Scope::Home).unwrap();
        fill(&mut stage, &spec, &source);
        let path = stage.path.clone();
        let mut calls = 0;
        assert!(
            stage
                .install(|| {
                    calls += 1;
                    ensure!(calls != 3, "cancelled");
                    Ok(())
                })
                .is_err()
        );
        drop(stage);
        assert!(!path.exists());
        assert_eq!(fs::read_to_string(home.0.join("report")).unwrap(), "old");
        let mut stage = Stage::new(&spec, &home.0, Scope::Home).unwrap();
        let path = stage.path.clone();
        stage.receive(Packet::Begin { bytes: 4, entries: 1 }).unwrap();
        stage
            .receive(Packet::Entry(Entry {
                path: "report".into(),
                kind: Kind::File,
                size: 4,
                mode: 0o644,
                seconds: 0,
                nanos: 0,
            }))
            .unwrap();
        drop(stage);
        assert!(!path.exists());
    }

    #[test]
    fn swapped_destination_and_staged_directory_links_cannot_redirect_writes() {
        let source = TempDir::new("stage-link-source");
        let home = TempDir::new("stage-link-home");
        let outside = TempDir::new("stage-link-outside");
        source.mkdir("tree");
        source.write("tree/file", "new");
        let spec = spec(&source, &home, "tree");
        let mut stage = Stage::new(&spec, &home.0, Scope::Home).unwrap();
        stage.receive(Packet::Begin { bytes: 3, entries: 2 }).unwrap();
        stage
            .receive(Packet::Entry(Entry {
                path: "tree".into(),
                kind: Kind::Directory,
                size: 0,
                mode: 0o755,
                seconds: 0,
                nanos: 0,
            }))
            .unwrap();
        stage.receive(Packet::End).unwrap();
        fs::remove_dir(stage.path.join("payload/tree")).unwrap();
        symlink(&outside.0, stage.path.join("payload/tree")).unwrap();
        assert!(
            stage
                .receive(Packet::Entry(Entry {
                    path: "tree/file".into(),
                    kind: Kind::File,
                    size: 3,
                    mode: 0o644,
                    seconds: 0,
                    nanos: 0
                }))
                .is_err()
        );
        drop(stage);
        assert!(!outside.0.join("file").exists());
        home.mkdir("destination");
        let mut spec = spec;
        spec.destination = home.0.join("destination");
        let mut stage = Stage::new(&spec, &home.0, Scope::Home).unwrap();
        fill(&mut stage, &spec, &source);
        fs::rename(&spec.destination, home.0.join("moved")).unwrap();
        symlink(&outside.0, &spec.destination).unwrap();
        assert!(stage.install(|| Ok(())).is_err());
        drop(stage);
        assert!(fs::read_dir(home.0.join("moved")).unwrap().next().is_none());
        assert!(fs::read_dir(&outside.0).unwrap().next().is_none());
    }

    #[test]
    fn scope_off_hidden_top_level_and_escaping_links_are_refused() {
        let source = TempDir::new("stage-scope-source");
        let home = TempDir::new("stage-scope-home");
        let spec = spec(&source, &home, ".ssh");
        assert!(Stage::new(&spec, &home.0, Scope::Home).is_err());
        assert!(Stage::new(&spec, &home.0, Scope::Off).is_err());
        let mut spec = spec;
        spec.sources = vec![source.0.join("link").to_str().unwrap().into()];
        for target in ["../outside", ".ssh/key", "Library/secret"] {
            if target.starts_with("Library") && !cfg!(target_os = "macos") {
                continue;
            }
            let mut stage = Stage::new(&spec, &home.0, Scope::Home).unwrap();
            stage.receive(Packet::Begin { bytes: 0, entries: 1 }).unwrap();
            stage
                .receive(Packet::Entry(Entry {
                    path: "link".into(),
                    kind: Kind::Link(target.into()),
                    size: 0,
                    mode: 0o777,
                    seconds: 0,
                    nanos: 0,
                }))
                .unwrap();
            stage.receive(Packet::End).unwrap();
            stage.receive(Packet::Done).unwrap();
            assert!(stage.install(|| Ok(())).is_err());
            assert!(home.0.join("link").symlink_metadata().is_err());
        }
    }

    #[test]
    fn link_targets_are_rechecked_before_installation() {
        let source = TempDir::new("stage-target-source");
        let home = TempDir::new("stage-target-home");
        let outside = TempDir::new("stage-target-outside");
        source.write("target", "source");
        symlink("target", source.0.join("link")).unwrap();
        home.mkdir("target");
        let spec = spec(&source, &home, "link");
        let mut stage = Stage::new(&spec, &home.0, Scope::Home).unwrap();
        fill(&mut stage, &spec, &source);
        fs::remove_dir(home.0.join("target")).unwrap();
        symlink(&outside.0, home.0.join("target")).unwrap();
        assert!(stage.install(|| Ok(())).is_err());
        drop(stage);
        assert!(!home.0.join("link").exists());
    }

    #[test]
    fn unsupported_exclusive_rename_is_refused_before_contents() {
        let home = TempDir::new("stage-probe");
        let fd = super::super::open_directory(&home.0).unwrap();
        probe(&fd).unwrap();
        assert!(fs::read_dir(&home.0).unwrap().next().is_none());
        let error = probe_with(&fd, |_, _| Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP))).unwrap_err();
        assert!(error.to_string().contains("choose another destination"));
        assert!(home.0.join("probe-a").is_dir(), "no source was installed");
    }
    #[test]
    fn copied_link_chains_cannot_hide_an_escape_before_parent_components() {
        let source = TempDir::new("stage-chain-source");
        let home = TempDir::new("stage-chain-home");
        let spec = spec(&source, &home, "tree");
        let mut stage = Stage::new(&spec, &home.0, Scope::Home).unwrap();
        stage.receive(Packet::Begin { bytes: 0, entries: 3 }).unwrap();
        for (path, kind) in
            [("tree", Kind::Directory), ("tree/a", Kind::Link("b/../.ssh".into())), ("tree/b", Kind::Link("..".into()))]
        {
            stage
                .receive(Packet::Entry(Entry { path: path.into(), kind, size: 0, mode: 0o755, seconds: 0, nanos: 0 }))
                .unwrap();
            stage.receive(Packet::End).unwrap();
        }
        stage.receive(Packet::Done).unwrap();
        assert!(stage.install(|| Ok(())).is_err());
        assert!(!home.0.join("tree").exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_reports_the_destination_filesystems_case_behavior() {
        let source = TempDir::new("stage-case-source");
        let home = TempDir::new("stage-case-home");
        home.write("REPORT", "old");
        let spec = spec(&source, &home, "report");
        let fd = super::super::open_directory(&home.0).unwrap();
        let sensitive = case_sensitive(&fd).unwrap();
        let stage = Stage::new(&spec, &home.0, Scope::Home).unwrap();
        assert_eq!(stage.replacements().is_empty(), sensitive);
        drop(stage);
        let mut spec = spec;
        spec.sources.push(source.0.join("REPORT").to_str().unwrap().into());
        assert_eq!(Stage::new(&spec, &home.0, Scope::Home).is_ok(), sensitive);
    }
    #[test]
    fn approved_folders_and_links_are_replaced_as_whole_items_without_following_them() {
        for link in [true, false] {
            let source = TempDir::new("stage-old-source");
            let home = TempDir::new("stage-old-home");
            let outside = TempDir::new("stage-old-outside");
            source.write("report", "new");
            outside.write("secret", "untouched");
            if link {
                symlink(&outside.0, home.0.join("report")).unwrap();
            } else {
                home.mkdir("report/nested");
                home.write("report/nested/old", "old");
            }
            let spec = spec(&source, &home, "report");
            let mut stage = Stage::new(&spec, &home.0, Scope::Home).unwrap();
            assert_eq!(stage.replacements(), ["report"]);
            fill(&mut stage, &spec, &source);
            let path = stage.path.clone();
            stage.install(|| Ok(())).unwrap();
            drop(stage);
            assert!(!path.exists());
            assert_eq!(fs::read_to_string(home.0.join("report")).unwrap(), "new");
            assert_eq!(fs::read_to_string(outside.0.join("secret")).unwrap(), "untouched");
        }
    }
    #[test]
    fn safe_copied_and_existing_link_chains_are_preserved_but_cycles_are_refused() {
        let source = TempDir::new("stage-safe-links-source");
        let home = TempDir::new("stage-safe-links-home");
        let outside = TempDir::new("stage-safe-links-outside");
        outside.write("secret", "untouched");
        symlink(&outside.0, home.0.join("tree")).unwrap();
        source.mkdir("tree");
        source.write("tree/data", "contents");
        symlink("./data", source.0.join("tree/b")).unwrap();
        symlink("b", source.0.join("tree/a")).unwrap();
        let spec = spec(&source, &home, "tree");
        let mut stage = Stage::new(&spec, &home.0, Scope::Home).unwrap();
        fill(&mut stage, &spec, &source);
        stage.install(|| Ok(())).unwrap();
        drop(stage);
        assert_eq!(fs::read(home.0.join("tree/a")).unwrap(), b"contents");
        assert_eq!(fs::read(outside.0.join("secret")).unwrap(), b"untouched");
        assert_eq!(fs::read_link(home.0.join("tree/a")).unwrap(), Path::new("b"));
        let mut spec = spec;
        spec.sources = vec![source.0.join("link").to_str().unwrap().into()];
        for target in ["tree/a", "link"] {
            let mut stage = Stage::new(&spec, &home.0, Scope::Home).unwrap();
            stage.receive(Packet::Begin { bytes: 0, entries: 1 }).unwrap();
            let result = stage.receive(Packet::Entry(Entry {
                path: "link".into(),
                kind: Kind::Link(target.into()),
                size: 0,
                mode: 0o777,
                seconds: 0,
                nanos: 0,
            }));
            result.unwrap();
            stage.receive(Packet::End).unwrap();
            stage.receive(Packet::Done).unwrap();
            if target == "link" {
                assert!(stage.install(|| Ok(())).unwrap_err().to_string().contains("loop"));
                assert_eq!(fs::read(home.0.join("link")).unwrap(), b"contents");
            } else {
                stage.install(|| Ok(())).unwrap();
                assert_eq!(fs::read(home.0.join("link")).unwrap(), b"contents");
            }
        }
    }
    #[test]
    fn malformed_link_paths_are_errors_and_a_link_swap_during_put_cannot_escape() {
        let source = TempDir::new("stage-link-path-source");
        let home = TempDir::new("stage-link-path-home");
        let outside = TempDir::new("stage-link-path-outside");
        let spec = spec(&source, &home, "link");
        for path in ["/", "", "../escape", "dir/../escape"] {
            let mut stage = Stage::new(&spec, &home.0, Scope::Home).unwrap();
            stage.receive(Packet::Begin { bytes: 0, entries: 1 }).unwrap();
            assert!(
                stage
                    .receive(Packet::Entry(Entry {
                        path: path.into(),
                        kind: Kind::Link("target".into()),
                        size: 0,
                        mode: 0o777,
                        seconds: 0,
                        nanos: 0
                    }))
                    .is_err()
            );
        }
        home.mkdir("target");
        let mut stage = Stage::new(&spec, &home.0, Scope::Home).unwrap();
        stage.receive(Packet::Begin { bytes: 0, entries: 1 }).unwrap();
        stage
            .receive(Packet::Entry(Entry {
                path: "link".into(),
                kind: Kind::Link("target".into()),
                size: 0,
                mode: 0o777,
                seconds: 0,
                nanos: 0,
            }))
            .unwrap();
        stage.receive(Packet::End).unwrap();
        stage.receive(Packet::Done).unwrap();
        let path = stage.path.clone();
        let mut calls = 0;
        assert!(
            stage
                .install(|| {
                    calls += 1;
                    if calls == 3 {
                        fs::remove_dir(home.0.join("target")).unwrap();
                        symlink(&outside.0, home.0.join("target")).unwrap();
                    }
                    Ok(())
                })
                .is_err()
        );
        drop(stage);
        assert!(fs::symlink_metadata(home.0.join("link")).is_err());
        assert_eq!(fs::read_link(path.join("payload/link")).unwrap(), Path::new("target"));
    }
}
