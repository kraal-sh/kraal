//! When the server asks git again about the directories panes are in. Not by the clock: when
//! something there may have changed (output in a pane in it, a command that ended, git writing
//! in its repository, the user coming back to the terminal), and otherwise rarely, for changes
//! made from elsewhere. Asking is what costs: `git status` reads the whole tree.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::watch::{self, Change, WatchId, Watcher};

/// A directory where panes print is looked at again at most this often.
pub(super) const ACTIVE_EVERY: Duration = Duration::from_secs(3);
/// After a sign of change (a command ended, git wrote, the user came back), at most this often.
pub(super) const SOON_EVERY: Duration = Duration::from_secs(1);
/// What git writes comes in bursts (a lock file, then the file renamed): it is looked at once
/// they settled.
pub(super) const SETTLE: Duration = Duration::from_millis(150);
/// With no sign of change, a directory whose repository is watched, or that is in none, is
/// looked at again this often (a file edited from elsewhere changes no git file). One in a
/// repository that cannot be watched is looked at as often as when panes print there.
pub(super) const QUIET_EVERY: Duration = Duration::from_secs(60);

#[derive(Debug, Default)]
struct Dir {
    /// When git was last asked about it.
    asked: Option<Instant>,
    pending: bool,
    /// Something changed while git was asked: ask again once it answers.
    again: bool,
    /// Asked about at this time (a change was noticed).
    due: Option<Instant>,
    /// It was in a repository, at the last answer.
    repo: bool,
    /// The directories its repository's git writes to, from the last answer.
    watch: Vec<PathBuf>,
}

impl Dir {
    /// Every one of its repository's directories is watched.
    fn watched(&self, watches: &HashMap<PathBuf, WatchId>) -> bool {
        !self.watch.is_empty() && self.watch.iter().all(|p| watches.contains_key(p))
    }
}

/// The directories git is asked about, and the watches on their repositories.
pub(super) struct GitWatch {
    dirs: HashMap<PathBuf, Dir>,
    watcher: Option<Watcher>,
    watches: HashMap<PathBuf, WatchId>,
    paths: HashMap<WatchId, Vec<PathBuf>>,
    /// The earliest `due`.
    next: Option<Instant>,
}

impl GitWatch {
    /// `watcher`: none where nothing can be watched (directories are then asked about as often
    /// as when panes print there).
    pub(super) fn new(watcher: Option<Watcher>) -> GitWatch {
        GitWatch { dirs: HashMap::new(), watcher, watches: HashMap::new(), paths: HashMap::new(), next: None }
    }

    /// When the next directory is due (see [`GitWatch::take_due`]).
    pub(super) fn next(&self) -> Option<Instant> {
        self.next
    }

    /// Whether to ask about `dir` now, a pane being there whose last output was at `output`:
    /// it was never asked about; or there was output since, and it was not asked about for
    /// [`ACTIVE_EVERY`]; or it was not for long.
    pub(super) fn periodic(&mut self, dir: &Path, output: Instant, now: Instant) -> bool {
        let watched = self.dirs.get(dir).is_some_and(|d| d.watched(&self.watches));
        let d = self.dirs.entry(dir.to_path_buf()).or_default();
        let Some(asked) = d.asked else { return !d.pending };
        let quiet_every = if d.repo && !watched { ACTIVE_EVERY } else { QUIET_EVERY };
        let since = now.saturating_duration_since(asked);
        !d.pending && ((output > asked && since >= ACTIVE_EVERY) || since >= quiet_every)
    }

    /// Something may have changed in `dir`: ask about it in `delay`, and not sooner than
    /// [`SOON_EVERY`] after it was last asked about.
    pub(super) fn soon(&mut self, dir: &Path, delay: Duration, now: Instant) {
        let d = self.dirs.entry(dir.to_path_buf()).or_default();
        let mut at = now + delay;
        if let Some(asked) = d.asked {
            at = at.max(asked + SOON_EVERY);
        }
        if d.due.is_none_or(|due| at < due) {
            d.due = Some(at);
        }
        self.next = Some(self.next.map_or(at, |n| n.min(at)));
    }

