//! Saving the session to disk and bringing it back after a restart.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossbeam_channel::Sender;

use super::ui::{ConfirmAction, Mode};
use super::{ClientId, Event, EventSender, Server};
use crate::layout::PaneId;
use crate::paths;
use crate::persist::{self, SavedScrollback, SessionSnapshot};

/// A pane that keeps printing has its scrollback saved again at most this often.
const SCROLLBACK_EVERY: Duration = Duration::from_secs(10);
/// What saving scrollback may take of a tick, a pane at least: reading the panes' text is the
/// event loop's work, writing it the writer thread's.
const SCROLLBACK_BUDGET: Duration = Duration::from_millis(4);
/// The paste buffers' file is written again this long after a write of it failed.
const BUFFERS_RETRY: Duration = Duration::from_secs(10);

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
        if self.workspaces.is_empty() && self.unrestored.is_empty() {
            return;
        }
        // Turned off: nothing is saved, and what was goes once this state is written.
        if !self.config.save_scrollback {
            self.scrollback.clear();
        }
        let panes = &self.panes;
        self.scrollback.retain(|id, _| panes.contains_key(id));
        let snap = persist::snapshot(self);
        // The state the last save wrote, and that save worked: there is nothing to write.
        let text = serde_json::to_vec(&snap).ok();
        let failed = self.persisting.lock().map_or(true, |p| p.state_failed);
        if text.is_some() && text == self.persisted && !failed {
            return;
        }
        self.persisted = text;
        // Written by its own thread: the disk (fsync) never holds up the event loop.
        if let Ok(mut p) = self.persisting.lock() {
            p.saves += 1;
        }
        let _ = self.persister.send(Persist::Save(self.session.clone(), snap));
    }

    /// Have the session written as it is now, the panes' scrollback first, and wait until it
    /// is (10 s at most): whether all of it is on disk. The event loop waits meanwhile, as the
    /// server is about to stop.
    pub(super) fn save_now(&mut self) -> Result<(), String> {
        self.save_scrollback(true);
        self.persist();
        self.save_buffers(true);
        let (done, written) = crossbeam_channel::bounded(1);
        self.persister
            .send(Persist::Flush(done))
            .map_err(|_| "the layout could not be saved: its writer is gone".to_string())?;
        written
            .recv_timeout(Duration::from_secs(10))
            .unwrap_or_else(|_| Err("the layout could not be saved: its writer did not finish in time".into()))
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
            // A pane waiting for Enter shows what was restored in it, saved already. One whose
            // history is being stored is saved next time, rather than store it here.
            .filter(|p| p.spec.login.is_none() && p.is_terminal() && !p.suspended && (all || p.history_stored()))
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
        let removing = crate::threads::named("temps").spawn(|| {
            for dir in paths::written_dirs() {
                let _ = paths::remove_files_in(&dir, paths::abandoned_temp);
            }
        });
        if let Err(e) = removing {
            self.warnings.push(format!("cannot remove the temporary files a killed kraal left: {e}"));
        }
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
                let kept = keep_aside(&paths::session_state_path(&self.session));
                self.warnings.push(format!("could not restore the previous session ({e:#}); {kept}"));
                return;
            }
        };
        if let Err(e) = self.apply_snapshot(snap) {
            self.warnings.push(format!("restore: {e:#}"));
        }
    }

    pub(super) fn apply_snapshot(&mut self, snap: SessionSnapshot) -> Result<()> {
        // Every pane is set up before the programs start.
        self.with_batch(|server| persist::apply(server, snap))
    }

    /// The question before a restart, with what it stops.
    pub(super) fn ask_restart(&self) -> Mode {
        let mut message = format!(
            "Restart session {} on the kraal installed now? The layout, the scrollback, SSH shells and agents come back.",
            self.session
        );
        if let Some(stops) = self.stops() {
            message.push('\n');
            message.push_str(&stops);
        }
        Mode::confirm(message, ConfirmAction::Restart)
    }

    /// The question before quitting, with what stops.
    pub(super) fn ask_quit(&self) -> Mode {
        let message = match self.stops() {
            Some(stops) => format!("Quit kraal? {stops}\nThe layout is saved for next time."),
            None => "Quit kraal? The layout is saved for next time.".into(),
        };
        Mode::confirm(message, ConfirmAction::Quit)
    }

    /// What stopping the server (to restart, or to quit) stops, as a question says it: a line
    /// per program, in the order of the workspaces. `None` when nothing does.
    fn stops(&self) -> Option<String> {
        let mut lines = Vec::new();
        for id in self.workspaces.iter().flat_map(|w| w.panes()) {
            let Some(pane) = self.panes.get(&id) else { continue };
            let program = if pane.shell_waiting() { None } else { pane.foreground_argv() };
            let info = self.pane_info(id);
            for what in persist::stops_with_server(&info, program.as_deref(), &pane.background_jobs()) {
                lines.push(format!("\n  {}", persist::stopping_line(&info, &what)));
            }
        }
        (!lines.is_empty()).then(|| format!("These programs stop:{}", lines.concat()))
    }

    /// Quit: the server stops once the session is saved, as for `kraal kill`, and its clients
    /// end. A session that cannot be saved keeps running and says why, first among the warnings:
    /// those of the failed save, which come after, say the same, and must not hide the answer to
    /// the key just pressed.
    pub(super) fn quit(&mut self) {
        match self.save_now() {
            Ok(()) => self.quitting = true,
            Err(e) => self.warnings.insert(0, format!("not quitting: {e}")),
        }
    }

    /// Restart the session: a helper started from the kraal installed now stops this server and
    /// starts the next one, which every attached client waits for.
    pub(super) fn restart(&mut self, client: ClientId) {
        let started = crate::client::installed_exe()
            .and_then(|exe| crate::client::restart_detached(&exe, &self.session))
            .map(crate::exec::reap_later);
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
    /// The paste buffers' file, as [`Persisting`] last has it, if an earlier job did not write
    /// it already.
    Buffers,
    /// The session ended with its last pane: nothing is left to restore.
    Delete(String),
    /// Everything asked before is written: say whether the last save of the state, of each
    /// pane's scrollback and of the paste buffers worked.
    Flush(Sender<Result<(), String>>),
}

