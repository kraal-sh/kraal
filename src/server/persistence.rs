//! Saving the session to disk and bringing it back after a restart.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossbeam_channel::Sender;

use super::ui::{ConfirmAction, Mode};
use super::{ClientId, Event, Server};
use crate::layout::PaneId;
use crate::paths;
use crate::persist::{self, SavedScrollback, SessionSnapshot};

/// A pane that keeps printing has its scrollback saved again at most this often.
const SCROLLBACK_EVERY: Duration = Duration::from_secs(10);
/// What saving scrollback may take of a tick, a pane at least: reading the panes' text is the
/// event loop's work, writing it the writer thread's.
const SCROLLBACK_BUDGET: Duration = Duration::from_millis(4);

impl Server {
    /// [`Server::persist`], unless the writer has not written the last state yet (a slow disk):
    /// then this one goes once that one is written, with what changed meanwhile, so that states
    /// never pile up.
    pub(super) fn persist_unless_writing(&mut self) {
        if self.persisting.lock().is_ok_and(|p| p.saves > 0) {
            return;
        }
        self.persist();
    }

    /// Have the state written, after what the writer still has to write.
    pub(super) fn persist(&mut self) {
        self.persist_dirty = false;
        self.last_persist = Instant::now();
        if self.workspaces.is_empty() {
            return;
        }
        // Turned off: nothing is saved, and what was goes once this state is written.
        if !self.config.save_scrollback {
            self.scrollback.clear();
        }
        let panes = &self.panes;
        self.scrollback.retain(|id, _| panes.contains_key(id));
        // Written by its own thread: the disk (fsync) never holds up the event loop.
        let snap = persist::snapshot(self);
        if let Ok(mut p) = self.persisting.lock() {
            p.saves += 1;
        }
        let _ = self.persister.send(Persist::Save(self.session.clone(), snap));
    }

    /// Save the scrollback of panes that printed since it was last saved, or whose last save
    /// failed, the oldest saved first: all of them when `all` (the server stops), else for a
    /// few milliseconds, and a pane at most every [`SCROLLBACK_EVERY`]. A pane's file is written
    /// before the state names it.
    pub(super) fn save_scrollback(&mut self, all: bool) {
        if !self.config.save_scrollback || self.workspaces.is_empty() {
            return;
        }
        let failed = self.persisting.lock().map(|mut p| std::mem::take(&mut p.failed)).unwrap_or_default();
        for s in self.scrollback.values_mut() {
            s.retry |= s.file.as_ref().is_some_and(|f| failed.contains(f));
        }
        let start = Instant::now();
        let mut due: Vec<(Option<Instant>, PaneId)> = self
            .panes
            .values()
            // A pane waiting for Enter shows what was restored in it, saved already.
            .filter(|p| p.is_terminal() && !p.suspended)
            .filter_map(|p| {
                let saved = self.scrollback.get(&p.id);
                let taken = saved.map(|s| s.taken);
                let changed = saved.is_none_or(|s| s.retry || p.last_output > s.taken);
                let ripe = all || taken.is_none_or(|t| start >= t + SCROLLBACK_EVERY);
                (changed && ripe).then_some((taken, p.id))
            })
            .collect();
        due.sort();
        for (_, id) in due {
            if !all && start.elapsed() > SCROLLBACK_BUDGET {
                break;
            }
            let taken = Instant::now();
            // None while a full-screen program shows: the pane is saved once it leaves.
            let Some(text) = self.panes.get(&id).and_then(|p| p.scrollback_ansi()) else { continue };
            let had = self.scrollback.get(&id).and_then(|s| s.file.clone());
            if text.is_empty() {
                // Nothing to show any more: the state stops naming its file.
                self.persist_dirty |= had.is_some();
                self.scrollback.insert(id, SavedScrollback { file: None, taken, retry: false });
                continue;
            }
            self.persist_dirty |= had.is_none();
            let file = had.unwrap_or_else(|| format!("{}-{id}.ans", self.scrollback_tag));
            // Only the latest text of a file waits for the writer, with one job to write it.
            if self.persisting.lock().is_ok_and(|mut p| p.offer(&file, text)) {
                let _ = self.persister.send(Persist::Scrollback { session: self.session.clone(), file: file.clone() });
            }
            self.scrollback.insert(id, SavedScrollback { file: Some(file), taken, retry: false });
        }
    }

    pub fn mark_persist(&mut self) {
        self.persist_dirty = true;
    }

