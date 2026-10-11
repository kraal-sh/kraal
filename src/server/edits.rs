//! Editing a file of a server in the local editor: a private copy, the editor in a floating
//! pane, each save sent back to the server, and a question when the server's file changed
//! meanwhile (see `crate::edit` for what runs there).

use std::path::{Path, PathBuf};

use super::contexts::Production;
use super::ui::{ConfirmAction, Mode, PickAction, PickItem, Picker, PickerKind};
use super::{ClientId, Server};
use crate::edit::{self, RemoteFile, Saved, Stamp};
use crate::layout::PaneId;
use crate::pane::PaneSpec;
use crate::protocol::{Placement, Response, ServerMsg};

/// A file to edit, and where its editor opens.
#[derive(Clone, Debug)]
pub struct EditTarget {
    /// The server, by the name it is shown with.
    pub server: String,
    /// ssh arguments that reach it, destination last.
    pub ssh: Vec<String>,
    /// Absolute, `~/…`, or relative to the home directory there.
    pub path: String,
    pub line: Option<u32>,
    /// The workspace the editor opens in; else the client's, else the first one.
    pub ws: Option<u32>,
    /// The pane the editor opens over.
    pub near: Option<PaneId>,
}

/// What to do when the server's file changed since it was read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    /// Keep the edited version as a local file; the server keeps its own.
    KeepLocal,
    /// The server's version stays, and the editor's copy becomes it.
    KeepServer,
    /// The edited version replaces the server's.
    Overwrite,
}

/// A file being edited.
pub(super) struct Edit {
    id: u32,
    /// Who asked: told about each save, and asked when the server's file changed.
    client: Option<ClientId>,
    target: EditTarget,
    file: RemoteFile,
    /// The server's version as last read or written, and what identifies it there. Once kept as
    /// a local file: what that file holds.
    remote: Vec<u8>,
    identity: String,
    /// The editor's copy, alone in its private directory.
    local: PathBuf,
    pane: PaneId,
    seen: Option<Stamp>,
    /// The copy changed at the last look: it is read once it holds still.
    settling: bool,
    /// An ssh command for this file runs.
    busy: bool,
    /// The server's version changed since it was read (what identifies it now): nothing is sent
    /// until the user says what to do.
    conflict: Option<String>,
    /// The question was shown; it is not shown again before the next save.
    asked: bool,
    /// Kept as this local file: saves go there, no longer to the server.
    kept: Option<PathBuf>,
    /// The editor ended.
    closed: bool,
}

impl Edit {
    fn name(&self) -> String {
        printable(&self.file.name)
    }

    fn dir(&self) -> &Path {
        self.local.parent().unwrap_or(Path::new("/"))
    }
}

/// `$EDITOR` (vi without one) on `path`, at `line`: what an editor's pane runs.
pub(super) fn editor_command(path: &Path, line: Option<u32>) -> Vec<String> {
    let mut command = vec!["sh".to_string(), "-c".into(), "exec ${EDITOR:-vi} \"$@\"".into(), "kraal-edit".into()];
    if let Some(line) = line {
        command.push(format!("+{line}"));
    }
    command.push(path.display().to_string());
    command
}

/// A name as shown in one line: control characters as `?`.
fn printable(s: &str) -> String {
    s.chars().map(|c| if c.is_control() { '?' } else { c }).collect()
}

impl Server {
    /// Edit `target` for `client`, asking first when it is production.
    pub(super) fn edit_request(
        &mut self,
        client: ClientId,
        target: EditTarget,
        production: Option<Production>,
    ) -> Mode {
        if let Some(why) = production {
            let message = format!(
                "Edit {} on {}? It is production ({why}).\nEach save in the editor goes to the server.",
                printable(&target.path),
                target.server
            );
            return Mode::confirm(message, ConfirmAction::Edit(Box::new(target)));
        }
        self.start_edit(Some(client), target, None);
        Mode::Normal
    }

    /// Why a server is production, if it may be (`docs/production.md`).
    pub(super) fn server_production(&self, s: &crate::servers::Server) -> Option<Production> {
        let words = &self.config.production.words;
        (!words.is_empty() && crate::context::server_production(s, words))
            .then(|| Production::new(format!("server {}", s.name), format!("server {s:?}")))
    }

