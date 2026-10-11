//! Git: what branch a pane is on and whether it has changes, and worktrees that give each coding
//! agent its own branch and directory.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitInfo {
    pub root: PathBuf,
    /// Branch name, or a short commit id when detached.
    pub branch: String,
    /// Tracked files differ from HEAD.
    pub dirty: bool,
    /// Files git does not track yet (what agents create most), or perhaps: in a tree too big to
    /// list them in time, they are not ruled out.
    pub untracked: bool,
    pub ahead: u32,
    pub behind: u32,
}

impl GitInfo {
    /// `⎇ main* ↑2` style label.
    pub fn label(&self, ascii: bool) -> String {
        let mut s = format!("{}{}", if ascii { "git:" } else { "⎇ " }, self.branch);
        if self.dirty {
            s.push('*');
        }
        if self.untracked {
            s.push('?');
        }
        if self.ahead > 0 {
            s.push_str(&format!(" {}{}", if ascii { "+" } else { "↑" }, self.ahead));
        }
        if self.behind > 0 {
            s.push_str(&format!(" {}{}", if ascii { "-" } else { "↓" }, self.behind));
        }
        s
    }
}

/// A worktree kraal created for an agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Worktree {
    /// The main checkout.
    pub repo: PathBuf,
    pub path: PathBuf,
    pub branch: String,
    /// What the branch started from, and what reviews compare against.
    pub base: String,
}

impl Worktree {
    pub fn name(&self) -> String {
        self.branch.strip_prefix("kraal/").unwrap_or(&self.branch).to_string()
    }
}

/// Run git with a deadline; stdout on success.
fn git(dir: &Path, args: &[&str], timeout: Duration) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(args).env("GIT_OPTIONAL_LOCKS", "0").env("GIT_TERMINAL_PROMPT", "0");
    let out = crate::exec::run(&mut cmd, timeout, 8 << 20).context("running git")?;
    if out.timed_out {
        bail!("git {} took too long", args.join(" "));
    }
    if !out.success() {
        bail!("git {}: {}", args.join(" "), out.stderr_text().trim());
    }
    Ok(out.stdout_text())
}

/// Branch and state of the repository containing `cwd`, if any. Untracked files count; when
/// listing them is too slow (huge trees), only tracked changes are checked and untracked files
/// are not ruled out (`?` stays shown), rather than the tree looking clean.
pub fn status(cwd: &Path) -> Option<GitInfo> {
    let base = ["status", "--porcelain=v2", "--branch"];
    let (out, listed) = match git(cwd, &[&base[..], &["-unormal"]].concat(), Duration::from_secs(5)) {
        Ok(out) => (out, true),
        Err(_) => (git(cwd, &[&base[..], &["-uno"]].concat(), Duration::from_secs(2)).ok()?, false),
    };
    let root = git(cwd, &["rev-parse", "--show-toplevel"], Duration::from_secs(1)).ok()?;
    let mut info = parse_status(&out)?;
    info.root = PathBuf::from(root.trim());
    info.untracked |= !listed;
    Some(info)
}

