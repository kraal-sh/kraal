//! Preparing a new worktree for its agent: files copied from the main checkout and setup
//! commands run in it before the agent starts (a repository's `.kraal/worktree.toml`, and
//! `[worktrees]` in the config), and the variables its panes get, so that worktrees of the same
//! repository can run the same dev server side by side.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

use crate::config::WorktreesConfig;
use crate::git::{self, Worktree};
use crate::text::numbered;
use crate::{paths, shell};

/// Where a repository says how to prepare its worktrees. It is read from the commit a new
/// worktree starts from: what that worktree checks out.
const FILE: &str = ".kraal/worktree.toml";
const FILE_MAX: usize = 64 << 10;

/// The variables of a worktree's panes.
const VARS: [&str; 4] = ["KRAAL_WORKTREE", "KRAAL_WORKTREE_INDEX", "KRAAL_PORT_OFFSET", "PORT"];

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Setup {
    /// Paths in the repository copied from the main checkout when they are there; `*` and `?`
    /// in the last part.
    pub copy: Vec<String>,
    /// Shell commands run in the new worktree, in order; the first that fails stops the rest.
    pub setup: Vec<String>,
}

impl Setup {
    pub fn parse(text: &str) -> Result<Setup> {
        let s: Setup = toml::from_str(text)?;
        s.check()?;
        Ok(s)
    }

    /// The user's own, from the config.
    pub fn of_user(cfg: &WorktreesConfig) -> Setup {
        Setup { copy: cfg.copy.clone(), setup: cfg.setup.clone() }
    }

    pub fn check(&self) -> Result<()> {
        ensure!(self.copy.len() <= 256, "copy: at most 256 paths");
        self.copy.iter().try_for_each(|p| check_pattern(p))
    }

    fn commands(&self) -> impl Iterator<Item = &String> {
        self.setup.iter().filter(|c| !c.trim().is_empty())
    }

    /// Whether it asks for nothing: no file to copy, no command to run.
    fn is_empty(&self) -> bool {
        self.copy.is_empty() && self.commands().next().is_none()
    }

    /// What a repository's file says. The error for one that cannot be read quotes it: with
    /// what a terminal would act on written out, as for all that kraal shows of the file.
    fn parse_repo(text: &str) -> Result<Setup> {
        Setup::parse(text).map_err(|e| anyhow::anyhow!(visible_lines(&format!("{e:#}"))))
    }
}