    fn download_dir(&self) -> PathBuf {
        crate::paths::expand_tilde(&self.config.servers.download_dir)
    }

    /// Read the file off the event loop, then open the editor on a private copy of it. `reply`
    /// is a control request (`kraal edit`) waiting to know how that went.
    pub(super) fn start_edit(&mut self, client: Option<ClientId>, target: EditTarget, reply: Option<ClientId>) {
        let (ssh, path) = (target.ssh.clone(), target.path.clone());
        let shown = format!("{}:{}", target.server, printable(&target.path));
        let started = self.background(
            move || edit::open(&ssh, &path),
            move |server, opened| {
                let result = opened
                    .and_then(|o| server.open_editor(client, target, o))
                    .map_err(|e| format!("cannot edit {shown}: {e:#}"));
                match reply {
                    Some(r) => {
                        let result = result.map(|p| Response::Pane(Box::new(server.pane_info(p))));
                        server.send_msg(r, &ServerMsg::Response { result });
                        server.clients.remove(&r);
                    }
                    None => {
                        if let Err(e) = result {
                            server.toast(client.unwrap_or(0), e);
                        }
                    }
                }
            },
        );
        if !started && let Some(r) = reply {
            self.send_msg(r, &ServerMsg::Response { result: Err("cannot start reading the file".into()) });
            self.clients.remove(&r);
        }
    }

    /// The editor on a private copy of `opened`, in a floating pane.
    fn open_editor(
        &mut self,
        client: Option<ClientId>,
        target: EditTarget,
        opened: edit::Opened,
    ) -> anyhow::Result<PaneId> {
        let base = crate::paths::private_runtime_dir()?.join("edit");
        let local = edit::local_copy(&base, &opened.file.name, &opened.content)?;
        let dir = local.parent().map(Path::to_path_buf).unwrap_or_default();
        let command = editor_command(&local, target.line);
        // It ends with the editor, and does not come back after a restart (nor do transfer
        // windows): its copy is gone by then.
        let spec = PaneSpec {
            command,
            cwd: Some(dir.clone()),
            close_on_exit: true,
            keep_on_failure: true,
            close_on_success: true,
            ..Default::default()
        };
        // The workspace may have closed while the file was read.
        let open = |ws: &u32| self.ws_index(*ws).is_some();
        let ws = target
            .ws
            .filter(open)
            .or_else(|| client.and_then(|c| self.clients.get(&c)).map(|c| c.workspace).filter(open))
            .or_else(|| self.workspaces.first().map(|w| w.id))
            .unwrap_or_else(|| self.new_workspace("main".into(), None));
        let label = format!("{} · {}", printable(&opened.file.name), target.server);
        let pane = match self.spawn_pane_in(ws, target.near, spec, Placement::Floating, Some(label)) {
            Ok(p) => p,
            Err(e) => {
                self.drop_empty_workspaces();
                let _ = std::fs::remove_dir_all(&dir);
                return Err(e);
            }
        };
        let id = self.alloc_id();
        self.edits.push(Edit {
            id,
            client,
            target,
            file: opened.file,
            remote: opened.content,
            identity: opened.identity,
            seen: edit::stamp(&local),
            local,
            pane,
            settling: false,
            busy: false,
            conflict: None,
            asked: false,
            kept: None,
            closed: false,
        });
        Ok(pane)
    }

    fn edit_index(&self, id: u32) -> Option<usize> {
        self.edits.iter().position(|e| e.id == id)
    }

    /// The client to tell about an edit: the one who asked, else the main one.
    fn edit_client(&self, id: u32) -> ClientId {
        let attached = |c: &ClientId| self.clients.get(c).is_some_and(|c| c.attached);
        let asked = self.edit_index(id).and_then(|i| self.edits[i].client);
        asked.filter(attached).or(self.primary.filter(attached)).unwrap_or(0)
    }

    fn edit_toast(&mut self, id: u32, message: String) {
        let client = self.edit_client(id);
        self.toast(client, message);
    }

