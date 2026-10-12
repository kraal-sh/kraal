//! The directories panes are in: what changes there, told by the kernel (see [`crate::watch`]),
//! and when to ask git about them again. Not by the clock: when something there may have
//! changed (output in a pane in it, a command that ended, git writing in its repository, an
//! entry added or removed in it, the user coming back to the terminal), and otherwise rarely,
//! for changes made from elsewhere. Asking is what costs: `git status` reads the whole tree.
//! The same watches tell when the directories of kraal's configuration change.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::watch::{self, Change, WatchId, Watcher};

/// A directory where panes print is looked at again at most this often; so is one whose
/// entries change.
pub(super) const ACTIVE_EVERY: Duration = Duration::from_secs(3);
/// After a sign of change (a command ended, git wrote, the user came back), at most this often.
pub(super) const SOON_EVERY: Duration = Duration::from_secs(1);
/// What git writes comes in bursts (a lock file, then the file renamed): it is looked at once
/// they settled.
pub(super) const SETTLE: Duration = Duration::from_millis(150);
/// With no sign of change, a directory in a watched repository is looked at again this often:
/// a file edited from elsewhere changes nothing git writes. One in a repository that cannot be
/// watched is looked at as often as when panes print there.
pub(super) const QUIET_EVERY: Duration = Duration::from_secs(60);
/// And one in no repository whose entries are watched (a repository made there would show),
/// this often.
pub(super) const OUTSIDE_EVERY: Duration = Duration::from_secs(300);

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

/// What the watcher's changes are about.
#[derive(Debug, Default, PartialEq)]
pub(super) struct Touched {
    /// Directories panes are in whose entries changed.
    pub dirs: Vec<PathBuf>,
    /// A directory of the configuration changed.
    pub config: bool,
}

/// The directories panes are in, the watches on them, on their repositories and on the
/// configuration's directories.
pub(super) struct DirWatch {
    dirs: HashMap<PathBuf, Dir>,
    watcher: Option<Watcher>,
    watches: HashMap<PathBuf, WatchId>,
    paths: HashMap<WatchId, Vec<PathBuf>>,
    /// Directories that could not be watched (on a network file system, or the kernel said no):
    /// not tried again while they are needed.
    unwatchable: HashSet<PathBuf>,
    /// The configuration's directories as watched: each, or where it would be made.
    config: Vec<PathBuf>,
    /// The earliest `due`.
    next: Option<Instant>,
}

impl DirWatch {
    /// `watcher`: none where nothing can be watched (directories are then looked at as often as
    /// before there were watches).
    pub(super) fn new(watcher: Option<Watcher>) -> DirWatch {
        DirWatch {
            dirs: HashMap::new(),
            watcher,
            watches: HashMap::new(),
            paths: HashMap::new(),
            unwatchable: HashSet::new(),
            config: Vec::new(),
            next: None,
        }
    }

    /// When the next directory is due (see [`DirWatch::take_due`]).
    pub(super) fn next(&self) -> Option<Instant> {
        self.next
    }

    /// Whether the entries of `dir` are watched: a change there is told.
    pub(super) fn entries_watched(&self, dir: &Path) -> bool {
        self.watches.contains_key(dir)
    }

    /// Whether a directory panes are in is in a repository that cannot be watched: git is then
    /// asked about it every few seconds.
    pub(super) fn polled(&self) -> bool {
        self.dirs.values().any(|d| d.repo && !self.repo_watched(d))
    }

    fn repo_watched(&self, d: &Dir) -> bool {
        !d.watch.is_empty() && d.watch.iter().all(|p| self.watches.contains_key(p))
    }

    /// Whether to ask about `dir` now, a pane being there whose last output was at `output`:
    /// it was never asked about; or there was output since, and it was not asked about for
    /// [`ACTIVE_EVERY`]; or it was not for long. Its entries are watched from now on.
    pub(super) fn periodic(&mut self, dir: &Path, output: Instant, now: Instant) -> bool {
        self.watch_path(dir);
        let repo_watched = self.dirs.get(dir).is_some_and(|d| self.repo_watched(d));
        let own_watched = self.entries_watched(dir);
        let d = self.dirs.entry(dir.to_path_buf()).or_default();
        let Some(asked) = d.asked else { return !d.pending };
        let quiet_every = match (d.repo, repo_watched, own_watched) {
            (true, true, _) => QUIET_EVERY,
            (true, false, _) => ACTIVE_EVERY,
            (false, _, true) => OUTSIDE_EVERY,
            (false, _, false) => QUIET_EVERY,
        };
        let since = now.saturating_duration_since(asked);
        !d.pending && ((output > asked && since >= ACTIVE_EVERY) || since >= quiet_every)
    }

    /// Something may have changed in `dir`: ask about it in `delay`, and not sooner than `gap`
    /// after it was last asked about.
    pub(super) fn soon(&mut self, dir: &Path, delay: Duration, gap: Duration, now: Instant) {
        let d = self.dirs.entry(dir.to_path_buf()).or_default();
        let mut at = now + delay;
        if let Some(asked) = d.asked {
            at = at.max(asked + gap);
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
            self.soon(dir, Duration::ZERO, SOON_EVERY, now);
        }
    }

