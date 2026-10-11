//! Saving the session to disk and bringing it back after a restart.

use std::time::{Duration, Instant};

use anyhow::Result;
use crossbeam_channel::Sender;

use super::{Event, Server};
use crate::layout::PaneId;
use crate::paths;
use crate::persist::{self, SavedScrollback, SessionSnapshot};

/// A pane that keeps printing has its scrollback saved again at most this often.
const SCROLLBACK_EVERY: Duration = Duration::from_secs(10);
/// What saving scrollback may take of a tick, a pane at least: reading the panes' text is the
/// event loop's work, writing it the writer thread's.
const SCROLLBACK_BUDGET: Duration = Duration::from_millis(4);

impl Server {
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
        let _ = self.persister.send(Persist::Save(self.session.clone(), snap));
    }

    /// Save the scrollback of panes that printed since it was last saved, the oldest saved
    /// first: all of them when `all` (the server stops), else for a few milliseconds, and a pane
    /// at most every [`SCROLLBACK_EVERY`]. A pane's file is written before the state names it.
    pub(super) fn save_scrollback(&mut self, all: bool) {
        if !self.config.save_scrollback || self.workspaces.is_empty() {
            return;
        }
        let start = Instant::now();
        let mut due: Vec<(Option<Instant>, PaneId)> = self
            .panes
            .values()
            // A pane waiting for Enter shows what was restored in it, saved already.
            .filter(|p| p.is_terminal() && !p.suspended)
            .filter_map(|p| {
                let taken = self.scrollback.get(&p.id).map(|s| s.taken);
                let changed = taken.is_none_or(|t| p.last_output > t);
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
                self.scrollback.insert(id, SavedScrollback { file: None, taken });
                continue;
            }
            self.persist_dirty |= had.is_none();
            let file = had.unwrap_or_else(|| format!("{}-{id}.ans", self.scrollback_tag));
            let job = Persist::Scrollback { session: self.session.clone(), file: file.clone(), text };
            let _ = self.persister.send(job);
            self.scrollback.insert(id, SavedScrollback { file: Some(file), taken });
        }
    }

    pub fn mark_persist(&mut self) {
        self.persist_dirty = true;
    }

    /// Recreate the layout saved by a previous server, if any.
    pub(super) fn restore(&mut self) {
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
}

/// What the writer thread does, in the order asked.
pub(super) enum Persist {
    Save(String, SessionSnapshot),
    /// A pane's scrollback, into its file (the state names it once written).
    Scrollback {
        session: String,
        file: String,
        text: Vec<u8>,
    },
    /// The session ended with its last pane: nothing is left to restore.
    Delete(String),
    /// Everything asked before is written: say whether the last save worked, and stop.
    Stop(Sender<Result<(), String>>),
}

/// The only writer of session state, so writes land in order: a late save never overwrites the
/// final one, nor brings back a session whose state was deleted.
pub(super) fn spawn_persister(events: Sender<Event>) -> Sender<Persist> {
    let (tx, rx) = crossbeam_channel::unbounded::<Persist>();
    let _ = std::thread::Builder::new().name("persist".into()).spawn(move || {
        let mut last: Result<(), String> = Ok(());
        let mut scrollback_failed = false;
        while let Ok(first) = rx.recv() {
            let mut queue = vec![first];
            queue.extend(rx.try_iter());
            for (i, job) in queue.iter().enumerate() {
                match job {
                    // Of several saves in a row, only the latest matters.
                    Persist::Save(..) if matches!(queue.get(i + 1), Some(Persist::Save(..))) => {}
                    Persist::Save(session, snap) => {
                        last = persist::save(session, snap).map_err(|e| format!("{e:#}"));
                        match &last {
                            Err(e) => {
                                let _ = events.send(Event::Warning(format!("saving session state: {e}")));
                            }
                            // The files it does not name are no pane's any more.
                            Ok(()) => persist::clean_scrollback(session, snap),
                        }
                    }
                    // The same pane's newer text comes before the next state.
                    Persist::Scrollback { session, file, .. }
                        if queue[i + 1..].iter().take_while(|j| !matches!(j, Persist::Save(..))).any(|j| {
                            matches!(j, Persist::Scrollback { session: s, file: f, .. } if s == session && f == file)
                        }) => {}
                    Persist::Scrollback { session, file, text } => {
                        let written = persist::save_scrollback(session, file, text);
                        // Said once, not at every save while the disk stays full.
                        if let Err(e) = &written
                            && !scrollback_failed
                        {
                            let _ = events.send(Event::Warning(format!("saving a pane's scrollback: {e:#}")));
                        }
                        scrollback_failed = written.is_err();
                    }
                    Persist::Delete(session) => {
                        let _ = std::fs::remove_file(paths::session_state_path(session));
                        let _ = persist::forget_scrollback(session);
                        last = Ok(());
                    }
                    Persist::Stop(done) => {
                        let _ = done.send(last.clone());
                        return;
                    }
                }
            }
        }
    });
    tx
}
