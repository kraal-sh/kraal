//! Saving the session to disk and bringing it back after a restart.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use crossbeam_channel::Sender;

use super::ui::{ConfirmAction, Mode, Question};
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

/// What a workspace close approves, independently of its shortened display labels.
#[derive(Debug, PartialEq, Eq)]
pub struct CloseInventory {
    name: String,
    panes: Vec<ClosePane>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ClosePane {
    id: PaneId,
    leader: Option<CloseProcess>,
    foreground: Option<CloseProcess>,
    background: Vec<CloseProcess>,
    members: Vec<CloseProcess>,
}

impl ClosePane {
    pub(super) fn read(pane: &crate::pane::Pane) -> Self {
        let mut members = pane.process_hold().map(|p| p.session_members()).unwrap_or_default();
        for pid in members.clone() {
            members.extend(crate::process::descendants(pid));
        }
        members.sort_unstable();
        members.dedup();
        Self {
            id: pane.id,
            leader: pane.pid().map(CloseProcess::read),
            foreground: pane.foreground_pid().map(CloseProcess::read),
            background: pane
                .pid()
                .map(|pid| crate::process::background_processes(pid, &[]))
                .unwrap_or_default()
                .into_iter()
                .map(CloseProcess::read)
                .collect(),
            members: members.into_iter().map(CloseProcess::read).collect(),
        }
    }

    pub(super) fn running(&self) -> bool {
        !self.members.is_empty()
    }

