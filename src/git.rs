//! Git: what branch a pane is on and whether it has changes, and worktrees that give each coding
//! agent its own branch and directory.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitInfo {
    pub root: PathBuf,
    /// Branch name, or a short commit id when detached.
    pub branch: String,
    /// Tracked files differ from HEAD.
    pub dirty: bool,
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
    let mut child = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running git")?;
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            let out = child.wait_with_output()?;
            if !status.success() {
                bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
            }
            return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            bail!("git {} took too long", args.join(" "));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Branch and state of the repository containing `cwd`, if any. Untracked files are ignored
/// (scanning them is slow in big trees and rarely what "dirty" should mean).
pub fn status(cwd: &Path) -> Option<GitInfo> {
    let out = git(cwd, &["status", "--porcelain=v2", "--branch", "-uno"], Duration::from_secs(2)).ok()?;
    let root = git(cwd, &["rev-parse", "--show-toplevel"], Duration::from_secs(1)).ok()?;
    let mut info = parse_status(&out)?;
    info.root = PathBuf::from(root.trim());
    Some(info)
}

/// Parse `git status --porcelain=v2 --branch` output.
pub fn parse_status(text: &str) -> Option<GitInfo> {
    let mut branch = None;
    let mut oid = None;
    let (mut ahead, mut behind, mut dirty) = (0, 0, false);
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
        }
    }
    let branch = branch.or(oid)?;
    Some(GitInfo { root: PathBuf::new(), branch, dirty, ahead, behind })
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
    let base = match base {
        Some(b) => b.to_string(),
        None => git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"], Duration::from_secs(2))?.trim().to_string(),
    };
    let branch = format!("kraal/{name}");
    let path = worktrees_dir(&repo).join(name);
    if path.exists() {
        bail!("{} already exists", path.display());
    }
    let path_s = path.display().to_string();
    git(&repo, &["worktree", "add", "-b", &branch, &path_s, &base], Duration::from_secs(60))?;
    Ok(Worktree { repo, path, branch, base })
}

/// Remove a worktree and, if it is merged or `force`, its branch.
pub fn remove_worktree(w: &Worktree, force: bool) -> Result<()> {
    let path = w.path.display().to_string();
    let mut args = vec!["worktree", "remove", path.as_str()];
    if force {
        args.push("--force");
    }
    git(&w.repo, &args, Duration::from_secs(30))?;
    let flag = if force { "-D" } else { "-d" };
    git(&w.repo, &["branch", flag, &w.branch], Duration::from_secs(10))?;
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
            list.push(Worktree { repo: repo.clone(), path: p, branch: b.to_string(), base: String::new() });
        }
    }
    Ok(list)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain_v2() {
        let s = "# branch.oid 3f58e39c80027dcb\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +2 -1\n1 .M N... 100644 100644 100644 a b f\n";
        let i = parse_status(s).unwrap();
        assert_eq!((i.branch.as_str(), i.dirty, i.ahead, i.behind), ("main", true, 2, 1));
        assert_eq!(i.label(false), "⎇ main* ↑2 ↓1");
        assert_eq!(i.label(true), "git:main* +2 -1");
        let d = parse_status("# branch.oid 3f58e39c80027dcb\n# branch.head (detached)\n").unwrap();
        assert_eq!((d.branch.as_str(), d.dirty), ("3f58e39", false));
        assert!(parse_status("").is_none());
    }

    #[test]
    fn worktree_lifecycle() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("kraal-git-{}", std::process::id()));
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
        let wi = status(&w.path).unwrap();
        assert_eq!(wi.branch, "kraal/fix-login");
        std::fs::write(w.path.join("a.txt"), "2").unwrap();
        assert!(status(&w.path).unwrap().dirty);
        assert_eq!(list_worktrees(&repo).unwrap().len(), 1);
        assert!(add_worktree(&repo, "fix-login", None).is_err(), "names are unique");
        assert!(add_worktree(&repo, "../escape", None).is_err());
        remove_worktree(&w, true).unwrap();
        assert!(!w.path.exists());
        assert!(list_worktrees(&repo).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
}
