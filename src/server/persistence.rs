//! Saving the session to disk and bringing it back after a restart.

use std::time::Instant;

use anyhow::Result;
use crossbeam_channel::Sender;

use super::{Event, Server};
use crate::paths;
use crate::persist::{self, SessionSnapshot};
impl Server {
    pub(super) fn persist(&mut self) {
        self.persist_dirty = false;
        self.last_persist = Instant::now();
        if self.workspaces.is_empty() {
            return;
        }
        // Written by its own thread: the disk (fsync) never holds up the event loop.
        let snap = persist::snapshot(self);
        let _ = self.persister.send(Persist::Save(self.session.clone(), snap));
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
        while let Ok(first) = rx.recv() {
            let mut queue = vec![first];
            queue.extend(rx.try_iter());
            for (i, job) in queue.iter().enumerate() {
                match job {
                    // Of several saves in a row, only the latest matters.
                    Persist::Save(..) if matches!(queue.get(i + 1), Some(Persist::Save(..))) => {}
                    Persist::Save(session, snap) => {
                        last = persist::save(session, snap).map_err(|e| format!("{e:#}"));
                        if let Err(e) = &last {
                            let _ = events.send(Event::Warning(format!("saving session state: {e}")));
                        }
                    }
                    Persist::Delete(session) => {
                        let _ = std::fs::remove_file(paths::session_state_path(session));
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