/// What `kraal config check` says of the repository's file as the checkout at `root` has it: an
/// error when kraal would refuse to prepare a worktree from it. A checkout without one is fine.
pub fn check_repo_file(root: &Path) -> Result<()> {
    let path = root.join(FILE);
    match paths::read_regular_text(&path, FILE_MAX as u64) {
        Ok(text) => Setup::parse_repo(&text).map(drop).with_context(|| format!("in {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// A repository's own setup file as of the commit a new worktree starts from (what that
/// worktree checks out): what kraal shows, what the user approves, and what a worktree is
/// then prepared from. It comes with the repository, so kraal acts on none of it (neither its
/// commands nor the files it names to copy) before this version of it is approved, which is an
/// act of its own, like a repository runbook's ([`crate::runbooks::trust_content`]).
#[derive(Debug, Clone)]
pub struct RepoFile {
    /// The main checkout.
    pub repo: PathBuf,
    pub base: String,
    /// The file's text and what it says; `None` when that commit has no such file.
    file: Option<(String, Setup)>,
}

impl RepoFile {
    /// The file of the repository containing `cwd`, as of `base` (by default the current
    /// branch).
    pub fn read(cwd: &Path, base: Option<&str>) -> Result<RepoFile> {
        let (repo, base) = git::start_point(cwd, base)?;
        RepoFile::at(repo, base)
    }

    /// The file of the repository whose main checkout is `repo`, as of `base`.
    fn at(repo: PathBuf, base: String) -> Result<RepoFile> {
        let file = match git::file_at(&repo, &base, FILE, FILE_MAX)? {
            Some(text) => {
                let setup = Setup::parse_repo(&text).with_context(|| format!("in {FILE} of {base}"))?;
                Some((text, setup))
            }
            None => None,
        };
        Ok(RepoFile { repo, base, file })
    }

    fn setup(&self) -> Option<&Setup> {
        self.file.as_ref().map(|(_, setup)| setup)
    }

    /// Whether kraal may act on it: it asks for nothing, or the user approved this version.
    pub fn approved(&self) -> bool {
        let Some((text, setup)) = &self.file else { return true };
        setup.is_empty() || crate::runbooks::trusted_content(&self.repo, Path::new(FILE), text)
    }

    /// Approve this version of it. `false` when the repository has no such file.
    pub fn trust(&self) -> Result<bool> {
        let Some((text, _)) = &self.file else { return Ok(false) };
        crate::runbooks::trust_content(&self.repo, Path::new(FILE), text)?;
        Ok(true)
    }

    /// Where it is, as said to the user: `~/src/app/.kraal/worktree.toml as of main`.
    pub fn place(&self) -> String {
        format!("{} as of {}", paths::shorten_home(&self.repo.join(FILE)), visible(&self.base))
    }

    /// Its text as a terminal shows it to be read ([`visible_lines`]), if it is there.
    pub fn shown(&self) -> Option<String> {
        self.file.as_ref().map(|(text, _)| visible_lines(text))
    }

    /// Why kraal does not act on it, and how to approve it: said where nobody can be asked
    /// (`--yes`, no terminal). `base` is the branch given to the command refused, which the
    /// commands named must be given too.
    pub fn not_approved(&self, base: Option<&str>) -> String {
        let base = base.map(|b| format!(" --base {}", shell::quote(b))).unwrap_or_default();
        let (show, trust) = (format!("kraal wt setup{base}"), format!("kraal wt trust{base}"));
        crate::runbooks::refusal("the worktree setup", &self.repo, &show, &trust)
    }

    /// The question asked before kraal acts on it: every command, in full, and what it copies,
    /// with what a terminal would act on rather than show written out (see [`visible`]).
    pub fn trust_question(&self, worktree: &str) -> String {
        let mut lines = vec![
            format!("Trust the worktree setup of the repository {}?", visible(&paths::shorten_home(&self.repo))),
            format!("Its {FILE} is new, or changed since you approved it. For the new worktree {worktree}:"),
        ];
        let Some(setup) = self.setup() else { return lines.join("\n") };
        if !setup.copy.is_empty() {
            let copy: Vec<String> = setup.copy.iter().map(|p| visible(p)).collect();
            lines.push(format!("it copies from the main checkout: {}", copy.join(", ")));
        }
        if setup.commands().next().is_some() {
            lines.push("it runs there:".into());
        }
        for (i, command) in setup.commands().enumerate() {
            let shown: Vec<String> = command.trim_end_matches('\n').split('\n').map(visible).collect();
            lines.extend(numbered(i + 1, &shown.join("\n")));
        }
        lines.join("\n")
    }
}

/// A path in the repository, with wildcards in its last part only.
fn check_pattern(p: &str) -> Result<()> {
    ensure!(p.len() <= 1024, "copy: {:?}…: longer than 1024 bytes", p.chars().take(40).collect::<String>());
    let path = Path::new(p);
    let inside = !p.is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)));
    ensure!(inside, "copy: {p:?} is not a path in the repository");
    let dir = path.parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
    ensure!(!dir.contains(['*', '?']), "copy: {p:?}: * and ? only in the last part");
    Ok(())
}

/// How a new worktree is made and prepared, decided before anything is created: the
/// repository's file is approved first.
#[derive(Debug, Clone)]
pub struct Plan {
    pub name: String,
    /// The repository, the commit the worktree starts from, and the repository's file as of
    /// that commit (not read for a worktree left as git makes it).
    pub from: RepoFile,
    /// The user's own (trusted).
    pub user: Setup,
}

impl Plan {
    /// Plan worktree `name` of the repository containing `cwd`, from `base` (by default the
    /// current branch), prepared as the repository and `user` say; or, without `prepare`, left
    /// as git makes it (neither is read then: a broken file does not stand in the way).
    pub fn new(cwd: &Path, name: &str, base: Option<&str>, user: Setup, prepare: bool) -> Result<Plan> {
        ensure!(git::valid_worktree_name(name), "invalid worktree name {name:?} (letters, digits, - _ .)");
        if prepare {
            user.check().context("in [worktrees] of the config")?;
        }
        let (repo, base) = git::start_point(cwd, base)?;
        let path = git::worktrees_dir(&repo).join(name);
        ensure!(!path.exists(), "{} already exists", path.display());
        if !prepare {
            let from = RepoFile { repo, base, file: None };
            return Ok(Plan { name: name.to_string(), from, user: Setup::default() });
        }
        Ok(Plan { name: name.to_string(), from: RepoFile::at(repo, base)?, user })
    }

    /// Whether the repository's file is still to be approved: kraal copies nothing it names and
    /// runs none of its commands before.
    pub fn needs_trust(&self) -> bool {
        !self.from.approved()
    }

    /// Approve this version of the repository's file.
    pub fn trust(&self) -> Result<()> {
        self.from.trust().map(drop)
    }

    /// The question asked before the repository's file is acted on ([`RepoFile::trust_question`]).
    pub fn trust_question(&self) -> String {
        self.from.trust_question(&self.name)
    }

    /// What is copied: the user's patterns, then the repository's.
    pub fn copy(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for p in self.user.copy.iter().chain(self.from.setup().map(|s| &s.copy).into_iter().flatten()) {
            if !out.contains(p) {
                out.push(p.clone());
            }
        }
        out
    }

    /// What runs: the repository's commands, then the user's.
    pub fn steps(&self) -> Vec<String> {
        self.from.setup().into_iter().flat_map(Setup::commands).chain(self.user.commands()).cloned().collect()
    }

    /// Create the worktree and copy the files into it; the commands are left to run there.
    pub fn create(&self) -> Result<Prepared> {
        let worktree = git::add_worktree(&self.from.repo, &self.name, Some(&self.from.base))?;
        let copied = copy_files(&self.from.repo, &worktree.path, &self.copy());
        Ok(Prepared { worktree, copied, steps: self.steps() })
    }
}

/// `s` as shown to be approved: control characters (which a terminal acts on: an escape
/// sequence, a carriage return that writes over the line) and the invisible ones that hide or
/// reorder text (zero-width, bidirectional) written as `\u{1b}`, `\r`…, so that what is shown
/// is what runs.
pub fn visible(s: &str) -> String {
    let hidden = |c: char| {
        c.is_control()
            || matches!(c, '\u{ad}' | '\u{61c}' | '\u{180e}' | '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}')
            || matches!(c, '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206f}' | '\u{feff}' | '\u{fff9}'..='\u{fffb}')
            || matches!(c, '\u{e0000}'..='\u{e007f}')
    };
    s.chars().map(|c| if hidden(c) { c.escape_default().to_string() } else { c.to_string() }).collect()
}

/// [`visible`] for text of several lines (a file, an error that quotes one): the lines stay
/// lines, a CR LF ends one like an LF.
pub fn visible_lines(text: &str) -> String {
    let lines = text.split('\n').map(|line| visible(line.strip_suffix('\r').unwrap_or(line)));
    lines.collect::<Vec<_>>().join("\n")
}

/// A worktree just created, its files copied, its commands still to run.
#[derive(Debug)]
pub struct Prepared {
    pub worktree: Worktree,
    copied: Copied,
    pub steps: Vec<String>,
}

impl Prepared {
    /// The script that prepares the worktree in its pane, before its agent: what was copied,
    /// then the commands, stopping at the first that fails. `None` when there is nothing to do
    /// or say.
    pub fn script(&self) -> Option<String> {
        let notes = self.copied.lines();
        if notes.is_empty() && self.steps.is_empty() {
            return None;
        }
        let mut s = String::new();
        for line in notes {
            s.push_str(&format!("printf '\\033[2mkraal: %s\\033[0m\\n' {}\n", shell::quote(&line)));
        }
        if !self.steps.is_empty() {
            s.push_str(&crate::runbooks::script(&self.steps, true));
        }
        Some(s)
    }

    /// Run [`Prepared::script`] here, in this terminal, with the worktree's variables.
    pub fn run_here(&self, env: &[(String, String)]) -> Result<()> {
        let Some(script) = self.script() else { return Ok(()) };
        let status = std::process::Command::new("sh")
            .arg("-c")
            .arg(&script)
            .current_dir(&self.worktree.path)
            .envs(env.iter().map(|(k, v)| (k, v)))
            .status()
            .context("running the setup")?;
        ensure!(
            status.success(),
            "the setup of {} failed{}; the worktree is at {}",
            self.worktree.name(),
            status.code().map(|c| format!(" (exit {c})")).unwrap_or_default(),
            self.worktree.path.display()
        );
        Ok(())
    }
}

/// What copying did.
#[derive(Debug, Default, PartialEq, Eq)]
struct Copied {
    copied: Vec<String>,
    /// Paths left out, and why.
    skipped: Vec<(String, String)>,
}

impl Copied {
    /// What to say of it. The names are the repository's files, which no approval showed (a
    /// pattern stands for them): they are written with nothing for a terminal to act on.
    fn lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        if !self.copied.is_empty() {
            let names: Vec<String> = self.copied.iter().map(|name| visible(name)).collect();
            lines.push(format!("copied from the main checkout: {}", names.join(", ")));
        }
        lines.extend(self.skipped.iter().map(|(p, why)| format!("not copied: {} ({})", visible(p), visible(why))));
        lines
    }
}