    /// Look at each edited copy: send what was saved, ask what was not answered, and end the
    /// edits whose editor ended.
    pub(super) fn tick_edits(&mut self) {
        let ids: Vec<u32> = self.edits.iter().map(|e| e.id).collect();
        for id in ids {
            self.check_edit(id);
        }
    }

    fn check_edit(&mut self, id: u32) {
        let Some(i) = self.edit_index(id) else { return };
        let running = self.panes.get(&self.edits[i].pane).is_some_and(|p| p.exit_code.is_none());
        if self.edits[i].busy || self.edit_question_shown(id) {
            return;
        }
        let e = &mut self.edits[i];
        e.closed |= !running;
        let now = edit::stamp(&e.local);
        let changed = now != e.seen;
        if changed {
            e.seen = now;
            e.settling = true;
        }
        if e.conflict.is_some() {
            // A new save asks again.
            if changed {
                e.asked = false;
            }
            let (asked, closed) = (e.asked, e.closed);
            if !asked && self.ask_edit(id) {
                return;
            }
            if closed {
                self.keep_edit_locally(id, "nobody said what to do about the server's change");
            }
            return;
        }
        if !(e.closed || (e.settling && !changed)) {
            return;
        }
        e.settling = false;
        let content = match crate::paths::read_regular(&e.local, edit::MAX_SIZE) {
            Ok(c) => c,
            Err(err) => {
                let (name, closed, gone) = (e.name(), e.closed, err.kind() == std::io::ErrorKind::NotFound);
                let dir = e.dir().to_path_buf();
                if closed && gone {
                    self.edit_toast(id, format!("{name}: the editor's copy is gone, nothing more was saved"));
                    self.finish_edit(id);
                } else if closed {
                    self.edit_toast(id, format!("{name} was not saved: {err}; your copy stays in {}", dir.display()));
                    self.edits.retain(|e| e.id != id);
                } else if !gone {
                    self.edit_toast(id, format!("{name} was not saved: {err}"));
                }
                return;
            }
        };
        if let Some(kept) = e.kept.clone() {
            if content != e.remote {
                let name = e.name();
                match crate::paths::write_private(&kept, &content) {
                    Ok(()) => {
                        e.remote = content;
                        let shown = crate::paths::shorten_home(&kept);
                        self.edit_toast(id, format!("{name} saved in {shown}"));
                    }
                    Err(err) => {
                        let dir = e.dir().to_path_buf();
                        self.edit_toast(id, format!("{name}: {err}; your copy stays in {}", dir.display()));
                        self.edits.retain(|e| e.id != id);
                        return;
                    }
                }
            }
            if self.edits.get(i).is_some_and(|e| e.closed) {
                self.finish_edit(id);
            }
            return;
        }
        if content != e.remote {
            let expected = e.identity.clone();
            self.send_edit(id, content, expected);
        } else if e.closed {
            self.finish_edit(id);
        }
    }

    /// Send `content` to the server, in place of what `expected` identifies.
    fn send_edit(&mut self, id: u32, content: Vec<u8>, expected: String) {
        let Some(i) = self.edit_index(id) else { return };
        let e = &mut self.edits[i];
        e.busy = true;
        let (ssh, file) = (e.target.ssh.clone(), e.file.clone());
        let started = self.background(
            move || {
                let result = edit::save(&ssh, &file, &expected, &content);
                (content, result)
            },
            move |server, (content, result)| server.edit_sent(id, content, result),
        );
        if !started && let Some(i) = self.edit_index(id) {
            self.edits[i].busy = false;
        }
    }

    fn edit_sent(&mut self, id: u32, content: Vec<u8>, result: anyhow::Result<Saved>) {
        let Some(i) = self.edit_index(id) else { return };
        let e = &mut self.edits[i];
        e.busy = false;
        let (name, server, closed) = (e.name(), e.target.server.clone(), e.closed);
        match result {
            Ok(Saved::Written(identity)) => {
                e.identity = identity;
                e.remote = content;
                self.edit_toast(id, format!("{name} saved to {server}"));
                self.refresh_file_managers();
            }
            Ok(Saved::Changed(identity)) => {
                e.conflict = Some(identity);
                e.asked = false;
                self.edit_toast(
                    id,
                    format!("{name} changed on {server} since you opened it: your version was not sent"),
                );
            }
            Err(err) if closed => {
                self.edit_toast(id, format!("{name} was not saved to {server}: {err:#}"));
                self.keep_edit_locally(id, "it could not be saved");
            }
            Err(err) => self.edit_toast(id, format!("{name} was not saved to {server}: {err:#}")),
        }
    }