/// Parse `git status --porcelain=v2 --branch` output.
pub fn parse_status(text: &str) -> Option<GitInfo> {
    let mut branch = None;
    let mut oid = None;
    let (mut ahead, mut behind, mut dirty, mut untracked) = (0, 0, false, false);
    for line in text.lines() {
        if let Some(h) = line.strip_prefix("# branch.head ") {
            if h != "(detached)" {
                branch = Some(h.to_string());
            }
        } else if let Some(o) = line.strip_prefix("# branch.oid ") {
            oid = Some(o.chars().take(7).collect::<String>());
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            for part in ab.split_whitespace() {
                if let Some(n) = part.strip_prefix('+') {
                    ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix('-') {
                    behind = n.parse().unwrap_or(0);
                }
            }
        } else if line.starts_with("1 ") || line.starts_with("2 ") || line.starts_with("u ") {
            dirty = true;
        } else if line.starts_with("? ") {
            untracked = true;
        }
    }
    let branch = branch.or(oid)?;
    Some(GitInfo { root: PathBuf::new(), branch, dirty, untracked, ahead, behind })
}

/// Top of the repository containing `cwd`. For a linked worktree, the main checkout.
pub fn main_checkout(cwd: &Path) -> Option<PathBuf> {
    let common = git(cwd, &["rev-parse", "--path-format=absolute", "--git-common-dir"], Duration::from_secs(1)).ok()?;
    let common = PathBuf::from(common.trim());
    // `<repo>/.git` for normal repositories.
    if common.file_name().is_some_and(|n| n == ".git") {
        return common.parent().map(Path::to_path_buf);
    }
    git(cwd, &["rev-parse", "--show-toplevel"], Duration::from_secs(1)).ok().map(|s| PathBuf::from(s.trim()))
}

/// The path account rules should see for `cwd`: inside a linked worktree, the same place in the
/// main checkout, so rules written for a repository also apply to its worktrees. `cwd` itself
/// when git could not tell in time (it is asked again next time).
pub fn project_path(cwd: &Path) -> PathBuf {
    if let Some(hit) = PROJECT_PATHS.lock().ok().and_then(|c| c.as_ref().and_then(|m| m.get(cwd).cloned())) {
        return hit;
    }
    let Some(found) = find_project_path(cwd) else { return cwd.to_path_buf() };
    if let Ok(mut c) = PROJECT_PATHS.lock() {
        c.get_or_insert_with(Default::default).insert(cwd.to_path_buf(), found.clone());
    }
    found
}

/// Answers of [`project_path`], remembered (worktrees do not move).
static PROJECT_PATHS: std::sync::Mutex<Option<std::collections::HashMap<PathBuf, PathBuf>>> =
    std::sync::Mutex::new(None);

/// [`project_path`] without running git, for the server's event loop: what git said before
/// (the git thread asks about the directories of panes). `None` when git was not asked yet, or
/// could not tell: a worktree may live anywhere, whatever its directory's name says, so ask
/// [`project_path`] off the loop first.
pub fn known_project_path(cwd: &Path) -> Option<PathBuf> {
    PROJECT_PATHS.lock().ok().and_then(|c| c.as_ref().and_then(|m| m.get(cwd).cloned()))
}

/// Where `cwd` is in its main checkout: `cwd` itself outside a repository or in the main
/// checkout. `None` when this could not be told for sure now (git took too long, or failed and
/// the repository's own files could not be read): such an answer is never remembered, and no
/// account is picked on it.
fn find_project_path(cwd: &Path) -> Option<PathBuf> {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir", "--show-toplevel"])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C");
    let out = match crate::exec::run(&mut cmd, Duration::from_secs(5), 64 << 10) {
        Ok(out) => out,
        // No git here: the repository's own files tell.
        Err(_) => return project_path_from_files(cwd).ok(),
    };
    if out.timed_out {
        return None;
    }
    let text = out.stdout_text();
    let mut lines = text.lines();
    let (Some(common), Some(top), true) = (lines.next(), lines.next(), out.success()) else {
        // Whatever git says when it fails, the files tell: "not a git repository" is also what
        // it says in a linked worktree whose main checkout is gone, which is still a worktree.
        return project_path_from_files(cwd).ok();
    };
    let (common, top) = (PathBuf::from(common), PathBuf::from(top));
    // `<repo>/.git` for normal repositories and their linked worktrees.
    let main = match common.file_name() {
        Some(n) if n == ".git" => common.parent().map(Path::to_path_buf).unwrap_or_else(|| top.clone()),
        _ => top.clone(),
    };
    Some(project_path_in(cwd, &top, &main))
}

/// `cwd`, in worktree `top`, as the same place in main checkout `main`.
fn project_path_in(cwd: &Path, top: &Path, main: &Path) -> PathBuf {
    if top == main {
        return cwd.to_path_buf();
    }
    let canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    match canonical.strip_prefix(top) {
        Ok(rest) => main.join(rest),
        Err(_) => main.to_path_buf(),
    }
}