    /// Recreate the layout saved by a previous server, if any.
    pub(super) fn restore(&mut self) {
        // What a kraal killed while it wrote left: the temporary files of its writes, off the
        // event loop.
        let _ = std::thread::Builder::new().name("temps".into()).spawn(|| {
            for dir in paths::written_dirs() {
                let _ = paths::remove_files_in(&dir, paths::abandoned_temp);
            }
        });
        let interrupted = crate::runbooks::recover_interrupted(&self.session);
        if !interrupted.is_empty() {
            self.warnings.push(format!(
                "kraal stopped while these runbooks ran: {} (their logs say how far they got)",
                interrupted.join(", ")
            ));
        }
        let snap = match persist::load(&self.session) {
            Ok(Some(s)) => s,
            Ok(None) => return,
            Err(e) => {
                // Keep the unreadable file aside rather than overwrite it with this new session.
                let path = paths::session_state_path(&self.session);
                let aside = path.with_extension(format!("json.unreadable-{}", crate::accounts::now()));
                let _ = std::fs::rename(&path, &aside);
                self.warnings.push(format!(
                    "could not restore the previous session ({e:#}); kept it as {}",
                    paths::shorten_home(&aside)
                ));
                return;
            }
        };
        if let Err(e) = self.apply_snapshot(snap) {
            self.warnings.push(format!("restore: {e:#}"));
        }
    }

    pub(super) fn apply_snapshot(&mut self, snap: SessionSnapshot) -> Result<()> {
        persist::apply(self, snap)
    }

    /// The question before a restart, with what it stops.
    pub(super) fn ask_restart(&self) -> Mode {
        let mut message = format!(
            "Restart session {} on the kraal installed now? The layout, the scrollback, SSH shells and agents come back.",
            self.session
        );
        let stops = self.restart_stops();
        if !stops.is_empty() {
            message.push_str("\nThese programs stop:");
            for line in stops {
                message.push_str("\n  ");
                message.push_str(&line);
            }
        }
        Mode::confirm(message, ConfirmAction::Restart)
    }

    /// What a restart stops, a line per pane, in the order of the workspaces.
    fn restart_stops(&self) -> Vec<String> {
        let ids: Vec<PaneId> = self.workspaces.iter().flat_map(|w| w.panes()).collect();
        ids.into_iter()
            .filter_map(|id| {
                let pane = self.panes.get(&id)?;
                let program = if pane.shell_waiting() { None } else { pane.foreground_argv() };
                let info = self.pane_info(id);
                let what = persist::stops_on_restart(&info, program.as_deref())?;
                Some(persist::stopping_line(&info, &what))
            })
            .collect()
    }

    /// Restart the session: a helper started from the kraal installed now stops this server and
    /// starts the next one, which every attached client waits for.
    pub(super) fn restart(&mut self, client: ClientId) {
        let started =
            crate::client::installed_exe().and_then(|exe| crate::client::restart_detached(&exe, &self.session));
        match started {
            Ok(()) => self.toast(client, "restarting the session…"),
            Err(e) => self.toast(client, format!("cannot restart: {e:#}")),
        }
    }
}

/// What the writer thread does, in the order asked.
pub(super) enum Persist {
    Save(String, SessionSnapshot),
    /// A pane's scrollback, into its file (the state names it once written): the latest text
    /// [`Persisting`] has for that file, if an earlier job did not write it already.
    Scrollback {
        session: String,
        file: String,
    },
    /// The paste buffers, into their private file; `None` deletes it.
    Buffers(std::path::PathBuf, Option<Vec<u8>>),
    /// The session ended with its last pane: nothing is left to restore.
    Delete(String),
    /// Everything asked before is written: say whether the last save of the state and of each
    /// pane's scrollback worked, and stop.
    Stop(Sender<Result<(), String>>),
}

/// What the event loop and the writer share beside the jobs, so that a disk slow to write never
/// piles up what waits for it: a pane's latest text only, and one state at a time.
#[derive(Default)]
pub(super) struct Persisting {
    /// The text each scrollback file is to have, waiting for the writer.
    texts: HashMap<String, Vec<u8>>,
    /// States sent to the writer and not written yet.
    saves: usize,
    /// Scrollback files the writer could not write: their panes are saved again.
    failed: Vec<String>,
}

impl Persisting {
    /// `text` is what `file` is to have: the text it was to have before, if not written yet,
    /// is dropped, and the job that was to write it writes this one. Whether a job is needed.
    fn offer(&mut self, file: &str, text: Vec<u8>) -> bool {
        self.texts.insert(file.to_string(), text).is_none()
    }

    /// The text `file` is to have, unless an earlier job wrote it already.
    fn take(&mut self, file: &str) -> Option<Vec<u8>> {
        self.texts.remove(file)
    }
}