    /// Show the question about edit `id` to its client, or the main one, when it is free.
    /// False when nobody is there to answer.
    fn ask_edit(&mut self, id: u32) -> bool {
        let Some(i) = self.edit_index(id) else { return false };
        let attached = |c: &ClientId| self.clients.get(c).is_some_and(|c| c.attached);
        let candidates: Vec<ClientId> =
            [self.edits[i].client, self.primary].into_iter().flatten().filter(attached).collect();
        if candidates.is_empty() {
            return false;
        }
        // A client busy with something else is asked once it is done.
        let Some(client) =
            candidates.into_iter().find(|c| self.clients.get(c).is_some_and(|c| matches!(c.mode, Mode::Normal)))
        else {
            return true;
        };
        let picker = self.edit_question(&self.edits[i]);
        self.edits[i].asked = true;
        if let Some(c) = self.clients.get_mut(&client) {
            c.mode = Mode::Picker(picker);
            c.dirty = true;
        }
        self.mark_dirty_client(client);
        true
    }

    fn edit_question(&self, e: &Edit) -> Picker {
        let (name, server) = (e.name(), &e.target.server);
        let downloads = crate::paths::shorten_home(&self.download_dir());
        let answer = |answer| PickAction::EditAnswer { edit: e.id, answer };
        let items = vec![
            PickItem::new(
                "Keep your version as a local file",
                format!("in {downloads}; {server} keeps its own"),
                answer(Answer::KeepLocal),
            ),
            PickItem::new(
                format!("Keep the version of {server}"),
                "your changes are dropped, and the editor's copy becomes that version",
                answer(Answer::KeepServer),
            ),
            PickItem::new(
                "Replace it with your version",
                format!("what changed on {server} is lost"),
                answer(Answer::Overwrite),
            ),
        ];
        Picker::new(PickerKind::EditConflict, &format!("{name} changed on {server} since you opened it"), items)
    }

    /// Whether a client shows the question about edit `id`.
    fn edit_question_shown(&self, id: u32) -> bool {
        self.clients.values().any(|c| match &c.mode {
            Mode::Picker(p) => {
                p.items.iter().any(|it| matches!(it.action, PickAction::EditAnswer { edit, .. } if edit == id))
            }
            _ => false,
        })
    }

    /// The user's answer about edit `id`.
    pub(super) fn answer_edit(&mut self, client: ClientId, id: u32, answer: Answer) {
        let Some(i) = self.edit_index(id) else {
            self.toast(client, "that edit is over");
            return;
        };
        let e = &mut self.edits[i];
        e.client = Some(client);
        let Some(now) = e.conflict.clone() else { return };
        let (name, server) = (e.name(), e.target.server.clone());
        match answer {
            Answer::Overwrite => match crate::paths::read_regular(&e.local, edit::MAX_SIZE) {
                Ok(content) => {
                    e.conflict = None;
                    self.send_edit(id, content, now);
                }
                Err(err) => self.toast(client, format!("{name} was not saved: {err}")),
            },
            Answer::KeepServer if e.closed => {
                self.toast(client, format!("{name}: the version of {server} stays"));
                self.finish_edit(id);
            }
            Answer::KeepServer => {
                e.conflict = None;
                e.busy = true;
                let (ssh, path) = (e.target.ssh.clone(), e.file.path());
                let started = self.background(
                    move || edit::open(&ssh, &path),
                    move |server, opened| server.edit_reloaded(id, now, opened),
                );
                if !started && let Some(i) = self.edit_index(id) {
                    self.edits[i].busy = false;
                }
            }
            Answer::KeepLocal => {
                let kept = crate::paths::read_regular(&e.local, edit::MAX_SIZE).and_then(|content| {
                    edit::keep(&self.download_dir(), &self.edits[i].file.name, &content).map(|p| (p, content))
                });
                match kept {
                    Ok((path, content)) => {
                        let e = &mut self.edits[i];
                        e.conflict = None;
                        e.kept = Some(path.clone());
                        e.remote = content;
                        let shown = crate::paths::shorten_home(&path);
                        let next = if e.closed { "" } else { "; your next saves go there" };
                        self.toast(client, format!("{name} kept in {shown}, {server} keeps its own{next}"));
                        if self.edits[i].closed {
                            self.finish_edit(id);
                        }
                    }
                    Err(err) => self.toast(client, format!("{name} could not be kept: {err}")),
                }
            }
        }
    }