/// [`find_project_path`] from the repository's own files, as git finds them: the nearest
/// `.git` above `cwd`; a linked worktree's is a file naming its git directory, whose `commondir`
/// leads to the main repository's. An error when one of them cannot be read.
fn project_path_from_files(cwd: &Path) -> std::io::Result<PathBuf> {
    let canonical = cwd.canonicalize()?;
    let mut dir = canonical.as_path();
    loop {
        let dot = dir.join(".git");
        match std::fs::symlink_metadata(&dot) {
            // The main checkout of a repository.
            Ok(m) if m.is_dir() => return Ok(cwd.to_path_buf()),
            Ok(_) => {
                let text = std::fs::read_to_string(&dot)?;
                let Some(gitdir) = text.lines().next().and_then(|l| l.strip_prefix("gitdir:")) else {
                    return Err(std::io::Error::other(format!("{}: not a git link", dot.display())));
                };
                let gitdir = dir.join(gitdir.trim());
                // The git directory it names must be there: a worktree whose main checkout went
                // is not "no worktree".
                if !std::fs::metadata(&gitdir)?.is_dir() {
                    return Err(std::io::Error::other(format!("{}: not a directory", gitdir.display())));
                }
                let common = match std::fs::read_to_string(gitdir.join("commondir")) {
                    Ok(c) => gitdir.join(c.trim()).canonicalize()?,
                    // A submodule: a repository of its own, not a worktree.
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(cwd.to_path_buf()),
                    Err(e) => return Err(e),
                };
                let main = match common.file_name() {
                    Some(n) if n == ".git" => {
                        common.parent().map(Path::to_path_buf).unwrap_or_else(|| dir.to_path_buf())
                    }
                    _ => dir.to_path_buf(),
                };
                return Ok(project_path_in(cwd, dir, &main));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => match dir.parent() {
                Some(up) => dir = up,
                // Not in a repository.
                None => return Ok(cwd.to_path_buf()),
            },
            Err(e) => return Err(e),
        }
    }
}

/// A name usable in a branch and a directory.
pub fn valid_worktree_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 60
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.starts_with(['.', '-'])
}

/// Where kraal puts the worktrees of `repo`: a sibling directory `<repo>.worktrees/`.
pub fn worktrees_dir(repo: &Path) -> PathBuf {
    let name = repo.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "repo".into());
    repo.with_file_name(format!("{name}.worktrees"))
}

/// Create a worktree on a new branch `kraal/<name>` from `base` (default: the current branch).
pub fn add_worktree(cwd: &Path, name: &str, base: Option<&str>) -> Result<Worktree> {
    if !valid_worktree_name(name) {
        bail!("invalid worktree name {name:?} (letters, digits, - _ .)");
    }
    let repo = main_checkout(cwd).context("not inside a git repository")?;
    // The current branch is the one checked out where the user is, which may be another
    // worktree than the main checkout; a detached HEAD is its commit.
    let base = match base {
        Some(b) => b.to_string(),
        None => match git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"], Duration::from_secs(2))?.trim() {
            "HEAD" => git(cwd, &["rev-parse", "HEAD"], Duration::from_secs(2))?.trim().to_string(),
            branch => branch.to_string(),
        },
    };
    let branch = format!("kraal/{name}");
    let path = worktrees_dir(&repo).join(name);
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    let path_s = path.display().to_string();
    git(&repo, &["worktree", "add", "-b", &branch, &path_s, &base], Duration::from_secs(60))?;
    // Remember the base with the branch, for reviews and merges in later sessions.
    git(&repo, &["config", &format!("branch.{branch}.kraalbase"), &base], Duration::from_secs(5))?;
    Ok(Worktree { repo, path, branch, base })
}

/// The branch a kraal worktree started from, as recorded when it was created.
pub fn worktree_base(repo: &Path, branch: &str) -> Option<String> {
    let out = git(repo, &["config", "--get", &format!("branch.{branch}.kraalbase")], Duration::from_secs(2)).ok()?;
    let base = out.trim().to_string();
    (!base.is_empty()).then_some(base)
}

/// What removing a worktree would lose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WorktreeState {
    /// Uncommitted changes, untracked files included.
    pub changes: usize,
    /// Commits its base does not have: on its branch, and on a detached HEAD in it.
    pub unmerged: usize,
    /// Which work that is: the commits at stake, what is staged, and the content of each
    /// changed file. A new edit to an already changed file, a change staged, or a rewritten
    /// commit changes it even when the counts stay the same.
    pub identity: u64,
}

impl WorktreeState {
    pub fn is_clean(&self) -> bool {
        self.changes == 0 && self.unmerged == 0
    }

    /// `3 uncommitted changes and 2 unmerged commits`.
    pub fn describe(&self) -> String {
        let n = |count: usize, what: &str| format!("{count} {what}{}", if count == 1 { "" } else { "s" });
        match (self.changes, self.unmerged) {
            (0, 0) => "nothing to lose".into(),
            (c, 0) => n(c, "uncommitted change"),
            (0, u) => n(u, "unmerged commit"),
            (c, u) => format!("{} and {}", n(c, "uncommitted change"), n(u, "unmerged commit")),
        }
    }
}

