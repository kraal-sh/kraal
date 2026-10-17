use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
struct Stamp {
    device: u64,
    inode: u64,
    size: u64,
    modified: (i64, i64),
    mode: u32,
}

fn stamp(path: &Path) -> io::Result<Option<Stamp>> {
    match fs::metadata(path) {
        Ok(m) => Ok(Some(Stamp {
            device: m.dev(),
            inode: m.ino(),
            size: m.len(),
            modified: (m.mtime(), m.mtime_nsec()),
            mode: m.mode(),
        })),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn listing(path: &Path) -> io::Result<Vec<OsString>> {
    let entries = match fs::read_dir(path) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut names = entries.map(|e| e.map(|e| e.file_name())).collect::<io::Result<Vec<_>>>()?;
    names.sort();
    Ok(names)
}

/// Whether reading again may succeed with unchanged inputs: invalid contents wait for a change.
pub(super) fn retryable(error: &anyhow::Error) -> bool {
    error.chain().any(|e| e.is::<io::Error>())
}

/// Captured before reading, so a write during a scan cannot become its baseline.
#[derive(Default)]
pub(super) struct Inputs {
    workspace_dirs: Vec<PathBuf>,
    sources: BTreeSet<PathBuf>,
    files: BTreeMap<PathBuf, io::Result<Option<Stamp>>>,
    directories: BTreeMap<PathBuf, io::Result<Vec<OsString>>>,
}

impl Inputs {
    pub(super) fn configuration() -> Self {
        let mut inputs = Self::default();
        inputs.file(&crate::paths::config_path());
        inputs.file(&crate::accounts::Accounts::path());
        inputs.file(&crate::servers::Inventory::path());
        inputs
    }

    pub(super) fn workspace_paths(&mut self, directories: &[PathBuf], sources: impl Iterator<Item = PathBuf>) {
        self.workspace_dirs = directories.to_vec();
        self.sources = sources.collect();
    }

    pub(super) fn scope_changed(&self, server: &super::Server) -> bool {
        self.workspace_dirs != server.config.workspace_dirs()
            || self.sources != server.workspaces.iter().filter_map(|w| w.definition.clone()).collect()
    }

    pub(super) fn follow_sources(&mut self, server: &super::Server) {
        // The applied scan may have followed a rename using the old source's inode.
        self.sources = server.workspaces.iter().filter_map(|w| w.definition.clone()).collect();
    }

    pub(super) fn file(&mut self, path: &Path) {
        self.files.entry(path.to_path_buf()).or_insert_with(|| stamp(path));
    }

    pub(super) fn directory(&mut self, path: &Path) {
        self.file(path);
        self.directories.entry(path.to_path_buf()).or_insert_with(|| listing(path));
    }

    pub(super) fn workspace_directory(&mut self, path: &Path) {
        self.directory(path);
        let files: Vec<_> = self.directories[path]
            .as_ref()
            .into_iter()
            .flatten()
            .map(|name| path.join(name))
            .filter(|p| p.extension().is_some_and(|ext| ext == "kdl"))
            .collect();
        for file in files {
            self.file(&file);
        }
    }

    /// Errors retry on a later tick, without making an unreadable path spin the loop.
    pub(super) fn changed(&self) -> io::Result<bool> {
        let mut changed = false;
        for (path, before) in &self.files {
            changed |= *before.as_ref().map_err(|e| io::Error::new(e.kind(), e.to_string()))? != stamp(path)?;
        }
        for (path, before) in &self.directories {
            changed |= *before.as_ref().map_err(|e| io::Error::new(e.kind(), e.to_string()))? != listing(path)?;
        }
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::TempDir;
    use std::time::{Duration, UNIX_EPOCH};

    fn modified(path: &Path, nanos: u64) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(1_700_000_000) + Duration::from_nanos(nanos))
            .unwrap();
    }

    #[test]
    fn metadata_detects_nanoseconds_size_inode_and_presence() {
        let dir = TempDir::new("scan-stamps");
        let path = dir.write("input", "first");
        modified(&path, 100);
        let mut inputs = Inputs::default();
        inputs.file(&path);
        assert!(!inputs.changed().unwrap());
        fs::write(&path, "other").unwrap();
        modified(&path, 200);
        assert!(inputs.changed().unwrap(), "same size, same second, different nanoseconds");
        modified(&path, 100);
        assert!(!inputs.changed().unwrap(), "the check does not read contents");
        fs::write(&path, "longer").unwrap();
        modified(&path, 100);
        assert!(inputs.changed().unwrap(), "size changed");
        let replacement = dir.write("replacement", "first");
        modified(&replacement, 100);
        fs::rename(&replacement, &path).unwrap();
        assert!(inputs.changed().unwrap(), "inode changed with size and time preserved");
        let mut inputs = Inputs::default();
        inputs.file(&path);
        fs::remove_file(&path).unwrap();
        assert!(inputs.changed().unwrap());
        let mut inputs = Inputs::default();
        inputs.file(&path);
        assert!(!inputs.changed().unwrap(), "missing files have a stable baseline");
        fs::write(&path, "first").unwrap();
        assert!(inputs.changed().unwrap());
    }

    #[test]
    fn directory_listings_detect_renames_even_with_the_same_metadata() {
        let dir = TempDir::new("scan-listing");
        let path = dir.mkdir("definitions");
        fs::write(path.join("first.kdl"), "layout { pane; }").unwrap();
        let mut inputs = Inputs::default();
        inputs.directory(&path);
        fs::rename(path.join("first.kdl"), path.join("other.kdl")).unwrap();
        // Ignore the directory stamp to check the listing independently.
        inputs.files.clear();
        assert!(inputs.changed().unwrap());
    }

    #[test]
    fn metadata_errors_never_become_an_unchanged_baseline() {
        let dir = TempDir::new("scan-error");
        let path = dir.0.join("loop");
        std::os::unix::fs::symlink(&path, &path).unwrap();
        let mut inputs = Inputs::default();
        inputs.file(&path);
        assert!(inputs.changed().is_err());
        assert!(inputs.changed().is_err());
        fs::remove_file(&path).unwrap();
        assert!(inputs.changed().is_err(), "a failed baseline must be retried after recovery too");
    }
}