/// What the event loop and the writer share beside the jobs, so that a disk slow to write never
/// piles up what waits for it: a pane's latest text only, the latest paste buffers, and one
/// state at a time.
#[derive(Default)]
pub(super) struct Persisting {
    /// The text each scrollback file is to have, waiting for the writer.
    texts: HashMap<String, Vec<u8>>,
    /// What the paste buffers' file is to be, waiting for the writer.
    buffers: Option<BuffersFile>,
    /// States sent to the writer and not written yet.
    saves: usize,
    /// Scrollback files the writer could not write: their panes are saved again.
    failed: Vec<String>,
    /// The last state written could not be: the next one is written even when the same.
    state_failed: bool,
    /// When the last write of the paste buffers' file failed: it is written again.
    buffers_failed: Option<Instant>,
}

/// The paste buffers' file and what it is to hold; `None` deletes it.
type BuffersFile = (std::path::PathBuf, Option<Vec<u8>>);

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

    /// What the paste buffers' file is to be: what it was to be before, if not written yet, is
    /// dropped, and the job that was to write it writes this. Whether a job is needed.
    pub(super) fn offer_buffers(&mut self, file: BuffersFile) -> bool {
        self.buffers.replace(file).is_none()
    }

    /// Whether the paste buffers' file is to be written again: its last write failed, at least
    /// [`BUFFERS_RETRY`] ago or right before the server stops.
    pub(super) fn retry_buffers(&mut self, stopping: bool) -> bool {
        self.buffers_failed.take_if(|at| stopping || at.elapsed() >= BUFFERS_RETRY).is_some()
    }
}

/// The only writer of session state, so writes land in order: a late save never overwrites the
/// final one, nor brings back a session whose state was deleted. It keeps `held`, what holds the
/// session for this server, until it has written its last: a server that stops without waiting
/// for a write that is stuck does not let go of the session while that write may still land.
pub(super) fn spawn_persister(
    events: EventSender,
    shared: Arc<Mutex<Persisting>>,
    held: impl Send + 'static,
) -> std::io::Result<Sender<Persist>> {
    let (tx, rx) = crossbeam_channel::unbounded::<Persist>();
    crate::threads::named("persist").spawn(move || {
        let _held = held;
        let mut last: Result<(), String> = Ok(());
        // Scrollback files whose last write failed, and why.
        let mut failing: HashMap<String, String> = HashMap::new();
        // Why the last write of the paste buffers' file failed.
        let mut buffers_failing: Option<String> = None;
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
                        if let Ok(mut s) = shared.lock() {
                            s.state_failed = last.is_err();
                        }
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
                    Persist::Buffers => {
                        let Some((path, data)) = shared.lock().ok().and_then(|mut s| s.buffers.take()) else {
                            continue;
                        };
                        let written = super::buffers::write_file(&path, data.as_deref());
                        if let Ok(mut s) = shared.lock() {
                            s.buffers_failed = written.is_err().then(Instant::now);
                        }
                        match written {
                            Ok(()) => buffers_failing = None,
                            Err(e) => {
                                // Said once, not at every try while the disk stays full.
                                if buffers_failing.is_none() {
                                    let _ = events.send(Event::Warning(format!("keeping the paste buffers: {e}")));
                                }
                                buffers_failing = Some(e.to_string());
                            }
                        }
                    }
                    Persist::Delete(session) => {
                        let _ = std::fs::remove_file(paths::session_state_path(session));
                        let _ = persist::forget_scrollback(session);
                        last = Ok(());
                        failing.clear();
                    }
                    Persist::Flush(done) => {
                        let _ = done.send(flush_answer(&last, &failing, buffers_failing.as_deref()));
                    }
                }
            }
        }
    })?;
    Ok(tx)
}