pub fn worktree_state(w: &Worktree) -> Result<WorktreeState> {
    use std::hash::{Hash, Hasher};
    let mut identity = std::collections::hash_map::DefaultHasher::new();
    let status = git(&w.path, &["status", "--porcelain", "-z", "--untracked-files=all"], Duration::from_secs(10))?;
    let mut changes = 0;
    let mut fields = status.split('\0').filter(|f| !f.is_empty());
    while let Some(field) = fields.next() {
        changes += 1;
        // `XY path`; a rename or copy is followed by its original path.
        if matches!(field.as_bytes().first(), Some(b'R' | b'C')) {
            fields.next();
        }
        let path = field.get(3..).unwrap_or_default();
        path.hash(&mut identity);
        hash_content(&w.path.join(path), &mut identity);
    }
    // What is staged, by blob id (a change staged over an unchanged file shows nowhere else).
    git(&w.path, &["diff", "--cached", "--raw", "-z", "--no-abbrev", "--no-renames"], Duration::from_secs(10))?
        .hash(&mut identity);
    let base = if w.base.is_empty() {
        worktree_base(&w.repo, &w.branch)
            .or_else(|| git(&w.repo, &["rev-parse", "--abbrev-ref", "HEAD"], Duration::from_secs(2)).ok())
            .map(|b| b.trim().to_string())
            .context("no base branch")?
    } else {
        w.base.clone()
    };
    // Commits only a detached HEAD holds are lost with the worktree too.
    let mut tips = Vec::new();
    let branch_ref = format!("refs/heads/{}", w.branch);
    if let Ok(tip) = git(&w.repo, &["rev-parse", "--verify", "-q", &branch_ref], Duration::from_secs(5)) {
        tips.push(tip.trim().to_string());
    }
    if git(&w.path, &["symbolic-ref", "-q", "HEAD"], Duration::from_secs(5)).is_err() {
        tips.push(git(&w.path, &["rev-parse", "HEAD"], Duration::from_secs(5))?.trim().to_string());
    }
    tips.hash(&mut identity);
    let unmerged = if tips.is_empty() {
        0
    } else {
        let mut args = vec!["rev-list", "--count"];
        args.extend(tips.iter().map(String::as_str));
        args.extend(["--not", base.as_str()]);
        git(&w.repo, &args, Duration::from_secs(10))?.trim().parse().unwrap_or(0)
    };
    Ok(WorktreeState { changes, unmerged, identity: identity.finish() })
}

/// Feed what `path` holds into `h`: a file's bytes, a link's target, or that it is missing.
fn hash_content(path: &Path, h: &mut impl std::hash::Hasher) {
    use std::hash::Hash;
    use std::io::Read;
    match path.symlink_metadata() {
        Ok(m) if m.file_type().is_symlink() => std::fs::read_link(path).ok().hash(h),
        Ok(m) if m.is_file() => {
            let Ok(mut f) = std::fs::File::open(path) else { return "unreadable".hash(h) };
            let mut buf = [0u8; 64 << 10];
            loop {
                match f.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => h.write(&buf[..n]),
                }
            }
        }
        Ok(_) => "other".hash(h),
        Err(_) => "missing".hash(h),
    }
}

/// Remove a worktree and its branch. Without `force`, refuse when that would lose uncommitted
/// changes or commits its base does not have.
pub fn remove_worktree(w: &Worktree, force: bool) -> Result<()> {
    if !force {
        let state = worktree_state(w)?;
        if !state.is_clean() {
            bail!("{} has {}; remove it with force to discard them", w.name(), state.describe());
        }
    }
    let path = w.path.display().to_string();
    let mut args = vec!["worktree", "remove", path.as_str()];
    if force {
        args.push("--force");
    }
    git(&w.repo, &args, Duration::from_secs(30))?;
    // Merged (checked above) or explicitly forced: -D, since git's own check compares with HEAD.
    git(&w.repo, &["branch", "-D", &w.branch], Duration::from_secs(10))?;
    Ok(())
}