/// The only writer of session state, so writes land in order: a late save never overwrites the
/// final one, nor brings back a session whose state was deleted.
pub(super) fn spawn_persister(events: Sender<Event>, shared: Arc<Mutex<Persisting>>) -> Sender<Persist> {
    let (tx, rx) = crossbeam_channel::unbounded::<Persist>();
    let _ = std::thread::Builder::new().name("persist".into()).spawn(move || {
        let mut last: Result<(), String> = Ok(());
        // Scrollback files whose last write failed, and why.
        let mut failing: HashMap<String, String> = HashMap::new();
        while let Ok(first) = rx.recv() {
            let mut queue = vec![first];
            queue.extend(rx.try_iter());
            for (i, job) in queue.iter().enumerate() {
                match job {
                    Persist::Save(session, snap) => {
                        if let Ok(mut s) = shared.lock() {
                            s.saves = s.saves.saturating_sub(1);
                        }
                        // Of several saves in a row, only the latest matters.
                        if matches!(queue.get(i + 1), Some(Persist::Save(..))) {
                            continue;
                        }
                        last = persist::save(session, snap).map_err(|e| format!("{e:#}"));
                        match &last {
                            Err(e) => {
                                let _ = events.send(Event::Warning(format!("saving session state: {e}")));
                            }
                            // The files it does not name are no pane's any more.
                            Ok(()) => {
                                persist::clean_scrollback(session, snap);
                                let named = persist::scrollback_files(snap);
                                failing.retain(|f, _| named.contains(f.as_str()));
                            }
                        }
                    }
                    Persist::Scrollback { session, file } => {
                        let Some(text) = shared.lock().ok().and_then(|mut s| s.take(file)) else { continue };
                        match persist::save_scrollback(session, file, &text) {
                            Ok(()) => {
                                failing.remove(file);
                            }
                            Err(e) => {
                                // Said once, not at every save while the disk stays full.
                                if failing.is_empty() {
                                    let _ = events.send(Event::Warning(format!("saving a pane's scrollback: {e:#}")));
                                }
                                failing.insert(file.clone(), format!("{e:#}"));
                                if let Ok(mut s) = shared.lock() {
                                    s.failed.push(file.clone());
                                }
                            }
                        }
                    }
                    Persist::Buffers(..) if queue[i + 1..].iter().any(|j| matches!(j, Persist::Buffers(..))) => {}
                    Persist::Buffers(path, data) => {
                        if let Err(e) = super::buffers::write_file(path, data.as_deref()) {
                            let _ = events.send(Event::Warning(format!("keeping the paste buffers: {e}")));
                        }
                    }
                    Persist::Delete(session) => {
                        let _ = std::fs::remove_file(paths::session_state_path(session));
                        let _ = persist::forget_scrollback(session);
                        last = Ok(());
                        failing.clear();
                    }
                    Persist::Stop(done) => {
                        let _ = done.send(stop_answer(&last, &failing));
                        return;
                    }
                }
            }
        }
    });
    tx
}

/// What the writer says when the server stops: whether the state and each pane's scrollback
/// are saved, said whole.
fn stop_answer(last: &Result<(), String>, failing: &HashMap<String, String>) -> Result<(), String> {
    if let Err(e) = last {
        return Err(format!("the layout could not be saved: {e}"));
    }
    match failing.values().next() {
        Some(e) if failing.len() == 1 => Err(format!("the scrollback of a pane could not be saved: {e}")),
        Some(e) => Err(format!("the scrollback of {} panes could not be saved: {e}", failing.len())),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slow_disk_keeps_one_text_per_file_waiting() {
        let mut p = Persisting::default();
        // Panes saved every ten seconds while the writer is stuck: their older texts go, and
        // one job each waits.
        let mut jobs = 0;
        for i in 0..100u8 {
            jobs += usize::from(p.offer("1-2-3.ans", vec![i; 1 << 20]));
            jobs += usize::from(p.offer("1-2-4.ans", vec![i; 10]));
        }
        let waiting: usize = p.texts.values().map(Vec::len).sum();
        assert_eq!((waiting, jobs), ((1 << 20) + 10, 2));
        assert_eq!(p.take("1-2-3.ans").map(|t| t[0]), Some(99), "the latest is written");
        assert_eq!(p.take("1-2-3.ans"), None);
        assert!(p.offer("1-2-3.ans", Vec::new()), "once written, the next text needs a job");
    }

    #[test]
    fn stopping_says_what_could_not_be_saved() {
        let none = HashMap::new();
        assert_eq!(stop_answer(&Ok(()), &none), Ok(()));
        let one = HashMap::from([("a.ans".to_string(), "No space left on device".to_string())]);
        let said = stop_answer(&Ok(()), &one).unwrap_err();
        assert_eq!(said, "the scrollback of a pane could not be saved: No space left on device");
        let state = stop_answer(&Err("read-only file system".into()), &one).unwrap_err();
        assert_eq!(state, "the layout could not be saved: read-only file system");
    }
}