/// Copy what `patterns` name in the main checkout `from` into the new worktree `to`. A path
/// that is not there is nothing to copy; one already in the worktree stays as it is. Whatever
/// is copied must be in the repository once its links are resolved, and nothing is written
/// through a link of the worktree (the repository may commit one where a copy goes). Links
/// inside a copied directory are left out.
fn copy_files(from: &Path, to: &Path, patterns: &[String]) -> Copied {
    let mut out = Copied::default();
    let (root, dest) = match (from.canonicalize(), to.canonicalize()) {
        (Ok(r), Ok(d)) => (r, d),
        (Err(e), _) | (_, Err(e)) => {
            out.skipped.extend(patterns.iter().map(|p| (p.clone(), e.to_string())));
            return out;
        }
    };
    for pattern in patterns {
        if let Err(e) = check_pattern(pattern) {
            out.skipped.push((pattern.clone(), format!("{e}")));
            continue;
        }
        for rel in matches(&root, pattern) {
            if out.copied.contains(&rel) {
                continue;
            }
            match copy_path(&root, &dest, Path::new(&rel), &mut out.skipped) {
                Ok(true) => out.copied.push(rel),
                Ok(false) => {}
                Err(e) => out.skipped.push((rel, e.to_string())),
            }
        }
    }
    out
}

