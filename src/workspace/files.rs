use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::{fs, io};

use anyhow::{Context as _, Result};

use super::model::{ImportPreview, WorkspaceDef};
use super::read::parse_workspace;
use super::write::to_kdl;

#[cfg(test)]
thread_local! {
    pub(crate) static SOURCE_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static SOURCE_LOADS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl ImportPreview {
    pub fn read(path: &Path) -> Result<Self> {
        let path = definition_path(path);
        let revision = crate::paths::FileRevision::read(&path)?;
        let source =
            std::str::from_utf8(revision.bytes.as_deref().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, "File missing. Choose a KDL file.")
            })?)?;
        let name = path.file_stem().unwrap_or_default().to_string_lossy();
        let definition = parse_workspace(&name, source).with_context(|| format!("parsing {}", path.display()))?;
        Ok(Self { path, revision, definition, accounts: Vec::new() })
    }

    pub fn summary(&self) -> String {
        let mut text = format!(
            "{} tab{} · {} pane{}\nCommands wait for you to start them.",
            self.definition.tabs.len(),
            if self.definition.tabs.len() == 1 { "" } else { "s" },
            self.definition.panes().len(),
            if self.definition.panes().len() == 1 { "" } else { "s" }
        );
        for warning in &self.definition.warnings {
            write!(text, "\n{warning}").unwrap();
        }
        text
    }
}
/// Whether `name` can name a definition file (`<name>.kdl`) inside the definitions directory:
/// one plain path component, so that it never reaches another directory, and no control
/// character (a newline would split the list of saved workspaces).
pub fn valid_file_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && !name.starts_with('.')
        && !name.contains(['/', '\\', '\0'])
        && !name.chars().any(char::is_control)
        && Path::new(name).components().count() == 1
}

/// The file name a workspace is saved under when none is given: its name when that is already
/// one plain name ([`valid_file_name`]). A group running a command is named after the whole
/// command, which may hold a `/` or be long: those become `_`, a name past 200 bytes is cut, and
/// whenever the name had to change, a digest of the whole is added, so that two workspaces never
/// share a file (`tail /var/log/a_b` and `tail /var/log/a/b` would otherwise both be
/// `tail _var_log_a_b`).
pub fn file_name_from(name: &str) -> String {
    if valid_file_name(name) && name.trim() == name {
        return name.to_string();
    }
    let replaced: String =
        name.chars().map(|c| if matches!(c, '/' | '\\' | '\0') || c.is_control() { '_' } else { c }).collect();
    // Trimmed first: a dot the spaces hid must not start the file name.
    let trimmed = replaced.trim();
    let out = match trimmed.strip_prefix('.') {
        Some(rest) => format!("_{rest}"),
        None if trimmed.is_empty() => "workspace".to_string(),
        None => trimmed.to_string(),
    };
    let digest = format!("-{:016x}", crate::text::fnv64(name));
    let mut cut = out.len().min(200 - digest.len());
    while !out.is_char_boundary(cut) {
        cut -= 1;
    }
    let file = format!("{}{digest}", &out[..cut]);
    if valid_file_name(&file) { file } else { format!("workspace{digest}") }
}

/// Load every `*.kdl` in `dir`, sorted by name, each listed under its [`definition_path`].
/// Unparseable files are returned as `Err` entries with the path.
///
/// A missing directory yields an empty list; any other error reading it yields a
/// single `Err` entry for `dir` itself.
pub fn load_dir(dir: &Path) -> Vec<(PathBuf, Result<WorkspaceDef>)> {
    #[cfg(test)]
    SOURCE_SCANS.with(|n| n.set(n.get() + 1));
    let dir = &fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            let err = anyhow::Error::new(e).context(format!("reading {}", dir.display()));
            return vec![(dir.to_path_buf(), Err(err))];
        }
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "kdl") && path.is_file())
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            let ws = load_file(&path);
            (path, ws)
        })
        .collect()
}

pub use crate::paths::normalized_entry as definition_path;

/// The definition in the file at `path`, named after the file.
pub fn load_file(path: &Path) -> Result<WorkspaceDef> {
    #[cfg(test)]
    SOURCE_LOADS.with(|n| n.set(n.get() + 1));
    let src = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let name = path.file_stem().map(|stem| stem.to_string_lossy().into_owned()).unwrap_or_default();
    parse_workspace(&name, &src).with_context(|| format!("parsing {}", path.display()))
}
/// Save the revision that was loaded or confirmed, keeping the replaced file beside it.
pub fn save_file(path: &Path, def: &WorkspaceDef, expected: &crate::paths::FileRevision) -> Result<String> {
    let source = to_kdl(def);
    crate::paths::write_checked(path, source.as_bytes(), expected, &expected.target.with_extension("kdl.bak"))
        .with_context(|| format!("saving {}", path.display()))?;
    Ok(source)
}

/// Ownership must stay stable through publication: called by the server, or under the
/// stopped session's lock. A file revision alone cannot detect a workspace being opened.
pub fn publish_import<'a>(
    path: &Path,
    source: &str,
    expected: &crate::paths::FileRevision,
    sources: impl Iterator<Item = (&'a str, &'a Path)>,
) -> Result<()> {
    for (name, owned) in sources {
        anyhow::ensure!(
            !(crate::paths::same_file(owned, path)
                || expected.bytes.is_some()
                    && crate::paths::same_file(owned, &expected.target.with_extension("kdl.bak"))),
            "destination belongs to workspace {name:?}; choose another file name"
        );
    }
    parse_workspace(&path.file_stem().unwrap_or_default().to_string_lossy(), source)?;
    crate::paths::write_checked(path, source.as_bytes(), expected, &expected.target.with_extension("kdl.bak"))?;
    Ok(())
}