    /// The directories due by `now`.
    pub(super) fn take_due(&mut self, now: Instant) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut next: Option<Instant> = None;
        for (dir, d) in &mut self.dirs {
            match d.due {
                Some(at) if at <= now => {
                    d.due = None;
                    out.push(dir.clone());
                }
                Some(at) => next = Some(next.map_or(at, |n| n.min(at))),
                None => {}
            }
        }
        self.next = next;
        out
    }

    /// Git is to be asked about `dir`: `false` when it is being asked already. Then, with
    /// `again` (a change was noticed since it was asked), it is asked again once it answers.
    pub(super) fn asking(&mut self, dir: &Path, again: bool, now: Instant) -> bool {
        let d = self.dirs.entry(dir.to_path_buf()).or_default();
        if d.pending {
            d.again |= again;
            return false;
        }
        d.pending = true;
        d.asked = Some(now);
        d.due = None;
        true
    }

    /// Git answered about `dir`: in a repository (whose git writes to `watch`) or not. Its
    /// directories are watched from now on; one asked about again meanwhile is due soon.
    pub(super) fn answered(&mut self, dir: &Path, repo: bool, watch: &[PathBuf], now: Instant) {
        for path in watch {
            self.watch_path(path);
        }
        let d = self.dirs.entry(dir.to_path_buf()).or_default();
        d.pending = false;
        d.repo = repo;
        d.watch = watch.to_vec();
        if std::mem::take(&mut d.again) {
            self.soon(dir, Duration::ZERO, now);
        }
    }

    fn watch_path(&mut self, path: &Path) {
        if self.watches.contains_key(path) {
            return;
        }
        let Some(w) = &mut self.watcher else { return };
        if let Ok(id) = w.add(path) {
            self.watches.insert(path.to_path_buf(), id);
            self.paths.entry(id).or_default().push(path.to_path_buf());
        }
    }

    fn unwatch(&mut self, path: &Path) {
        let Some(id) = self.watches.remove(path) else { return };
        if let Some(w) = &mut self.watcher {
            w.remove(id);
        }
        if let Some(list) = self.paths.get_mut(&id) {
            list.retain(|p| p != path);
            if list.is_empty() {
                self.paths.remove(&id);
            }
        }
    }

    /// What the watcher saw: the directories whose repository git wrote to are due once it
    /// settled. A watch that went is dropped; the next answer watches again what is there.
    pub(super) fn changed(&mut self, changes: &[Change], now: Instant) {
        let mut paths: HashSet<PathBuf> = HashSet::new();
        let mut all = false;
        for c in changes {
            if c.id == watch::ANY {
                all = true;
                continue;
            }
            let list = self.paths.get(&c.id).cloned().unwrap_or_default();
            for p in list {
                if c.gone {
                    self.unwatch(&p);
                }
                paths.insert(p);
            }
        }
        let due: Vec<PathBuf> = self
            .dirs
            .iter()
            .filter(|(_, d)| all || d.watch.iter().any(|p| paths.contains(p)))
            .map(|(dir, _)| dir.clone())
            .collect();
        for dir in due {
            self.soon(&dir, SETTLE, now);
        }
    }

    /// Forget the directories no pane is in any more, and stop watching what only they needed.
    pub(super) fn retain(&mut self, live: &HashSet<PathBuf>) {
        // One being asked about stays until it is answered.
        self.dirs.retain(|dir, d| live.contains(dir) || d.pending);
        let needed: HashSet<&PathBuf> = self.dirs.values().flat_map(|d| &d.watch).collect();
        let unused: Vec<PathBuf> = self.watches.keys().filter(|p| !needed.contains(p)).cloned().collect();
        for p in unused {
            self.unwatch(&p);
        }
        if self.next.is_some() && self.dirs.values().all(|d| d.due.is_none()) {
            self.next = None;
        }
    }

    /// Every directory panes are in.
    pub(super) fn dirs(&self) -> impl Iterator<Item = &PathBuf> {
        self.dirs.keys()
    }

    #[cfg(test)]
    fn watched_paths(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = self.watches.keys().cloned().collect();
        v.sort();
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        PathBuf::from("/x").join(name)
    }

    #[test]
    fn a_directory_is_asked_about_first_then_when_panes_print_there() {
        let mut g = GitWatch::new(None);
        let t0 = Instant::now();
        let d = dir("repo");
        assert!(g.periodic(&d, t0, t0), "never asked");
        assert!(g.asking(&d, false, t0));
        assert!(!g.periodic(&d, t0, t0 + Duration::from_secs(10)), "not while git runs");
        g.answered(&d, false, &[], t0 + Duration::from_millis(50));
        // No output since: not before long.
        assert!(!g.periodic(&d, t0, t0 + Duration::from_secs(30)));
        assert!(g.periodic(&d, t0, t0 + QUIET_EVERY));
        // Output since: after ACTIVE_EVERY.
        let out = t0 + Duration::from_secs(1);
        assert!(!g.periodic(&d, out, t0 + Duration::from_secs(2)));
        assert!(g.periodic(&d, out, t0 + ACTIVE_EVERY));
    }

    #[test]
    fn a_repository_that_cannot_be_watched_is_asked_about_as_before() {
        let mut g = GitWatch::new(None);
        let t0 = Instant::now();
        let d = dir("repo");
        assert!(g.asking(&d, false, t0));
        // In a repository, but nothing watches its git directory.
        g.answered(&d, true, &[dir("repo/.git")], t0);
        assert!(g.periodic(&d, t0, t0 + ACTIVE_EVERY), "every few seconds, output or not");
    }

    #[test]
    fn signs_of_change_ask_soon_but_not_too_often() {
        let mut g = GitWatch::new(None);
        let t0 = Instant::now();
        let d = dir("repo");
        assert!(g.asking(&d, false, t0));
        g.answered(&d, true, &[], t0);
        // A command ended right after: not before SOON_EVERY since it was asked.
        g.soon(&d, Duration::ZERO, t0 + Duration::from_millis(100));
        assert_eq!(g.next(), Some(t0 + SOON_EVERY));
        assert!(g.take_due(t0 + Duration::from_millis(500)).is_empty());
        assert_eq!(g.take_due(t0 + SOON_EVERY), vec![d.clone()]);
        assert_eq!(g.next(), None);
        // Long after: at once, or once git's writes settled.
        let later = t0 + Duration::from_secs(30);
        g.soon(&d, SETTLE, later);
        assert_eq!(g.next(), Some(later + SETTLE));
        // An earlier sign wins.
        g.soon(&d, Duration::ZERO, later);
        assert_eq!(g.take_due(later), vec![d]);
    }

    #[test]
    fn a_change_while_git_runs_asks_again_once_it_answered() {
        let mut g = GitWatch::new(None);
        let t0 = Instant::now();
        let d = dir("repo");
        assert!(g.asking(&d, false, t0));
        // A commit while git status runs: its answer may be from before.
        assert!(!g.asking(&d, true, t0 + Duration::from_millis(10)));
        g.answered(&d, true, &[], t0 + Duration::from_millis(40));
        assert_eq!(g.take_due(t0 + SOON_EVERY), vec![d.clone()]);
        // Asked again without a change (an account rule waiting for git): nothing more.
        assert!(g.asking(&d, false, t0 + SOON_EVERY));
        assert!(!g.asking(&d, false, t0 + SOON_EVERY));
        g.answered(&d, true, &[], t0 + SOON_EVERY);
        assert_eq!(g.next(), None);
    }

    #[test]
    fn git_writing_in_a_watched_repository_asks_about_its_directories() {
        let root = std::env::temp_dir().join(format!("kraal-gitwatch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (git_dir, other_git) = (root.join("a/.git"), root.join("b/.git"));
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::create_dir_all(&other_git).unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        let watcher = Watcher::new(move |c| {
            let _ = tx.send(c);
        })
        .unwrap();
        let mut g = GitWatch::new(Some(watcher));
        let t0 = Instant::now();
        let (a, a_sub, b) = (root.join("a"), root.join("a/src"), root.join("b"));
        for (d, w) in [(&a, &git_dir), (&a_sub, &git_dir), (&b, &other_git)] {
            assert!(g.asking(d, false, t0));
            g.answered(d, true, std::slice::from_ref(w), t0);
        }
        assert_eq!(g.watched_paths(), vec![git_dir.clone(), other_git.clone()]);
        // Watched: not asked about again before long, output or not.
        assert!(!g.periodic(&a, t0, t0 + ACTIVE_EVERY));
        // Git replaces the index of `a`: both directories in that repository are due.
        std::fs::write(git_dir.join("index.lock"), "x").unwrap();
        std::fs::rename(git_dir.join("index.lock"), git_dir.join("index")).unwrap();
        let changes = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let now = t0 + Duration::from_secs(10);
        g.changed(&changes, now);
        assert_eq!(g.next(), Some(now + SETTLE));
        let mut due = g.take_due(now + SETTLE);
        due.sort();
        assert_eq!(due, vec![a.clone(), a_sub.clone()]);
        // A pane left `b`: its repository is not watched any more.
        g.retain(&[a.clone(), a_sub.clone()].into_iter().collect());
        assert_eq!(g.watched_paths(), vec![git_dir.clone()]);
        // The git directory goes (the repository deleted): no watch is left on it.
        std::fs::remove_dir_all(&git_dir).unwrap();
        let start = Instant::now();
        while !g.watched_paths().is_empty() {
            assert!(start.elapsed() < Duration::from_secs(5), "the watch did not go");
            if let Ok(changes) = rx.recv_timeout(Duration::from_millis(100)) {
                g.changed(&changes, now);
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