/// The paths `pattern` names in `root`: itself when it is there, or with wildcards the
/// matching names of its directory, in order.
fn matches(root: &Path, pattern: &str) -> Vec<String> {
    let path = Path::new(pattern);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    if !name.contains(['*', '?']) {
        let there = root.join(path).symlink_metadata().is_ok();
        return if there { vec![path.display().to_string()] } else { Vec::new() };
    }
    let dir = path.parent().unwrap_or(Path::new(""));
    let Ok(entries) = std::fs::read_dir(root.join(dir)) else { return Vec::new() };
    let mut found: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_str()?.to_string();
            glob(name.as_bytes(), n.as_bytes()).then(|| dir.join(&n).display().to_string())
        })
        .collect();
    found.sort();
    found
}

/// Whether `name` matches `pattern`, where `*` is any run of characters and `?` any one. At
/// most one pass over `name` per character of `pattern`, whatever the pattern: when a match
/// fails, only the last `*` takes one more character.
fn glob(pattern: &[u8], name: &[u8]) -> bool {
    let (mut p, mut n) = (0, 0);
    // After the last `*` seen: where the pattern goes on, and where in `name` it was tried.
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        match pattern.get(p) {
            Some(b'*') => {
                star = Some((p + 1, n));
                p += 1;
            }
            Some(&c) if c == b'?' || c == name[n] => {
                p += 1;
                n += 1;
            }
            _ => match star {
                Some((after, tried)) => {
                    star = Some((after, tried + 1));
                    (p, n) = (after, tried + 1);
                }
                None => return false,
            },
        }
    }
    pattern[p..].iter().all(|&c| c == b'*')
}

/// Copy `rel` from the main checkout `root` into the worktree `dest` (both resolved). False
/// when it was left out (`skipped` says why).
fn copy_path(root: &Path, dest: &Path, rel: &Path, skipped: &mut Vec<(String, String)>) -> std::io::Result<bool> {
    let shown = rel.display().to_string();
    let mut skip = |why: &str| {
        skipped.push((shown.clone(), why.to_string()));
        Ok(false)
    };
    let real = root.join(rel).canonicalize()?;
    if !real.starts_with(root) || real == root {
        return skip("a link out of the repository");
    }
    if real.starts_with(root.join(".git")) {
        return skip("git's own files");
    }
    let meta = std::fs::metadata(&real)?;
    let target = dest_path(dest, rel)?;
    if meta.is_dir() {
        copy_dir(&real, &target, &shown, skipped)?;
        return Ok(true);
    }
    if !meta.is_file() {
        return skip("not a file");
    }
    if target.symlink_metadata().is_ok() {
        return skip("already in the worktree");
    }
    copy_file(&real, &target, &meta)?;
    Ok(true)
}