/// The kraal worktrees of the repository containing `cwd`.
pub fn list_worktrees(cwd: &Path) -> Result<Vec<Worktree>> {
    let repo = main_checkout(cwd).context("not inside a git repository")?;
    let out = git(&repo, &["worktree", "list", "--porcelain"], Duration::from_secs(5))?;
    let mut list = Vec::new();
    let mut path: Option<PathBuf> = None;
    for line in out.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(p));
        } else if let Some(b) = line.strip_prefix("branch refs/heads/")
            && b.starts_with("kraal/")
            && let Some(p) = path.take()
        {
            let base = worktree_base(&repo, b).unwrap_or_default();
            list.push(Worktree { repo: repo.clone(), path: p, branch: b.to_string(), base });
        }
    }
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain_v2() {
        let s = "# branch.oid 3f58e39c80027dcb\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +2 -1\n1 .M N... 100644 100644 100644 a b f\n? new.txt\n";
        let i = parse_status(s).unwrap();
        assert_eq!((i.branch.as_str(), i.dirty, i.untracked, i.ahead, i.behind), ("main", true, true, 2, 1));
        assert_eq!(i.label(false), "⎇ main*? ↑2 ↓1");
        assert_eq!(i.label(true), "git:main*? +2 -1");
        let d = parse_status("# branch.oid 3f58e39c80027dcb\n# branch.head (detached)\n").unwrap();
        assert_eq!((d.branch.as_str(), d.dirty), ("3f58e39", false));
        assert!(parse_status("").is_none());
    }

    #[test]
    fn a_worktree_is_found_even_when_git_cannot_say() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("kraal-gitfiles-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .unwrap();
            assert!(ok.status.success(), "{args:?}: {}", String::from_utf8_lossy(&ok.stderr));
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["commit", "-q", "--allow-empty", "-m", "one"]);
        let wt = dir.join("far/feature");
        git(&["worktree", "add", "-q", "-b", "feature", wt.to_str().unwrap()]);
        std::fs::create_dir_all(wt.join("src")).unwrap();
        let repo = repo.canonicalize().unwrap();
        // From the files alone: the worktree's place in the main checkout.
        assert_eq!(project_path_from_files(&wt.join("src")).unwrap(), repo.join("src"));
        assert_eq!(project_path_from_files(&repo).unwrap(), repo);
        assert_eq!(project_path_from_files(&dir).unwrap(), dir, "not in a repository");
        // git fails on a broken configuration: that is no answer, the files still tell.
        let config = repo.join(".git/config");
        let mut text = std::fs::read_to_string(&config).unwrap();
        text.push_str("\n[[broken\n");
        std::fs::write(&config, text).unwrap();
        assert_eq!(find_project_path(&wt.join("src")), Some(repo.join("src")));
        // A worktree link that cannot be read is no answer either: nothing is remembered.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(wt.join(".git"), std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_to_string(wt.join(".git")).is_err() {
            assert_eq!(find_project_path(&wt.join("src")), None);
        }
        std::fs::set_permissions(wt.join(".git"), std::fs::Permissions::from_mode(0o644)).unwrap();
        // The main checkout gone, git says "not a git repository": still a worktree, of a
        // repository that cannot be told now. No answer, and nothing remembered.
        std::fs::rename(&repo, dir.join("moved")).unwrap();
        assert_eq!(find_project_path(&wt.join("src")), None);
        assert!(project_path_from_files(&wt.join("src")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn worktree_lifecycle() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("kraal-git-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let repo = dir.join("app");
        std::fs::create_dir_all(&repo).unwrap();
        let run = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .unwrap();
            assert!(ok.status.success(), "{args:?}: {}", String::from_utf8_lossy(&ok.stderr));
        };
        run(&["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("a.txt"), "1").unwrap();
        run(&["add", "a.txt"]);
        run(&["commit", "-q", "-m", "one"]);
        let info = status(&repo).unwrap();
        assert_eq!((info.branch.as_str(), info.dirty), ("main", false));
        let w = add_worktree(&repo, "fix-login", None).unwrap();
        assert_eq!(w.branch, "kraal/fix-login");
        assert_eq!(w.base, "main");
        assert_eq!(w.path, dir.join("app.worktrees").join("fix-login"));
        assert_eq!(main_checkout(&w.path).unwrap(), std::fs::canonicalize(&repo).unwrap());
        std::fs::create_dir_all(w.path.join("src")).unwrap();
        let canonical_repo = std::fs::canonicalize(&repo).unwrap();
        assert_eq!(project_path(&w.path.join("src")), canonical_repo.join("src"), "rules see the repository");
        assert_eq!(project_path(&repo), repo, "a main checkout is itself");
        assert_eq!(known_project_path(&w.path.join("src")), Some(canonical_repo.join("src")), "remembered");
        // A directory's name says nothing: a worktree of `app` named like another repository's.
        let decoy = dir.join("other.worktrees/fix");
        run(&["worktree", "add", "-q", "-b", "decoy", decoy.to_str().unwrap()]);
        assert_eq!(known_project_path(&decoy), None, "only git can tell");
        assert_eq!(project_path(&decoy), canonical_repo, "its repository, not `other`");
        assert_eq!(known_project_path(&decoy), Some(canonical_repo.clone()), "remembered");
        assert_eq!(known_project_path(&dir.join("anywhere")), None, "only git can tell");
        assert_eq!(project_path(&dir.join("anywhere")), dir.join("anywhere"), "outside any repository");
        let wi = status(&w.path).unwrap();
        assert_eq!(wi.branch, "kraal/fix-login");
        // Made from inside a worktree, a new one starts from that worktree's branch.
        let f = add_worktree(&w.path, "followup", None).unwrap();
        assert_eq!(f.base, "kraal/fix-login");
        remove_worktree(&f, true).unwrap();
        let in_tree = |args: &[&str]| {
            let ok = Command::new("git")
                .arg("-C")
                .arg(&w.path)
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
                .args(args)
                .output()
                .unwrap();
            assert!(ok.status.success(), "{args:?}");
        };
        std::fs::write(w.path.join("b.txt"), "b").unwrap();
        assert!(status(&w.path).unwrap().untracked);
        in_tree(&["add", "b.txt"]);
        in_tree(&["commit", "-q", "-m", "two"]);
        let counts = |w: &Worktree| {
            let s = worktree_state(w).unwrap();
            (s.changes, s.unmerged)
        };
        assert_eq!(counts(&w), (0, 1));
        std::fs::write(w.path.join("a.txt"), "2").unwrap();
        assert!(status(&w.path).unwrap().dirty);
        let listed = list_worktrees(&repo).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].base, "main", "the base is remembered");
        assert!(add_worktree(&repo, "fix-login", None).is_err(), "names are unique");
        assert!(add_worktree(&repo, "../escape", None).is_err());
        assert_eq!(counts(&w), (1, 1));
        // More work in the same file: the same counts, but not the same work.
        let before = worktree_state(&w).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(w.path.join("a.txt"), "22").unwrap();
        assert_eq!(counts(&w), (1, 1));
        assert_ne!(worktree_state(&w).unwrap(), before, "a new edit changes the identity");
        // The same size and the same time, but other content.
        let before = worktree_state(&w).unwrap();
        let file = w.path.join("a.txt");
        let mtime = file.metadata().unwrap().modified().unwrap();
        std::fs::write(&file, "33").unwrap();
        std::fs::File::options().write(true).open(&file).unwrap().set_modified(mtime).unwrap();
        assert_ne!(worktree_state(&w).unwrap(), before, "content counts, not size and time");
        // A change staged over the same file: the counts do not move either.
        let before = worktree_state(&w).unwrap();
        in_tree(&["add", "a.txt"]);
        assert_eq!(counts(&w), (1, 1));
        assert_ne!(worktree_state(&w).unwrap(), before, "what is staged counts");
        // A commit made on a detached HEAD would be lost with the worktree.
        in_tree(&["stash", "-q"]);
        in_tree(&["checkout", "-q", "--detach"]);
        std::fs::write(w.path.join("c.txt"), "c").unwrap();
        in_tree(&["add", "c.txt"]);
        in_tree(&["commit", "-q", "-m", "detached"]);
        assert_eq!(counts(&w), (0, 2), "the branch's commit and the detached one");
        in_tree(&["checkout", "-q", "kraal/fix-login"]);
        in_tree(&["stash", "pop", "-q"]);
        let refused = remove_worktree(&w, false).unwrap_err().to_string();
        assert!(refused.contains("1 uncommitted change and 1 unmerged commit"), "{refused}");
        assert!(w.path.exists(), "nothing removed without force");
        remove_worktree(&w, true).unwrap();
        assert!(!w.path.exists());
        assert!(list_worktrees(&repo).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