    /// The server's version, read again for the editor's copy after the user chose to keep it.
    fn edit_reloaded(&mut self, id: u32, conflict: String, opened: anyhow::Result<edit::Opened>) {
        let Some(i) = self.edit_index(id) else { return };
        let e = &mut self.edits[i];
        e.busy = false;
        let (name, server) = (e.name(), e.target.server.clone());
        match opened {
            Ok(_) if e.closed => {
                self.edit_toast(id, format!("{name}: the version of {server} stays"));
                self.finish_edit(id);
            }
            Ok(o) => match std::fs::write(&e.local, &o.content) {
                Ok(()) => {
                    e.seen = edit::stamp(&e.local);
                    e.settling = false;
                    (e.file, e.remote, e.identity) = (o.file, o.content, o.identity);
                    self.edit_toast(
                        id,
                        format!("{name}: the editor's copy is the version of {server} now (reload it)"),
                    );
                }
                Err(err) => {
                    e.conflict = Some(conflict);
                    e.asked = true;
                    self.edit_toast(id, format!("{name}: {err}"));
                }
            },
            Err(err) => {
                // Asked again at the next save, or when the editor ends.
                e.conflict = Some(conflict);
                e.asked = true;
                self.edit_toast(id, format!("{name}: the version of {server} could not be read: {err:#}"));
            }
        }
    }

    /// End edit `id`, keeping the editor's version as a local file first.
    fn keep_edit_locally(&mut self, id: u32, why: &str) {
        let Some(i) = self.edit_index(id) else { return };
        let (name, server) = (self.edits[i].name(), self.edits[i].target.server.clone());
        let kept = crate::paths::read_regular(&self.edits[i].local, edit::MAX_SIZE)
            .and_then(|content| edit::keep(&self.download_dir(), &self.edits[i].file.name, &content));
        match kept {
            Ok(path) => {
                let shown = crate::paths::shorten_home(&path);
                self.edit_toast(id, format!("{name}: {why}; your version is in {shown}, {server} keeps its own"));
                self.finish_edit(id);
            }
            Err(err) => {
                let dir = self.edits[i].dir().display().to_string();
                self.edit_toast(id, format!("{name}: {why}, and {err}: your copy stays in {dir}"));
                self.edits.retain(|e| e.id != id);
            }
        }
    }

    /// Forget edit `id` and remove its private copy.
    fn finish_edit(&mut self, id: u32) {
        let Some(i) = self.edit_index(id) else { return };
        let e = self.edits.remove(i);
        let _ = std::fs::remove_dir_all(e.dir());
    }

    /// The server stops: what was not sent is kept as local files (there is no time to send it),
    /// and the private copies go.
    pub(super) fn end_edits(&mut self) {
        let download_dir = self.download_dir();
        for e in std::mem::take(&mut self.edits) {
            // A copy that cannot be read stays where it is.
            let Ok(content) = crate::paths::read_regular(&e.local, edit::MAX_SIZE) else { continue };
            let kept = match &e.kept {
                _ if content == e.remote => Ok(None),
                Some(path) => crate::paths::write_private(path, &content).map(|()| Some(path.clone())),
                None => edit::keep(&download_dir, &e.file.name, &content).map(Some),
            };
            match kept {
                Ok(path) => {
                    if let Some(path) = path {
                        eprintln!(
                            "kraal: {} was not saved to {}: it is in {}",
                            e.name(),
                            e.target.server,
                            path.display()
                        );
                    }
                    let _ = std::fs::remove_dir_all(e.dir());
                }
                Err(err) => eprintln!("kraal: {} was not saved: {err}; it stays in {}", e.name(), e.dir().display()),
            }
        }
    }
}