/// Where `rel` goes in the worktree `dest`, its directories made on the way; none of them may be
/// a link.
fn dest_path(dest: &Path, rel: &Path) -> std::io::Result<PathBuf> {
    let parts: Vec<Component> = rel.components().collect();
    let mut at = dest.to_path_buf();
    for (i, part) in parts.iter().enumerate() {
        at.push(part);
        if i + 1 == parts.len() {
            break;
        }
        match at.symlink_metadata() {
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Err(std::io::Error::other(format!("{} is not a directory", at.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir(&at)?,
            Err(e) => return Err(e),
        }
    }
    Ok(at)
}

/// A new file at `target` with the content and permissions of `src`; never over something
/// there, nor through a link.
fn copy_file(src: &Path, target: &Path, meta: &std::fs::Metadata) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true).mode(meta.permissions().mode() & 0o777);
    let mut out = opts.open(target)?;
    std::io::copy(&mut std::fs::File::open(src)?, &mut out)?;
    Ok(())
}

/// Copy directory `src` into `target`, adding what `target` does not have yet.
fn copy_dir(src: &Path, target: &Path, shown: &str, skipped: &mut Vec<(String, String)>) -> std::io::Result<()> {
    match target.symlink_metadata() {
        Ok(m) if m.is_dir() => {}
        Ok(_) => {
            skipped.push((shown.to_string(), "already in the worktree".into()));
            return Ok(());
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::create_dir(target)?,
        Err(e) => return Err(e),
    }
    let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(src)?.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let (from, to) = (e.path(), target.join(e.file_name()));
        let inner = format!("{shown}/{}", e.file_name().to_string_lossy());
        let meta = from.symlink_metadata()?;
        if meta.file_type().is_symlink() {
            skipped.push((inner, "a link".into()));
        } else if meta.is_dir() {
            copy_dir(&from, &to, &inner, skipped)?;
        } else if meta.is_file() && to.symlink_metadata().is_err() {
            copy_file(&from, &to, &meta)?;
        }
    }
    Ok(())
}

/// The variables of the panes of worktree `name`, number `index`: none without a number.
pub fn env(name: &str, index: u32, cfg: &WorktreesConfig) -> Vec<(String, String)> {
    if index == 0 {
        return Vec::new();
    }
    let offset = index.saturating_mul(u32::from(cfg.port_step));
    let mut env = vec![
        ("KRAAL_WORKTREE".to_string(), name.to_string()),
        ("KRAAL_WORKTREE_INDEX".to_string(), index.to_string()),
        ("KRAAL_PORT_OFFSET".to_string(), offset.to_string()),
    ];
    let port = cfg.port_base.and_then(|b| u32::from(b).checked_add(offset)).and_then(|p| u16::try_from(p).ok());
    if let Some(port) = port {
        env.push(("PORT".to_string(), port.to_string()));
    }
    env
}

/// Keep the worktree variables of a pane's spec in the one that replaces it (an agent resumed,
/// or moved to another account).
pub fn carry_env(from: &[(String, String)], to: &mut Vec<(String, String)>) {
    for (k, v) in from {
        if VARS.contains(&k.as_str()) && !to.iter().any(|(t, _)| t == k) {
            to.push((k.clone(), v.clone()));
        }
    }
}

/// Whether `dir` is where kraal puts worktrees (`<repo>.worktrees/<name>`, or below it).
pub fn in_worktrees_dir(dir: &Path) -> bool {
    dir.ancestors().filter_map(|a| a.parent()?.file_name()).any(|n| n.to_string_lossy().ends_with(".worktrees"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_files_parse() {
        let s = Setup::parse("copy = [\".env*\", \"config/master.key\"]\nsetup = [\"npm ci\", \"\"]\n").unwrap();
        assert_eq!(s.copy, vec![".env*", "config/master.key"]);
        assert_eq!(s.commands().count(), 1, "blank commands are nothing");
        assert!(Setup::parse("").unwrap() == Setup::default());
        for bad in ["copy = [\"../x\"]", "copy = [\"/etc/passwd\"]", "copy = [\"*/x\"]", "copy = [\"\"]", "run = []"] {
            assert!(Setup::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn wildcards() {
        assert!(glob(b".env*", b".env"));
        assert!(glob(b".env*", b".env.local"));
        assert!(!glob(b".env*", b"env"));
        assert!(glob(b"?.key", b"a.key"));
        assert!(!glob(b"?.key", b"ab.key"));
        assert!(glob(b"*.*.local", b".env.dev.local"));
    }

    #[test]
    fn ports_follow_the_number() {
        let mut cfg = crate::config::Config::default().worktrees;
        assert_eq!(cfg.port_step, 10);
        assert!(env("x", 0, &cfg).is_empty(), "the main checkout gets nothing");
        let e = env("fix", 2, &cfg);
        assert!(e.contains(&("KRAAL_WORKTREE".into(), "fix".into())));
        assert!(e.contains(&("KRAAL_PORT_OFFSET".into(), "20".into())));
        assert!(!e.iter().any(|(k, _)| k == "PORT"), "no base, no PORT");
        cfg.port_base = Some(3000);
        assert!(env("fix", 2, &cfg).contains(&("PORT".into(), "3020".into())));
        cfg.port_base = Some(65530);
        assert!(!env("fix", 2, &cfg).iter().any(|(k, _)| k == "PORT"), "never a port that does not exist");
        // A spec rebuilt for the same pane keeps them, and only them.
        cfg.port_base = Some(3000);
        let mut old = env("fix", 1, &cfg);
        old.push(("OTHER".into(), "x".into()));
        let mut to = vec![("CLAUDE_CONFIG_DIR".to_string(), "/a".to_string())];
        carry_env(&old, &mut to);
        assert_eq!(to.len(), 5, "{to:?}");
        assert!(to.contains(&("PORT".into(), "3010".into())) && !to.iter().any(|(k, _)| k == "OTHER"));
        assert!(in_worktrees_dir(Path::new("/src/app.worktrees/fix/web")));
        assert!(!in_worktrees_dir(Path::new("/src/app")));
        assert!(!in_worktrees_dir(Path::new("/src/app.worktrees")));
    }

    #[test]
    fn copies_stay_in_the_repository_and_never_write_through_a_link() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = std::env::temp_dir().join(format!("kraal-wtcopy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (repo, wt, outside) = (dir.join("repo"), dir.join("wt"), dir.join("outside"));
        for d in [&repo, &wt, &outside, &repo.join("config/keys"), &repo.join(".git")] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(repo.join(".env"), "SECRET=1\n").unwrap();
        std::fs::set_permissions(repo.join(".env"), std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(repo.join(".env.local"), "LOCAL=1\n").unwrap();
        std::fs::write(repo.join("config/keys/a.key"), "a").unwrap();
        std::fs::write(repo.join(".git/config"), "x").unwrap();
        std::fs::write(outside.join("id_rsa"), "private").unwrap();
        std::fs::write(repo.join("tracked.txt"), "main checkout").unwrap();
        std::fs::write(wt.join("tracked.txt"), "worktree").unwrap();
        // Links out of the repository, at the top and inside a copied directory.
        symlink(outside.join("id_rsa"), repo.join(".env.prod")).unwrap();
        symlink(&outside, repo.join("config/keys/more")).unwrap();
        // A link inside the repository is followed.
        symlink(repo.join(".env"), repo.join(".env.link")).unwrap();
        // The worktree commits a link where a copy would go: nothing is written through it.
        symlink(&outside, wt.join("config")).unwrap();
        let patterns: Vec<String> =
            [".env*", "config/keys", "missing.txt", "tracked.txt", ".git/config", "../outside/id_rsa"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        let got = copy_files(&repo, &wt, &patterns);
        assert_eq!(got.copied, vec![".env", ".env.link", ".env.local"], "{got:?}");
        assert_eq!(std::fs::read_to_string(wt.join(".env")).unwrap(), "SECRET=1\n");
        let mode = std::fs::metadata(wt.join(".env")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "permissions are kept");
        assert_eq!(std::fs::read_to_string(wt.join(".env.link")).unwrap(), "SECRET=1\n");
        assert!(!wt.join(".env.prod").exists(), "a link out of the repository is not followed");
        assert!(!outside.join("keys").exists(), "nothing written through the worktree's link");
        assert_eq!(std::fs::read_to_string(wt.join("tracked.txt")).unwrap(), "worktree", "kept as it is");
        let why = |p: &str| got.skipped.iter().find(|(s, _)| s == p).map(|(_, w)| w.clone()).unwrap_or_default();
        assert_eq!(why(".env.prod"), "a link out of the repository");
        assert_eq!(why("tracked.txt"), "already in the worktree");
        assert_eq!(why(".git/config"), "git's own files");
        assert!(why("config/keys").contains("not a directory"), "{got:?}");
        assert!(why("../outside/id_rsa").contains("not a path in the repository"), "{got:?}");
        assert!(why("missing.txt").is_empty(), "a file that is not there is nothing to copy");
        // A directory: what is under it, its links left out.
        std::fs::remove_file(wt.join("config")).unwrap();
        let got = copy_files(&repo, &wt, &["config".to_string()]);
        assert_eq!(got.copied, vec!["config"]);
        assert_eq!(std::fs::read_to_string(wt.join("config/keys/a.key")).unwrap(), "a");
        assert!(!wt.join("config/keys/more").exists());
        assert_eq!(got.skipped, vec![("config/keys/more".to_string(), "a link".to_string())]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_plan_reads_the_file_of_the_commit_the_worktree_starts_from() {
        if std::process::Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("kraal-wtplan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = dir.join("app");
        std::fs::create_dir_all(repo.join(".kraal")).unwrap();
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .unwrap();
            assert!(ok.status.success(), "{args:?}: {}", String::from_utf8_lossy(&ok.stderr));
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(repo.join(".kraal/worktree.toml"), "copy = [\".env\"]\nsetup = [\"echo one\"]\n").unwrap();
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "one"]);
        // Changed but not committed: not what a new worktree checks out.
        std::fs::write(repo.join(".kraal/worktree.toml"), "setup = [\"echo two\"]\n").unwrap();
        std::fs::write(repo.join(".env"), "A=1\n").unwrap();
        let user = Setup { copy: vec![".env".into(), "local.txt".into()], setup: vec!["echo mine".into()] };
        let plan = Plan::new(&repo, "feature", None, user, true).unwrap();
        assert_eq!(plan.from.base, "main");
        assert_eq!(plan.steps(), vec!["echo one", "echo mine"], "the repository's first, then yours");
        assert_eq!(plan.copy(), vec![".env", "local.txt"]);
        assert!(plan.trust_question().contains("  1. echo one"), "{}", plan.trust_question());
        assert!(!plan.trust_question().contains("echo mine"), "yours need no approval");
        let prepared = plan.create().unwrap();
        assert_eq!(prepared.worktree.index, 1);
        assert_eq!(std::fs::read_to_string(prepared.worktree.path.join(".env")).unwrap(), "A=1\n");
        let script = prepared.script().unwrap();
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&script)
            .current_dir(&prepared.worktree.path)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success() && text.contains("copied from the main checkout: .env"), "{text}");
        assert!(text.find("one").unwrap() < text.find("mine").unwrap(), "{text}");
        // Already made: the name is taken.
        assert!(Plan::new(&repo, "feature", None, Setup::default(), true).is_err());
        // A file that cannot be read is an error, not a worktree made without its setup.
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "two"]);
        std::fs::write(repo.join(".kraal/worktree.toml"), "setup = \"not a list\"\n").unwrap();
        git(&["commit", "-q", "-am", "broken"]);
        assert!(Plan::new(&repo, "other", None, Setup::default(), true).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--no-setup` reads neither the repository's file nor the user's: one that is broken does
    /// not stop a worktree made without them.
    #[test]
    fn without_setup_a_broken_file_is_not_read() {
        if std::process::Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("kraal-wtbare-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = dir.join("app");
        std::fs::create_dir_all(repo.join(".kraal")).unwrap();
        std::fs::write(repo.join(".kraal/worktree.toml"), "setup = \"not a list\"\nunknown = 1\n").unwrap();
        for args in [&["init", "-q", "-b", "main"][..], &["add", "."], &["commit", "-q", "-m", "broken"]] {
            let ok = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .unwrap();
            assert!(ok.status.success(), "{args:?}");
        }
        let broken_user = Setup { copy: vec!["../outside".into()], setup: Vec::new() };
        assert!(Plan::new(&repo, "x", None, Setup::default(), true).is_err());
        let plan = Plan::new(&repo, "x", None, broken_user, false).unwrap();
        assert!(plan.from.file.is_none() && plan.copy().is_empty() && plan.steps().is_empty());
        assert!(!plan.needs_trust());
        let prepared = plan.create().unwrap();
        assert!(prepared.worktree.path.join(".kraal/worktree.toml").exists());
        assert!(prepared.script().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What the approval shows is what runs: escape sequences, carriage returns and invisible
    /// characters of the repository's commands are written out, never acted on by the terminal.
    #[test]
    fn a_command_is_shown_for_approval_as_it_runs() {
        let text = "setup = [\"touch /tmp/unapproved # \\u001b[2K\\r  1. echo safe\", \"echo \\u202Eevil\\u202C ok\"]\n\
                    copy = [\".env\\u0007\"]\n";
        let setup = Setup::parse(text).unwrap();
        assert!(setup.setup[0].contains('\u{1b}'), "the command itself is untouched");
        let file = |text: &str| RepoFile {
            repo: PathBuf::from("/src/app"),
            base: "main".into(),
            file: Some((text.into(), Setup::parse(text).unwrap())),
        };
        let shown = file(text).trust_question("x");
        assert!(!shown.chars().any(|c| c.is_control() && c != '\n'), "{shown:?}");
        assert!(!shown.contains(['\u{202e}', '\u{202c}']), "{shown:?}");
        assert!(shown.contains("  1. touch /tmp/unapproved # \\u{1b}[2K\\r  1. echo safe"), "{shown}");
        assert!(shown.contains("  2. echo \\u{202e}evil\\u{202c} ok"), "{shown}");
        assert!(shown.contains("it copies from the main checkout: .env\\u{7}"), "{shown}");
        assert_eq!(visible("plain é 漢字 $(x) \\n"), "plain é 漢字 $(x) \\n");
        // The file as `kraal wt setup` prints it, and an error that quotes it: no more there.
        let raw = "copy = [\".env\"] # \u{200b}\u{202e}\r\nsetup = []\n";
        assert_eq!(file(raw).shown().unwrap(), "copy = [\".env\"] # \\u{200b}\\u{202e}\nsetup = []\n");
        let error = format!("{:#}", Setup::parse_repo("setup = \u{1b}[2J\n").unwrap_err());
        assert!(error.contains("\\u{1b}[2J") && !error.contains('\u{1b}'), "{error:?}");
    }

    /// The repository's file is acted on once approved, whatever it asks for: files to copy
    /// need the approval as commands do. One that asks for nothing needs none.
    #[test]
    fn what_a_repository_asks_for_needs_approval_be_it_only_a_copy() {
        let file = |text: &str| RepoFile {
            repo: PathBuf::from("/nowhere/kraal-test-repository"),
            base: "main".into(),
            file: Some((text.into(), Setup::parse(text).unwrap())),
        };
        let asks_nothing = |text: &str| Setup::parse(text).unwrap().is_empty();
        assert!(!asks_nothing("setup = [\"npm ci\"]\n") && !asks_nothing("copy = [\".env\"]\n"));
        // Nothing asked for, or no file: approved without a look at what was recorded.
        assert!(asks_nothing("") && file("").approved() && file("setup = [\" \"]\ncopy = []\n").approved());
        let none = RepoFile { repo: PathBuf::from("/nowhere/kraal-test-repository"), base: "main".into(), file: None };
        assert!(none.approved() && none.shown().is_none());
        // What is asked says each part there is, and only those.
        let copy_only = file("copy = [\".env\", \"config/*.key\"]\n").trust_question("fix");
        assert!(copy_only.contains("it copies from the main checkout: .env, config/*.key"), "{copy_only}");
        assert!(!copy_only.contains("it runs"), "{copy_only}");
        let both = file("copy = [\".env\"]\nsetup = [\"npm ci\"]\n").trust_question("fix");
        assert!(both.contains("it copies from the main checkout: .env\nit runs there:\n  1. npm ci"), "{both}");
        // Where nobody can be asked: the commands that show it and approve it, for that branch.
        let said = file("copy = [\".env\"]\n").not_approved(Some("release 1"));
        assert!(said.starts_with("the worktree setup comes from the repository /nowhere/kraal-test-repository"));
        assert!(
            said.contains("`kraal wt setup --base 'release 1'`")
                && said.contains("`kraal wt trust --base 'release 1'`")
        );
        assert!(file("copy = [\".env\"]\n").not_approved(None).contains("(`kraal wt trust`)"));
    }

    /// A pattern matches in time proportional to its length times the name's, however many
    /// wildcards it has.
    #[test]
    fn a_pattern_never_takes_long_to_match() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let pattern = format!("{}b", "*a".repeat(30));
            let _ = tx.send(glob(pattern.as_bytes(), "a".repeat(60).as_bytes()));
        });
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(5)), Ok(false), "too slow");
        assert!(glob(b"*a*b*c", b"xaybzc"));
        assert!(glob(b"a*", b"a"));
        assert!(glob(b"*", b""));
        assert!(!glob(b"?", b""));
        assert!(!glob(b"*a", b"ab"));
        assert!(Setup::parse(&format!("copy = [\"{}\"]", "x".repeat(2000))).is_err(), "patterns have a length limit");
    }
}