/// What the writer says when asked whether everything is written: whether the state, each
/// pane's scrollback and the paste buffers are saved, said whole.
fn flush_answer(
    last: &Result<(), String>,
    failing: &HashMap<String, String>,
    buffers_failing: Option<&str>,
) -> Result<(), String> {
    if let Err(e) = last {
        return Err(format!("the layout could not be saved: {e}"));
    }
    match (failing.values().next(), buffers_failing) {
        (Some(e), _) if failing.len() == 1 => Err(format!("the scrollback of a pane could not be saved: {e}")),
        (Some(e), _) => Err(format!("the scrollback of {} panes could not be saved: {e}", failing.len())),
        (None, Some(e)) => Err(format!("the paste buffers could not be saved: {e}")),
        (None, None) => Ok(()),
    }
}

/// Move the state at `path`, which could not be restored, aside rather than overwrite it with
/// this new session; what became of it, to tell the user.
fn keep_aside(path: &std::path::Path) -> String {
    let aside = path.with_extension(format!("json.unreadable-{}", crate::accounts::now()));
    match std::fs::rename(path, &aside) {
        Ok(()) => format!("kept it as {}", paths::shorten_home(&aside)),
        Err(e) => format!("it could not be kept aside ({e}): saving this session replaces it"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The writer keeps what holds the session for as long as it can be asked to write, and
    /// lets go of it as it ends: never while a write of its own may still land.
    #[test]
    fn the_writer_keeps_the_session_until_it_ends() {
        use std::sync::atomic::{AtomicBool, Ordering};
        struct Held(Arc<AtomicBool>);
        impl Drop for Held {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let let_go = Arc::new(AtomicBool::new(false));
        let (events, _events) = crate::server::event_channel(4096);
        let writer = spawn_persister(events, Arc::default(), Held(let_go.clone())).unwrap();
        let (done, written) = crossbeam_channel::bounded(1);
        writer.send(Persist::Flush(done)).unwrap();
        assert_eq!(written.recv_timeout(crate::testing::PATIENCE).unwrap(), Ok(()));
        assert!(!let_go.load(Ordering::SeqCst), "let go while it could still be asked to write");
        drop(writer);
        let start = Instant::now();
        while !let_go.load(Ordering::SeqCst) {
            assert!(start.elapsed() < crate::testing::PATIENCE, "kept after its last write");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn an_unreadable_state_is_kept_aside_or_said_not_to_be() {
        let dir = std::env::temp_dir().join(format!("kraal-unreadable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let state = dir.join("main.json");
        std::fs::write(&state, "{").unwrap();
        let kept = keep_aside(&state);
        assert!(kept.starts_with("kept it as ") && !state.exists(), "{kept}");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let lost = keep_aside(&state);
        assert!(lost.starts_with("it could not be kept aside"), "{lost}");
        let _ = std::fs::remove_dir_all(dir);
    }

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
    fn a_slow_disk_keeps_the_latest_paste_buffers_waiting() {
        let mut p = Persisting::default();
        let path = std::path::PathBuf::from("main.json");
        // Buffers changed again and again while the writer is stuck: one job, the latest data.
        let mut jobs = 0;
        for i in 0..100u8 {
            jobs += usize::from(p.offer_buffers((path.clone(), Some(vec![i; 1 << 20]))));
        }
        let waiting = p.buffers.as_ref().and_then(|(_, data)| data.as_ref()).map(|d| (d.len(), d[0]));
        assert_eq!((waiting, jobs), (Some((1 << 20, 99)), 1));
        // Turned off meanwhile: the file goes, nothing older is written.
        assert!(!p.offer_buffers((path.clone(), None)));
        assert_eq!(p.buffers.take(), Some((path.clone(), None)));
        assert!(p.offer_buffers((path, None)), "once taken, the next needs a job");
    }

    #[test]
    fn a_failed_write_of_the_paste_buffers_is_tried_again() {
        let mut p = Persisting::default();
        assert!(!p.retry_buffers(true), "nothing failed");
        p.buffers_failed = Some(Instant::now());
        assert!(!p.retry_buffers(false), "not at once");
        assert!(p.retry_buffers(true), "at once when the server stops");
        assert!(!p.retry_buffers(true), "once");
        p.buffers_failed = Instant::now().checked_sub(BUFFERS_RETRY);
        assert!(p.retry_buffers(false));
    }

    #[test]
    fn the_writer_says_what_could_not_be_saved() {
        let none = HashMap::new();
        assert_eq!(flush_answer(&Ok(()), &none, None), Ok(()));
        let one = HashMap::from([("a.ans".to_string(), "No space left on device".to_string())]);
        let said = flush_answer(&Ok(()), &one, None).unwrap_err();
        assert_eq!(said, "the scrollback of a pane could not be saved: No space left on device");
        let state = flush_answer(&Err("read-only file system".into()), &one, None).unwrap_err();
        assert_eq!(state, "the layout could not be saved: read-only file system");
        let buffers = flush_answer(&Ok(()), &none, Some("Not a directory")).unwrap_err();
        assert_eq!(buffers, "the paste buffers could not be saved: Not a directory");
    }
}