    /// Watch the configuration's directories `dirs`; one not there yet, where it would be made
    /// (its nearest parent that is).
    pub(super) fn watch_config(&mut self, dirs: &[PathBuf]) {
        let mut config = Vec::new();
        for dir in dirs {
            let there = dir.ancestors().find(|a| a.is_dir()).map(Path::to_path_buf);
            if let Some(d) = there.filter(|d| !config.contains(d)) {
                config.push(d);
            }
        }
        for d in &config {
            self.watch_path(d);
        }
        self.config = config;
        self.unwatch_unused();
    }

    /// Whether every directory of the configuration is watched: a change there is told.
    pub(super) fn config_watched(&self) -> bool {
        self.config.iter().all(|d| self.watches.contains_key(d))
    }

    fn watch_path(&mut self, path: &Path) {
        if self.watches.contains_key(path) || self.unwatchable.contains(path) {
            return;
        }
        let Some(w) = &mut self.watcher else { return };
        match watch::local(path).then(|| w.add(path)) {
            Some(Ok(id)) => {
                self.watches.insert(path.to_path_buf(), id);
                self.paths.entry(id).or_default().push(path.to_path_buf());
            }
            _ => {
                self.unwatchable.insert(path.to_path_buf());
            }
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

    /// What the watcher saw. The directories whose repository git wrote to are due once it
    /// settled; those whose entries changed are due too, and told; a watch that went is dropped
    /// (what is there is watched again when next needed).
    pub(super) fn changed(&mut self, changes: &[Change], now: Instant) -> Touched {
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
        let mut touched =
            Touched { config: all || self.config.iter().any(|d| paths.contains(d)), ..Default::default() };
        let mut due = Vec::new();
        for (dir, d) in &self.dirs {
            if all || paths.contains(dir) {
                touched.dirs.push(dir.clone());
                due.push((dir.clone(), ACTIVE_EVERY));
            } else if d.watch.iter().any(|p| paths.contains(p)) {
                due.push((dir.clone(), SOON_EVERY));
            }
        }
        for (dir, gap) in due {
            self.soon(&dir, SETTLE, gap, now);
        }
        touched
    }

    /// Forget the directories no pane is in any more, and stop watching what only they needed.
    pub(super) fn retain(&mut self, live: &HashSet<PathBuf>) {
        // One being asked about stays until it is answered.
        self.dirs.retain(|dir, d| live.contains(dir) || d.pending);
        self.unwatch_unused();
        if self.next.is_some() && self.dirs.values().all(|d| d.due.is_none()) {
            self.next = None;
        }
    }

    fn unwatch_unused(&mut self) {
        let needed: HashSet<&PathBuf> =
            self.dirs.iter().flat_map(|(dir, d)| std::iter::once(dir).chain(&d.watch)).chain(&self.config).collect();
        let unused: Vec<PathBuf> = self.watches.keys().filter(|p| !needed.contains(p)).cloned().collect();
        self.unwatchable.retain(|p| needed.contains(p));
        for p in unused {
            self.unwatch(&p);
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

    /// A watcher whose changes come out of the returned channel.
    fn watcher() -> (Watcher, crossbeam_channel::Receiver<Vec<Change>>) {
        let (tx, rx) = crossbeam_channel::unbounded();
        let w = Watcher::new(move |c| {
            let _ = tx.send(c);
        })
        .unwrap();
        (w, rx)
    }

    fn temp(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("kraal-dirwatch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root.canonicalize().unwrap()
    }

    #[test]
    fn a_directory_is_asked_about_first_then_when_panes_print_there() {
        let mut g = DirWatch::new(None);
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
        let mut g = DirWatch::new(None);
        let t0 = Instant::now();
        let d = dir("repo");
        assert!(g.asking(&d, false, t0));
        // In a repository, but nothing watches its git directory.
        g.answered(&d, true, &[dir("repo/.git")], t0);
        assert!(g.polled());
        assert!(g.periodic(&d, t0, t0 + ACTIVE_EVERY), "every few seconds, output or not");
    }

    #[test]
    fn signs_of_change_ask_soon_but_not_too_often() {
        let mut g = DirWatch::new(None);
        let t0 = Instant::now();
        let d = dir("repo");
        assert!(g.asking(&d, false, t0));
        g.answered(&d, true, &[], t0);
        // A command ended right after: not before SOON_EVERY since it was asked.
        g.soon(&d, Duration::ZERO, SOON_EVERY, t0 + Duration::from_millis(100));
        assert_eq!(g.next(), Some(t0 + SOON_EVERY));
        assert!(g.take_due(t0 + Duration::from_millis(500)).is_empty());
        assert_eq!(g.take_due(t0 + SOON_EVERY), vec![d.clone()]);
        assert_eq!(g.next(), None);
        // Long after: at once, or once git's writes settled.
        let later = t0 + Duration::from_secs(30);
        g.soon(&d, SETTLE, SOON_EVERY, later);
        assert_eq!(g.next(), Some(later + SETTLE));
        // An earlier sign wins.
        g.soon(&d, Duration::ZERO, SOON_EVERY, later);
        assert_eq!(g.take_due(later), vec![d]);
    }

    #[test]
    fn a_change_while_git_runs_asks_again_once_it_answered() {
        let mut g = DirWatch::new(None);
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
        let root = temp("repo");
        let (git_dir, other_git) = (root.join("a/.git"), root.join("b/.git"));
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::create_dir_all(&other_git).unwrap();
        let (a, a_sub, b) = (root.join("a"), root.join("a/src"), root.join("b"));
        std::fs::create_dir_all(&a_sub).unwrap();
        let (w, rx) = watcher();
        let mut g = DirWatch::new(Some(w));
        let t0 = Instant::now();
        for (d, w) in [(&a, &git_dir), (&a_sub, &git_dir), (&b, &other_git)] {
            assert!(g.periodic(d, t0, t0));
            assert!(g.asking(d, false, t0));
            g.answered(d, true, std::slice::from_ref(w), t0);
        }
        assert_eq!(g.watched_paths(), vec![a.clone(), git_dir.clone(), a_sub.clone(), b.clone(), other_git.clone()]);
        assert!(!g.polled());
        // Watched: not asked about again before long, output or not.
        assert!(!g.periodic(&a, t0, t0 + ACTIVE_EVERY));
        // Git replaces the index of `a`: both directories in that repository are due.
        std::fs::write(git_dir.join("index.lock"), "x").unwrap();
        std::fs::rename(git_dir.join("index.lock"), git_dir.join("index")).unwrap();
        let mut changes = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        while let Ok(more) = rx.recv_timeout(Duration::from_millis(100)) {
            changes.extend(more);
        }
        let now = t0 + Duration::from_secs(10);
        assert_eq!(g.changed(&changes, now), Touched::default(), "no pane's own directory changed");
        assert_eq!(g.next(), Some(now + SETTLE));
        let mut due = g.take_due(now + SETTLE);
        due.sort();
        assert_eq!(due, vec![a.clone(), a_sub.clone()]);
        // A pane left `b`: neither it nor its repository is watched any more.
        g.retain(&[a.clone(), a_sub.clone()].into_iter().collect());
        assert_eq!(g.watched_paths(), vec![a.clone(), git_dir.clone(), a_sub.clone()]);
        // The git directory goes (the repository deleted): no watch is left on it.
        std::fs::remove_dir_all(&git_dir).unwrap();
        let start = Instant::now();
        while g.watched_paths().contains(&git_dir) {
            assert!(start.elapsed() < Duration::from_secs(5), "the watch did not go");
            if let Ok(changes) = rx.recv_timeout(Duration::from_millis(100)) {
                g.changed(&changes, now);
            }
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_pane_directory_whose_entries_change_is_told() {
        let root = temp("entries");
        let (w, rx) = watcher();
        let mut g = DirWatch::new(Some(w));
        let t0 = Instant::now();
        assert!(g.periodic(&root, t0, t0));
        assert!(g.entries_watched(&root));
        assert!(g.asking(&root, false, t0));
        g.answered(&root, false, &[], t0);
        // Outside any repository, with its entries watched: not asked about again for long.
        assert!(!g.periodic(&root, t0, t0 + QUIET_EVERY));
        assert!(g.periodic(&root, t0, t0 + OUTSIDE_EVERY));
        // `terraform init`, `git init`: an entry appears.
        std::fs::create_dir(root.join(".terraform")).unwrap();
        let changes = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let now = t0 + Duration::from_secs(10);
        assert_eq!(g.changed(&changes, now), Touched { dirs: vec![root.clone()], config: false });
        assert_eq!(g.take_due(now + SETTLE), vec![root.clone()]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_configuration_is_watched_where_it_is_or_would_be() {
        let root = temp("config");
        let (w, rx) = watcher();
        let mut g = DirWatch::new(Some(w));
        let workspaces = root.join("kraal/workspaces");
        // Nothing there yet: what is there is watched, for the directory to come.
        g.watch_config(std::slice::from_ref(&workspaces));
        assert!(g.config_watched());
        assert_eq!(g.watched_paths(), vec![root.clone()]);
        std::fs::create_dir_all(&workspaces).unwrap();
        let changes = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(g.changed(&changes, Instant::now()).config);
        // Asked again (the server does after such a change): the directory itself now.
        g.watch_config(std::slice::from_ref(&workspaces));
        assert_eq!(g.watched_paths(), vec![workspaces.clone()]);
        while rx.recv_timeout(Duration::from_millis(200)).is_ok() {}
        std::fs::write(workspaces.join("w.kdl"), "layout {}\n").unwrap();
        let changes = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(g.changed(&changes, Instant::now()).config);
        let _ = std::fs::remove_dir_all(&root);
    }
}