    pub(super) fn commands(&self) -> Vec<String> {
        self.members.iter().filter_map(|p| p.command.as_ref().map(|argv| crate::shell::join(argv))).collect()
    }
}

#[derive(Debug, PartialEq, Eq)]
struct CloseProcess {
    pid: u32,
    started: Option<u64>,
    command: Option<Vec<String>>,
}

impl CloseProcess {
    fn read(pid: u32) -> Self {
        Self { pid, started: crate::process::start_time(pid), command: crate::process::argv(pid) }
    }
}

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
        // Nothing to save before a session exists. Once one was queued, an empty state may
        // need to replace it, including a rollback of the first workspace's creation.
        if self.workspaces.is_empty()
            && self.workspace_order.is_empty()
            && self.unrestored.is_empty()
            && self.deleted_workspaces.is_empty()
            && self.removed_sources.is_empty()
            && self.persisted.is_none()
        {
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
    pub(super) fn restore(&mut self) -> Result<()> {
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
        if let Some(snap) = persist::load(&self.session)? {
            self.apply_snapshot(snap)?;
            let closing: Vec<_> = self
                .workspaces
                .iter()
                .filter(|w| w.delete_pending || w.kept.as_ref().is_some_and(|s| s.close_pending.is_some()))
                .map(|w| w.id)
                .collect();
            for ws in closing {
                let deleting = self.ws_index(ws).is_some_and(|wi| self.workspaces[wi].delete_pending);
                let result = if deleting {
                    self.keep_and_delete_workspace(ws).map(|_| ())
                } else {
                    self.keep_and_close_workspace(ws)
                };
                if let Err(e) = result {
                    if let Some(wi) = self.ws_index(ws) {
                        self.workspaces[wi].close_error = Some(e.clone());
                    }
                    self.warnings.push(e);
                }
            }
            self.scan_definitions();
        }
        Ok(())
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
        if let Some(stops) = self.stops(None) {
            message.push('\n');
            message.push_str(&stops);
        }
        Mode::confirm(message, ConfirmAction::Restart)
    }

    /// The question before quitting, with what stops.
    pub(super) fn ask_quit(&self) -> Mode {
        let message = match self.stops(None) {
            Some(stops) => format!("Quit kraal? {stops}\nThe layout is saved for next time."),
            None => "Quit kraal? The layout is saved for next time.".into(),
        };
        Mode::confirm(message, ConfirmAction::Quit)
    }

    pub(super) fn ask_close_workspace(&mut self, client: ClientId, ws: u32) -> Mode {
        let Some(wi) = self.ws_index(ws) else { return Mode::Normal };
        if self.workspaces[wi].delete_pending {
            return self.ask_delete_workspace(super::workspace_lifecycle::DeleteTarget::Workspace(ws));
        }
        if self.workspaces[wi].close_error.is_some() && self.workspaces[wi].kept.is_some() {
            return self.close_workspace(client, ws);
        }
        if self.workspaces[wi].closed() {
            let mut picker = self.workspace_picker(self.clients.get(&client).map_or(0, |c| c.workspace));
            picker.select(|item| item.action.workspace() == Some(ws));
            return Mode::Picker(picker);
        }
        let Some(stops) = self.stops(Some(ws)) else { return self.close_workspace(client, ws) };
        let workspace = &self.workspaces[wi];
        let inventory = CloseInventory {
            name: workspace.name.clone(),
            panes: workspace
                .panes()
                .into_iter()
                .filter_map(|id| {
                    let pane = self.panes.get(&id)?;
                    Some(ClosePane::read(pane))
                })
                .collect(),
        };
        Mode::Confirm(Question {
            title: Some(format!("Close {}?", workspace.name)),
            message: format!("{stops}\n\nThe layout will be kept.\nUnsaved work in these programs may be lost."),
            action: ConfirmAction::CloseWorkspace { workspace: ws, inventory },
            yes: false,
            scroll: 0,
            workspace_back: None,
        })
    }

    /// What quitting or restarting the session, or closing just `workspace`, stops.
    pub(super) fn stops(&self, workspace: Option<u32>) -> Option<String> {
        let mut lines = Vec::new();
        for id in self.workspaces.iter().filter(|w| workspace.is_none_or(|id| w.id == id)).flat_map(|w| w.panes()) {
            let Some(pane) = self.panes.get(&id) else { continue };
            let waiting = pane.shell_waiting();
            let program = if waiting && workspace.is_none() { None } else { pane.foreground_argv() };
            let info = self.pane_info(id);
            let jobs = pane.background_jobs();
            let stops = if workspace.is_some() {
                persist::stops_with_workspace(&info, program.as_deref(), &jobs, waiting)
            } else {
                persist::stops_with_server(&info, program.as_deref(), &jobs)
            };
            for what in stops {
                let label = if workspace.is_some() { what } else { persist::stopping_line(&info, &what) };
                lines.push((label, info.tab.clone()));
            }
        }
        if lines.is_empty() {
            return None;
        }
        let mut message =
            if workspace.is_some() { "These programs will stop:" } else { "These programs stop:" }.to_string();
        for (label, tab) in &lines {
            message.push_str(&format!("\n  {label}"));
            if workspace.is_some() && lines.iter().filter(|(other, _)| other == label).count() > 1 {
                message.push_str(&format!("  ({tab})"));
            }
        }
        Some(message)
    }

    /// Quit: the server stops once the session is saved, as for `kraal kill`, and its clients
    /// end. A session that cannot be saved keeps running and tells the client why, replacing
    /// its previous message. The failed save's warnings must not hide the answer to Quit.
    pub(super) fn quit(&mut self, client: ClientId) {
        match self.save_now() {
            Ok(()) => self.quitting = true,
            Err(e) => self.toast(client, format!("not quitting: {e}")),
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
    /// Keep rollback files until the replacement or its rollback has been written.
    RetainScrollback(HashSet<String>),
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
/// final one. It keeps `held`, what holds the
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
        let mut retained = HashSet::new();
        // Scrollback files whose last write failed, and why.
        let mut failing: HashMap<String, String> = HashMap::new();
        // Why the last write of the paste buffers' file failed.
        let mut buffers_failing: Option<String> = None;
        while let Ok(first) = rx.recv() {
            let mut queue = vec![first];
            queue.extend(rx.try_iter());
            for (i, job) in queue.iter().enumerate() {
                match job {
                    Persist::RetainScrollback(files) => retained = files.clone(),
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
                                persist::clean_scrollback(session, snap, &retained);
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

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    pub(in crate::server) fn writer(
        server: &mut Server,
        answers: Vec<Result<(), String>>,
    ) -> Arc<Mutex<Vec<serde_json::Value>>> {
        let (tx, rx) = crossbeam_channel::unbounded();
        let saved = Arc::new(Mutex::new(Vec::new()));
        let recorded = saved.clone();
        let shared = server.persisting.clone();
        server.persister = tx;
        std::thread::spawn(move || {
            let mut answers = answers.into_iter();
            while let Ok(job) = rx.recv() {
                match job {
                    Persist::Save(_, snap) => {
                        recorded.lock().unwrap().push(serde_json::to_value(snap).unwrap());
                        shared.lock().unwrap().saves = 0;
                    }
                    Persist::Flush(done) => {
                        let answer = answers.next().unwrap_or(Ok(()));
                        shared.lock().unwrap().state_failed = answer.is_err();
                        let _ = done.send(answer);
                    }
                    _ => {}
                }
            }
        });
        saved
    }

    fn workspace(server: &mut Server) -> u32 {
        use crate::pane::PaneSpec;
        use crate::protocol::Placement;
        let ws = server.new_workspace("personal".into(), Some("/missing/source.kdl".into()));
        for placement in [Placement::Tab, Placement::Right, Placement::Floating, Placement::Tab] {
            server
                .spawn_pane_in(
                    ws,
                    None,
                    PaneSpec { files: Some(["/missing".into(), "/missing".into()]), ..Default::default() },
                    placement,
                    Some("kept".into()),
                )
                .unwrap();
        }
        server.workspaces[0].definition_source = Some("// original source".into());
        ws
    }

    #[test]
    fn a_failed_quit_replaces_an_active_copy_toast_on_the_notice_line() {
        for (cols, rows) in [(80, 24), (124, 36)] {
            let (tx, _rx) = crate::server::queue::channel(16);
            let mut server = crate::server::tests::server(tx);
            let (client, _peer) = crate::server::tests::client(&mut server);
            let c = server.clients.get_mut(&client).unwrap();
            c.cols = cols;
            c.rows = rows;
            let error = "the layout could not be saved: Disk full";
            let saved = writer(&mut server, vec![Err(error.into()), Ok(())]);
            server.copy_to_client(client, "text");
            server.client_input(client, b"\x02q");
            assert!(matches!(server.clients[&client].mode, Mode::Confirm(_)));
            let (text, until) = server.clients[&client].toast.as_ref().unwrap();
            assert_eq!(text, "copy sent to terminal");
            assert!(*until > Instant::now());
            server.client_input(client, b"y");
            assert!(!server.quitting);
            assert!(matches!(server.clients[&client].mode, Mode::Normal));
            // The writer's warning may arrive after the answer to Quit.
            server.warnings.push(error.into());
            server.show_warnings();
            let mut buffer = ratatui::buffer::Buffer::empty(server.client_screen(client));
            server.compose(client, crate::accounts::now(), &mut buffer);
            let notice: String = (0..cols).map(|x| buffer[(x, 0)].symbol()).collect();
            assert!(notice.contains("not quitting: the layout could not be saved"), "{cols}x{rows}: {notice}");
            let result = format!("not quitting: {error}");
            assert_eq!(server.clients[&client].toast.as_ref().unwrap().0, result);
            assert!(server.messages.iter().any(|m| m.text == result));
            assert!(server.messages.iter().any(|m| m.warning && m.text == error));
            assert_eq!(saved.lock().unwrap().len(), 1);
            server.client_input(client, b"\x02qy");
            assert!(server.quitting, "Quit can be retried once saving works");
            assert_eq!(saved.lock().unwrap().len(), 2);
        }
    }

    #[test]
    fn closed_and_empty_workspaces_have_no_compatibility_shell() {
        let (tx, _rx) = crate::server::event_channel(4096);
        let mut server = crate::server::tests::server(tx.clone());
        server.new_workspace("empty".into(), None);
        for closed in [false, true] {
            let mut snap = persist::snapshot(&server);
            snap.workspaces[0].closed = closed;
            let value = serde_json::to_value(&snap).unwrap();
            assert_eq!(value["version"], 2);
            assert!(value.get("revision").is_none());
            assert_eq!(value["workspaces"][0]["tabs"], serde_json::json!([]));
            let mut restored = crate::server::tests::server(tx.clone());
            persist::apply(&mut restored, snap).unwrap();
            assert_eq!(restored.workspaces.len(), 1);
            assert_eq!(restored.workspaces[0].name, "empty");
            assert_eq!(restored.workspaces[0].closed(), closed);
            assert!(restored.panes.is_empty());
            assert!(restored.unrestored.is_empty());
        }
    }

    #[test]
    fn close_saves_intent_before_stopping_and_reopens_the_personal_layout() {
        let (tx, _rx) = crate::server::event_channel(4096);
        let mut server = super::super::tests::server(tx.clone());
        let ws = workspace(&mut server);
        let original = serde_json::to_value(persist::snapshot(&server)).unwrap();
        let saved = writer(&mut server, vec![Ok(()), Ok(())]);
        server.keep_and_close_workspace(ws).unwrap();
        assert!(!server.quitting);
        assert!(server.panes.is_empty());
        assert!(server.workspaces[0].closed());
        assert_eq!(server.workspaces[0].counts(), (2, 4));
        let writes = saved.lock().unwrap();
        assert_eq!(writes.len(), 2);
        assert!(writes[0]["workspaces"][0]["close_pending"].is_object());
        assert_ne!(writes[0]["workspaces"][0]["closed"], true);
        assert_eq!(writes[1]["workspaces"][0]["closed"], true);
        assert_eq!(writes[0]["workspaces"][0]["tabs"], original["workspaces"][0]["tabs"]);
        drop(writes);
        let mut restored = super::super::tests::server(tx);
        restored.apply_snapshot(persist::snapshot(&server)).unwrap();
        assert!(restored.panes.is_empty(), "closed workspaces start no programs");
        assert!(restored.workspaces[0].closed());
        restored.open_workspace("personal").unwrap();
        assert_eq!(serde_json::to_value(persist::snapshot(&restored)).unwrap(), original);
        assert_eq!(restored.workspaces.len(), 1, "the missing source is never imported again");
    }

    #[test]
    fn a_failed_preparation_stops_nothing_and_a_retry_keeps_the_layout() {
        let (tx, _rx) = crate::server::event_channel(4096);
        let mut server = super::super::tests::server(tx);
        let ws = workspace(&mut server);
        let before = serde_json::to_value(persist::snapshot(&server)).unwrap();
        writer(&mut server, vec![Err("Disk full".into()), Ok(()), Ok(())]);
        let Mode::Confirm(question) = server.close_workspace(0, ws) else { panic!("retry") };
        assert!(question.message.contains("Nothing was stopped."));
        assert!(question.message.contains("Disk full"));
        assert_eq!(question.action.verb(), "Retry");
        assert!(!question.yes);
        assert!(
            matches!(server.workspace_picker(ws).current().unwrap().action, super::super::ui::PickAction::RetryWorkspaceClose(id) if id == ws)
        );
        assert_eq!(server.panes.len(), 4);
        assert_eq!(serde_json::to_value(persist::snapshot(&server)).unwrap(), before);
        assert!(matches!(server.close_workspace(0, ws), Mode::Picker(_)));
        assert!(server.workspaces[0].closed());
        assert!(server.workspaces[0].close_error.is_none());
    }

    #[test]
    fn a_failed_final_save_keeps_the_prepared_layout_and_tells_the_truth() {
        let (tx, _rx) = crate::server::event_channel(4096);
        let mut server = super::super::tests::server(tx.clone());
        let ws = workspace(&mut server);
        let saved = writer(&mut server, vec![Ok(()), Err("Disk full".into()), Ok(())]);
        let Mode::Confirm(question) = server.close_workspace(0, ws) else { panic!("retry") };
        assert!(question.message.contains("Programs stopped. Couldn't save the final state."));
        assert!(!question.message.contains("Nothing was stopped"));
        assert!(server.panes.is_empty());
        assert_eq!(server.workspaces[0].status(), "Programs stopped · Save failed");
        let prepared = saved.lock().unwrap()[0].clone();
        let mut recovered = super::super::tests::server(tx);
        recovered.apply_snapshot(serde_json::from_value(prepared).unwrap()).unwrap();
        assert!(recovered.panes.is_empty(), "an interrupted close never relaunches its programs");
        let id = recovered.workspaces[0].id;
        writer(&mut recovered, vec![Ok(()), Ok(())]);
        recovered.keep_and_close_workspace(id).unwrap();
        assert!(recovered.workspaces[0].closed());
        assert_eq!(recovered.workspaces[0].counts(), (2, 4));
        server.close_workspace(0, ws);
        assert!(server.workspaces[0].close_error.is_none());
        assert_eq!(server.workspaces[0].status(), "Closed");
    }

    #[test]
    fn the_last_pane_keeps_an_empty_workspace_across_restore() {
        let (tx, _rx) = crate::server::event_channel(4096);
        let mut server = super::super::tests::server(tx.clone());
        workspace(&mut server);
        let id = server.workspaces[0].stable_id.clone();
        for pane in server.workspaces[0].panes() {
            server.close_pane(pane);
        }
        assert!(!server.quitting);
        assert!(server.workspaces[0].tabs.is_empty());
        assert!(server.workspaces[0].tab().is_none());
        let saved = persist::snapshot(&server);
        assert!(saved.workspaces[0].empty);
        assert_eq!(saved.workspaces[0].counts(), (0, 0));
        assert!(saved.workspaces[0].tabs.is_empty(), "empty workspaces have no fallback panes");
        let mut restored = super::super::tests::server(tx);
        restored.apply_snapshot(saved).unwrap();
        assert!(restored.unrestored.is_empty());
        assert_eq!(restored.workspaces[0].stable_id, id);
        assert!(restored.workspaces[0].tabs.is_empty());
        let ws = restored.workspaces[0].id;
        writer(&mut restored, vec![]);
        restored.keep_and_close_workspace(ws).unwrap();
        restored.reopen_workspace(ws).unwrap();
        assert!(restored.workspaces[0].tabs.is_empty());
        assert!(restored.panes.is_empty());
    }

    #[test]
    fn partial_close_snapshots_and_scrollback_survive_later_saves() {
        let (tx, _rx) = crate::server::event_channel(4096);
        let mut server = super::super::tests::server(tx);
        let ws = workspace(&mut server);
        let mut saved = persist::snapshot(&server).workspaces.remove(0);
        saved.close_pending = Some(persist::CloseIntent::new(Vec::new(), Vec::new()));
        let persist::NodeSnap::Pane(pane) = saved.tabs[1].root.as_mut().unwrap() else { panic!() };
        pane.scrollback = Some("1-2-3.ans".into());
        server.workspaces[0].kept = Some(saved.clone());
        for pane in server.workspaces[0].panes() {
            server.close_pane(pane);
        }
        let partial = persist::snapshot(&server);
        assert_eq!(serde_json::to_value(&partial.workspaces[0]).unwrap(), serde_json::to_value(saved).unwrap());
        assert!(persist::scrollback_files(&partial).contains("1-2-3.ans"));
        assert!(server.reopen_workspace(ws).unwrap_err().to_string().contains("Retry Close"));
        server.config.save_scrollback = false;
        assert!(persist::scrollback_files(&persist::snapshot(&server)).is_empty());
    }

    #[test]
    fn a_live_program_is_not_signalled_until_the_preparation_is_acknowledged() {
        let (tx, _rx) = crate::server::event_channel(4096);
        let mut server = super::super::tests::server(tx);
        server.config.save_scrollback = false;
        let ws = server.new_workspace("live".into(), None);
        let pane = server
            .spawn_pane_in(
                ws,
                None,
                crate::pane::PaneSpec { command: vec!["sleep".into(), "60".into()], ..Default::default() },
                crate::protocol::Placement::Tab,
                None,
            )
            .unwrap();
        let process = server.panes[&pane].process_hold().unwrap();
        let observed = process.clone();
        let (writer, jobs) = crossbeam_channel::unbounded();
        server.persister = writer;
        let written = std::thread::spawn(move || {
            let mut saves = 0;
            let mut prepared = false;
            while let Ok(job) = jobs.recv() {
                match job {
                    Persist::Save(_, snap) => {
                        saves += 1;
                        if saves == 1 {
                            let intent = snap.workspaces[0].close_pending.as_ref().unwrap();
                            assert_eq!(intent.local_sessions()[0].leader, observed.session_owner().unwrap().leader);
                            assert!(observed.group_running());
                        }
                    }
                    Persist::Flush(done) => {
                        if !prepared {
                            assert!(observed.group_running(), "the writer has not acknowledged preparation yet");
                            prepared = true;
                        } else {
                            assert!(!observed.group_running(), "Closed requires stopped programs");
                            done.send(Ok(())).unwrap();
                            return saves;
                        }
                        done.send(Ok(())).unwrap();
                    }
                    _ => {}
                }
            }
            panic!("close did not finish")
        });
        server.keep_and_close_workspace(ws).unwrap();
        assert_eq!(written.join().unwrap(), 2);
        assert!(!process.group_running());
        assert!(server.workspaces[0].closed());
    }

    #[test]
    fn a_failed_reopen_keeps_the_layout_and_other_empty_workspaces() {
        let (tx, _rx) = crate::server::event_channel(4096);
        let mut server = super::super::tests::server(tx);
        let ws = workspace(&mut server);
        writer(&mut server, vec![]);
        server.keep_and_close_workspace(ws).unwrap();
        let kept = server.workspaces[0].kept.as_mut().unwrap();
        let persist::NodeSnap::Pane(p) = kept.tabs[1].root.as_mut().unwrap() else { panic!() };
        p.spec.files = None;
        // An earlier SSH pane is ready to launch, before the later shell fails preparation.
        let earlier = &mut kept.tabs[0].floating[0].0;
        earlier.spec.files = None;
        earlier.spec.command = vec!["ssh".into(), "example.invalid".into()];
        earlier.stopped = false;
        let original = serde_json::to_value(&server.workspaces[0].kept).unwrap();
        server.new_workspace("empty neighbour".into(), None);
        server.env.shell = "/no/such/shell".into();
        let (launches, queued) = crossbeam_channel::unbounded();
        server.launches = launches;
        assert!(server.reopen_workspace(ws).unwrap_err().to_string().contains("Its layout is kept"));
        assert!(queued.is_empty(), "failed construction must never queue the earlier program for launch");
        assert!(server.panes.is_empty(), "partial construction is rolled back");
        assert!(server.workspaces[0].closed());
        assert_eq!(serde_json::to_value(&server.workspaces[0].kept).unwrap(), original);
        assert_eq!(server.workspaces.len(), 2);
        assert!(server.workspaces[1].tabs.is_empty());
    }

    #[test]
    fn closing_the_last_workspace_keeps_all_clients_in_the_list() {
        let (tx, _rx) = crate::server::event_channel(4096);
        let mut server = super::super::tests::server(tx);
        let (first, _peer) = super::super::tests::client(&mut server);
        let ws = server.clients[&first].workspace;
        server.workspaces[0].tabs.clear();
        let (second, _other) = super::super::tests::client(&mut server);
        server.workspaces.pop();
        server.clients.get_mut(&second).unwrap().workspace = ws;
        writer(&mut server, vec![]);
        server.keep_and_close_workspace(ws).unwrap();
        for id in [first, second] {
            assert_eq!(server.clients[&id].workspace, ws);
            assert!(matches!(server.clients[&id].mode, Mode::Picker(_)));
        }
        assert!(!server.quitting);
        server.attach(first, 80, 24, None);
        assert!(server.workspaces[0].closed(), "attaching without a name does not reopen it");
        assert!(matches!(server.clients[&first].mode, Mode::Picker(_)));
        assert_eq!(server.workspaces.len(), 1, "no replacement main workspace");
    }

    #[test]
    fn workspace_identity_survives_renaming_reordering_and_restoring() {
        use crate::layout::Node;
        use crate::pane::PaneSpec;
        use crate::server::Tab;
        let (tx, _rx) = crate::server::event_channel(4096);
        let mut server = super::super::tests::server(tx.clone());
        for name in ["one", "two"] {
            server.new_workspace(name.into(), Some(format!("/missing/{name}.kdl").into()));
            let pane = server
                .spawn_pane(
                    None,
                    PaneSpec { files: Some(["/missing".into(), "/missing".into()]), ..Default::default() },
                )
                .unwrap();
            let mut tab = Tab::new(server.alloc_id(), name.into());
            tab.root = Some(Node::Leaf(pane));
            server.workspaces.last_mut().unwrap().tabs.push(tab);
        }
        let ids: Vec<_> = server.workspaces.iter().map(|w| w.stable_id.clone()).collect();
        assert_ne!(ids[0], ids[1]);
        server.workspaces[0].name = "renamed".into();
        server.workspaces[1].name = "renamed".into();
        server.workspaces[0].set_definition(Some(std::path::Path::new("/elsewhere/source.kdl")));
        server.workspaces.reverse();
        let mut saved = persist::snapshot(&server);
        saved.workspaces[0].restore_error = Some("previous failure".into());
        let mut restored = super::super::tests::server(tx);
        // Runtime ids change across restores and must not become the saved identity.
        restored.alloc_id();
        persist::apply(&mut restored, saved).unwrap();
        assert!(restored.unrestored.is_empty());
        let saved = persist::snapshot(&restored);
        assert!(saved.workspaces[0].restore_error.is_none());
        assert_eq!(saved.workspaces.iter().map(|w| &w.id).collect::<Vec<_>>(), [&ids[1], &ids[0]]);
        assert_eq!(saved.workspaces[0].name, "renamed", "saved name collisions are kept");
        assert_eq!(saved.workspaces[1].name, "renamed");
        assert_eq!(saved.workspaces[1].definition.as_deref(), Some(std::path::Path::new("/elsewhere/source.kdl")));
        assert_eq!(saved.workspaces[1].tabs[0].name, "one");
    }

    #[test]
    fn a_failed_workspace_keeps_its_identity_source_layout_and_reason_until_restored() {
        let (tx, _rx) = crate::server::event_channel(4096);
        let mut server = super::super::tests::server(tx.clone());
        let value = serde_json::json!({ "version": 2, "workspaces": [{
            "id": "stable", "name": "kept", "definition": "/missing/source.kdl",
            "definition_source": "// source kept verbatim", "cwd": null, "active": 0, "tabs": []
        }] });
        persist::apply(&mut server, serde_json::from_value(value).unwrap()).unwrap();
        assert!(server.workspaces.is_empty());
        let saved = persist::snapshot(&server);
        let failed = &saved.workspaces[0];
        assert_eq!(failed.id, "stable");
        assert_eq!(failed.definition_source.as_deref(), Some("// source kept verbatim"));
        assert_eq!(failed.restore_error.as_deref(), Some("the saved workspace has no restorable panes"));
        assert!(server.open_workspace("kept").unwrap_err().to_string().contains("fix the cause and restart"));
        let reason = failed.restore_error.clone();
        let mut restored = super::super::tests::server(tx);
        persist::apply(&mut restored, saved).unwrap();
        assert_eq!(restored.unrestored[0].id, "stable");
        assert_eq!(restored.unrestored[0].restore_error, reason);
    }

    #[test]
    fn screen_close_names_programs_and_only_uses_tabs_for_repeated_names() {
        let _batch = crate::pane::batch();
        let (tx, _rx) = crate::server::queue::channel(32);
        let mut server = crate::server::tests::server(tx);
        let def = crate::workspace::parse_workspace(
            "web",
            concat!(
                "layout { tab name=\"server\" { pane command=\"sleep\" { args \"600\"; }; }; ",
                "tab name=\"worker\" { pane command=\"sleep\" { args \"600\"; }; }; ",
                "tab name=\"logs\" { pane command=\"tail\" { args \"-f\" \"app.log\"; }; }; }",
            ),
        )
        .unwrap();
        let ws = server.start_definition_inner(&def, None).unwrap();
        for linked in [false, true] {
            server.workspaces[0].definition = linked.then(|| "/project/web.kdl".into());
            let Mode::Confirm(q) = server.ask_close_workspace(0, ws) else { panic!() };
            assert!(!q.yes);
            for (cols, rows) in [(80, 24), (124, 36)] {
                let area = ratatui::layout::Rect::new(0, 0, cols, rows);
                let mut buf = ratatui::buffer::Buffer::empty(area);
                super::super::overlays::draw_confirm(&q, area, &mut buf);
                let shown: Vec<String> =
                    buf.content.chunks(cols as usize).map(|row| row.iter().map(|c| c.symbol()).collect()).collect();
                for expected in [
                    "Close web?",
                    "These programs will stop:",
                    "sleep 600  (server)",
                    "sleep 600  (worker)",
                    "tail -f app.log",
                    "The layout will be kept.",
                    "Unsaved work in these programs may be lost.",
                    "Cancel",
                    "Stop and close",
                ] {
                    assert!(shown.iter().any(|row| row.contains(expected)), "missing {expected}: {shown:?}");
                }
                for absent in ["Confirm", "pane ", "file", "Reopening", "last open", "(logs)"] {
                    assert!(!shown.iter().any(|row| row.contains(absent)), "unexpected {absent}: {shown:?}");
                }
            }
        }
        server.close_pane(server.workspaces[0].tabs[1].panes()[0]);
        let Mode::Confirm(q) = server.ask_close_workspace(0, ws) else { panic!() };
        for (cols, rows) in [(80, 24), (124, 36)] {
            let area = ratatui::layout::Rect::new(0, 0, cols, rows);
            let mut buf = ratatui::buffer::Buffer::empty(area);
            super::super::overlays::draw_confirm(&q, area, &mut buf);
            let shown: Vec<String> =
                buf.content.chunks(cols as usize).map(|row| row.iter().map(|c| c.symbol()).collect()).collect();
            assert!(shown.iter().any(|row| row.contains("sleep 600")));
            assert!(!shown.iter().any(|row| row.contains("(server)")), "{shown:?}");
        }
    }

    #[test]
    fn closing_keeps_the_same_question_with_or_without_a_source_or_another_workspace() {
        let _batch = crate::pane::batch();
        let dir = std::env::temp_dir().join(format!("kraal-close-layout-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("api.kdl");
        std::fs::write(&path, "layout { pane; }\n").unwrap();
        let (tx, _rx) = crate::server::queue::channel(32);
        let mut server = crate::server::tests::server(tx);
        let ws = server.new_workspace("api".into(), None);
        server
            .spawn_pane_in(
                ws,
                None,
                crate::pane::PaneSpec { command: vec!["sleep".into(), "60".into()], ..Default::default() },
                crate::protocol::Placement::Tab,
                None,
            )
            .unwrap();
        for linked in [false, true] {
            server.workspaces[0].definition = linked.then(|| path.clone());
            let Mode::Confirm(q) = server.ask_close_workspace(0, ws) else { panic!("confirmation") };
            assert!(!q.yes);
            assert_eq!(q.title.as_deref(), Some("Close api?"));
            assert_eq!(
                q.message,
                "These programs will stop:\n  sleep 60\n\nThe layout will be kept.\nUnsaved work in these programs may be lost."
            );
        }
        server.new_workspace("other".into(), None);
        let Mode::Confirm(q) = server.ask_close_workspace(0, ws) else { panic!("confirmation") };
        assert!(!q.message.contains("This is the last open workspace."));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "layout { pane; }\n");
        assert!(matches!(server.ask_close_workspace(0, u32::MAX), Mode::Normal));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn closing_keeps_the_layout_when_the_linked_definition_was_deleted_or_became_invalid() {
        let dir = std::env::temp_dir().join(format!("kraal-close-missing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = dir.canonicalize().unwrap();
        let path = dir.join("api.kdl");
        for replacement in [None, Some("not a workspace")] {
            std::fs::write(&path, "layout { pane command=\"sleep\" { args \"60\"; }; }\n").unwrap();
            let (tx, _rx) = crate::server::queue::channel(32);
            let mut server = crate::server::tests::server(tx);
            let ws = server.open_definition(&path).unwrap();
            if let Some(text) = replacement {
                std::fs::write(&path, text).unwrap();
            } else {
                std::fs::remove_file(&path).unwrap();
            }
            server.scan_definitions();
            assert_eq!(server.workspaces[0].definition.as_ref(), Some(&path));
            let Mode::Confirm(q) = server.ask_close_workspace(0, ws) else { panic!("confirmation") };
            assert!(!q.yes);
            assert!(q.message.contains("The layout will be kept."));
            assert!(!q.message.contains("file"));
            assert_eq!(server.workspaces[0].panes().len(), 1);
            assert_eq!(std::fs::read_to_string(&path).ok().as_deref(), replacement);
            let mut expected = serde_json::to_value(persist::snapshot(&server)).unwrap();
            expected["workspaces"][0]["tabs"][0]["root"]["Pane"]["stopped"] = true.into();
            writer(&mut server, vec![Ok(()), Ok(())]);
            server.keep_and_close_workspace(ws).unwrap();
            assert!(server.workspaces[0].closed());
            assert!(server.panes.is_empty());
            server.reopen_workspace(ws).unwrap();
            let after = serde_json::to_value(persist::snapshot(&server)).unwrap();
            assert_eq!(after["workspaces"][0]["tabs"], expected["workspaces"][0]["tabs"]);
            assert_eq!(std::fs::read_to_string(&path).ok().as_deref(), replacement);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn closing_lists_a_shell_script_leading_the_foreground_group() {
        let (tx, _rx) = crate::server::queue::channel(32);
        let mut server = crate::server::tests::server(tx);
        let def = crate::workspace::parse_workspace(
            "script",
            "layout { pane command=\"sh\" { args \"-c\" \"sleep 600; echo done\"; }; }\n",
        )
        .unwrap();
        let ws = server.start_definition_inner(&def, None).unwrap();
        let pane = &server.panes[&server.workspaces[0].panes()[0]];
        let pid = pane.pid().unwrap();
        let start = Instant::now();
        while !crate::process::children(pid)
            .into_iter()
            .any(|child| crate::process::argv(child).is_some_and(|a| a == ["sleep", "600"]))
        {
            assert!(start.elapsed() < crate::testing::PATIENCE, "the script never started");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(pane.foreground_pid(), Some(pid));
        assert!(pane.shell_waiting());
        assert!(pane.background_jobs().is_empty());
        let Mode::Confirm(q) = server.ask_close_workspace(0, ws) else { panic!("confirmation") };
        assert!(q.message.contains("These programs will stop:\n  sh -c 'sleep 600; echo done'"), "{}", q.message);
        assert!(q.message.contains("Unsaved work in these programs may be lost."), "{}", q.message);
    }

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
