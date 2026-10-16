//! External sources never replace the personal layout without a reviewed, recoverable load.

use anyhow::Context;

use super::{Server, ui::Mode};
use crate::theme::NoticeKind;
use crate::{paths, persist, workspace};

pub(super) fn source_missing(error: &anyhow::Error) -> bool {
    error.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}

impl Server {
    pub(super) fn refresh_source_status(&mut self) {
        let sources = self
            .workspaces
            .iter()
            .filter_map(|ws| {
                let path = ws.definition.as_ref()?;
                Some((path.clone(), workspace::ImportPreview::read(path)))
            })
            .collect::<Vec<_>>();
        if self.apply_source_status(&sources) {
            self.refresh_workspace_pickers();
        }
    }

    pub(super) fn apply_source_status(
        &mut self,
        sources: &[(std::path::PathBuf, anyhow::Result<workspace::ImportPreview>)],
    ) -> bool {
        let mut changed = Vec::new();
        for ws in &mut self.workspaces {
            let preview = match &ws.definition {
                Some(path) => {
                    let Some((_, preview)) = sources.iter().find(|(p, _)| p == path) else { continue };
                    Some(preview)
                }
                None => None,
            };
            let status = match preview {
                Some(Ok(preview)) => {
                    let source = preview.revision.bytes.as_deref();
                    (source != ws.definition_source.as_deref().map(str::as_bytes)
                        && source != ws.source_reviewed.as_deref().map(str::as_bytes))
                    .then_some(("File changed", NoticeKind::Notice))
                }
                Some(Err(e)) if source_missing(e) => Some(("Source file missing", NoticeKind::Warning)),
                Some(Err(_)) => Some(("File error", NoticeKind::Error)),
                None => None,
            };
            if status.map(|(text, _)| text) != ws.source_status {
                ws.source_status = status.map(|(text, _)| text);
                changed.push((ws.id, status));
            }
        }
        if changed.is_empty() {
            return false;
        }
        for (ws, status) in changed {
            if let Some((status, kind)) = status {
                let clients: Vec<_> = self.clients.values().filter(|c| c.workspace == ws).map(|c| c.id).collect();
                for client in clients {
                    self.toast(client, kind, format!("{status} · Review"));
                    self.clients.get_mut(&client).unwrap().toast_action =
                        Some((0, super::attention::ToastAction::ReviewWorkspace(ws)));
                }
            }
        }
        self.mark_dirty();
        true
    }

    pub(super) fn keep_source_layout(&mut self, ws: u32, preview: &workspace::ImportPreview) -> anyhow::Result<()> {
        let wi = self.ws_index(ws).ok_or_else(|| anyhow::anyhow!("Workspace unavailable."))?;
        self.check_source_revision(wi, preview)?;
        let previous = self.workspaces[wi].source_reviewed.clone();
        self.workspaces[wi].source_reviewed = Some(String::from_utf8(preview.revision.bytes.clone().unwrap())?);
        if let Err(e) = self.save_now() {
            self.workspaces[wi].source_reviewed = previous;
            self.persist();
            anyhow::bail!("Couldn't save Keep layout. Retry.\n{e}");
        }
        self.refresh_source_status();
        Ok(())
    }

    fn check_source_revision(&self, wi: usize, preview: &workspace::ImportPreview) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.workspaces[wi].definition.as_ref() == Some(&preview.path)
                && paths::FileRevision::read(&preview.path)? == preview.revision,
            "File changed again. Review the current file before loading."
        );
        Ok(())
    }

    pub(super) fn replace_source_layout(
        &mut self,
        ws: u32,
        preview: Option<&workspace::ImportPreview>,
        approved: Option<&[super::persistence::ClosePane]>,
    ) -> anyhow::Result<()> {
        let wi = self.ws_index(ws).ok_or_else(|| anyhow::anyhow!("Workspace unavailable."))?;
        anyhow::ensure!(
            !self.workspaces[wi].delete_pending
                && self.workspaces[wi].close_error.is_none()
                && self.workspaces[wi].kept.as_ref().is_none_or(|s| s.close_pending.is_none()),
            "Finish Close or Delete in Workspaces, then retry."
        );
        let mut pending_layout = None;
        if let Some(preview) = preview {
            self.check_source_revision(wi, preview)?;
            let checked = self.preview_source(&preview.path)?;
            anyhow::ensure!(
                checked.accounts.is_empty(),
                "The file refers to an unavailable account. Fix the account in the source file or add it in Accounts, then Review again."
            );
            let mut definition = preview.definition.clone();
            super::workspace_files::suspend_import(&mut definition);
            pending_layout = Some(workspace::bind_source_accounts(&workspace::to_kdl(&definition), &self.accounts)?);
        } else {
            anyhow::ensure!(self.workspaces[wi].layout_undo.is_some(), "This replacement was already undone.");
        }
        let inventory = self.source_close_inventory(wi);
        let inventory = approved.unwrap_or(&inventory);
        let action = if preview.is_some() { "Load from file" } else { "Undo load" };
        // Close durably retains the previous layout before any process is signalled.
        if let Err(e) = self.keep_and_close_workspace_checked(ws, |server| {
            if let Some(preview) = preview {
                server.check_source_revision(wi, preview).map_err(|e| format!("{e:#}"))?;
            }
            if server.source_close_inventory(wi) != inventory {
                return Err("Programs changed. Review again before stopping them.".into());
            }
            Ok(())
        }) {
            if self.workspaces[wi].kept.is_some() {
                self.workspaces[wi].close_error = Some(e.clone());
                anyhow::bail!("{e}\nRetry Close in Workspaces, then {action} again.");
            }
            anyhow::bail!("{e}\nRetry {action}.");
        }
        if let Some(preview) = preview {
            self.check_source_revision(wi, preview)
                .context("Programs stopped. Your previous layout is kept closed.")?;
        }
        let old = crate::server::persistence::snapshot(self).workspaces.remove(wi);
        let mut next = if let Some(preview) = preview {
            let mut backup = old.clone();
            backup.layout_undo = None;
            let mut next = old.clone();
            next.tabs.clear();
            next.active = 0;
            next.cwd = preview.definition.cwd.clone();
            next.unopened = true;
            next.pending_layout = pending_layout;
            next.empty = false;
            next.definition_source = Some(String::from_utf8(preview.revision.bytes.clone().unwrap())?);
            next.source_reviewed = None;
            next.layout_undo = Some(Box::new(backup));
            next
        } else {
            let mut next = *old.layout_undo.clone().unwrap();
            next.name = old.name.clone();
            next.definition = old.definition.clone();
            next.layout_undo = None;
            next
        };
        next.closed = true;
        next.close_pending = None;
        next.sync = super::SyncScope::Off;
        // Writer jobs stay ordered even after save_now times out. Protect every file
        // the rollback can name until the replacement or queued rollback is written.
        let retained = persist::scrollback_files(&crate::server::persistence::snapshot(self))
            .into_iter()
            .map(str::to_owned)
            .collect();
        let _ = self.persister.send(super::persistence::Persist::RetainScrollback(retained));
        self.install_source_layout(wi, next);
        if let Err(failure) = self.save_reversible(|server| server.install_source_layout(wi, old)) {
            let _ = self.persister.send(super::persistence::Persist::RetainScrollback(Default::default()));
            self.mark_persist();
            let (error, rollback) = match failure {
                super::persistence::SaveFailure::Initial(error) => (error, String::new()),
                super::persistence::SaveFailure::Rollback { error, rollback } => {
                    (error, format!("\nCouldn't confirm the rollback: {rollback}. Retry before restarting."))
                }
            };
            anyhow::bail!("Programs stopped. The previous layout is kept closed. Retry {action}.\n{error}{rollback}");
        }
        let _ = self.persister.send(super::persistence::Persist::RetainScrollback(Default::default()));
        self.refresh_source_status();
        self.refresh_workspace_pickers();
        self.mark_dirty();
        Ok(())
    }

    pub(super) fn source_close_inventory(&self, wi: usize) -> Vec<super::persistence::ClosePane> {
        self.workspaces[wi]
            .panes()
            .iter()
            .filter_map(|id| self.panes.get(id))
            // Suspended panes run only a short-lived placeholder, never the saved command.
            .filter(|pane| !pane.suspended)
            .map(super::persistence::ClosePane::read)
            .collect()
    }

    fn install_source_layout(&mut self, wi: usize, saved: persist::WorkspaceSnap) {
        let ws = &mut self.workspaces[wi];
        ws.cwd = saved.cwd.clone();
        ws.definition_source = saved.definition_source.clone();
        ws.source_reviewed = saved.source_reviewed.clone();
        ws.layout_undo = saved.layout_undo.clone();
        ws.kept = Some(saved);
    }

    pub(super) fn source_result(&self, client: super::ClientId, ws: u32, undo: bool) -> Mode {
        let mut picker = self.workspace_picker(self.clients.get(&client).map_or(0, |c| c.workspace));
        picker.select(|i| i.action.workspace() == Some(ws));
        picker.result(
            NoticeKind::Notice,
            if undo { "Layout restored · Enter Open" } else { "Layout loaded · Actions: Undo load · Enter Open" },
        )
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::server::persistence::tests::writer;

    const ORIGINAL: &str = "layout { pane command=\"old\"; }";
    const CHANGED: &str = "layout { pane command=\"new\"; pane agent=\"legacy-agent\"; }";

    #[test]
    fn source_changes_are_notices_and_missing_or_invalid_sources_need_attention() {
        let (mut server, path, ws) = setup("notice-kinds");
        let (client, _peer) = super::super::tests::client(&mut server);
        server.clients.get_mut(&client).unwrap().workspace = ws;
        for (source, kind) in
            [(Some(CHANGED), NoticeKind::Notice), (Some("layout {"), NoticeKind::Error), (None, NoticeKind::Warning)]
        {
            if let Some(source) = source {
                std::fs::write(&path, source).unwrap();
            } else {
                std::fs::remove_file(&path).unwrap();
            }
            server.refresh_source_status();
            assert_eq!(server.clients[&client].toast.as_ref().unwrap().2, kind);
            assert_eq!(server.messages.back().unwrap().kind, kind);
        }
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    pub(in crate::server) fn setup(name: &str) -> (Server, std::path::PathBuf, u32) {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = super::super::tests::server(tx);
        let dir = std::env::temp_dir().join(format!("kraal-w11-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(dir).unwrap();
        let path = dir.join("source.kdl");
        std::fs::write(&path, ORIGINAL).unwrap();
        server.config.workspace_dirs = vec![dir.display().to_string()];
        let ws = server.new_workspace("personal".into(), Some(path.clone()));
        server.workspaces[0].definition_source = Some(ORIGINAL.into());
        server
            .spawn_pane_in(
                ws,
                None,
                crate::pane::PaneSpec { files: Some(["/missing".into(), "/missing".into()]), ..Default::default() },
                crate::protocol::Placement::Tab,
                Some("local pane".into()),
            )
            .unwrap();
        writer(&mut server, vec![]);
        (server, path, ws)
    }

    #[test]
    fn personal_layout_edits_never_change_the_source_status_even_after_restart() {
        let (mut server, path, ws) = setup("personal-edits");
        server.refresh_source_status();
        assert_eq!(server.workspaces[0].source_status, None);
        server.workspaces[0].name = "personal name".into();
        server
            .spawn_pane_in(
                ws,
                None,
                crate::pane::PaneSpec { files: Some(["/missing".into(), "/missing".into()]), ..Default::default() },
                crate::protocol::Placement::Tab,
                Some("another local pane".into()),
            )
            .unwrap();
        server.refresh_source_status();
        assert_eq!(server.workspaces[0].source_status, None);
        assert_eq!(server.workspaces[0].counts(), (2, 2));

        for changed in [false, true] {
            if changed {
                std::fs::write(&path, CHANGED).unwrap();
            }
            let expected = changed.then_some("File changed");
            server.refresh_source_status();
            assert_eq!(server.workspaces[0].source_status, expected);
            let saved = serde_json::to_vec(&crate::server::persistence::snapshot(&server)).unwrap();
            let (tx, _rx) = super::super::queue::channel(32);
            let mut restored = super::super::tests::server(tx);
            restored.apply_snapshot(serde_json::from_slice(&saved).unwrap()).unwrap();
            restored.refresh_source_status();
            assert_eq!(restored.workspaces[0].source_status, expected);
            assert_eq!(restored.workspaces[0].counts(), (2, 2));
            let picker = restored.workspace_picker(restored.workspaces[0].id);
            assert_eq!(picker.items[0].detail.contains("File changed"), changed);
        }
        assert_eq!(server.workspaces[0].definition_source.as_deref(), Some(ORIGINAL));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn changed_twice_keep_layout_is_revision_specific_and_never_authorizes_export() {
        let (mut server, path, ws) = setup("twice");
        let before = serde_json::to_value(crate::server::persistence::snapshot(&server)).unwrap();
        std::fs::write(&path, CHANGED).unwrap();
        server.scan_definitions();
        assert_eq!(server.workspaces[0].source_status, Some("File changed"));
        assert_eq!(serde_json::to_value(crate::server::persistence::snapshot(&server)).unwrap(), before);
        let preview = server.preview_source(&path).unwrap();
        let twice = "layout { pane command=\"second\"; }";
        std::fs::write(&path, twice).unwrap();
        assert!(server.keep_source_layout(ws, &preview).unwrap_err().to_string().contains("File changed again"));
        assert!(
            server
                .replace_source_layout(ws, Some(&preview), None)
                .unwrap_err()
                .to_string()
                .contains("File changed again")
        );
        assert!(!server.workspaces[0].closed());
        assert_eq!(server.panes.len(), 1);
        let preview = server.preview_source(&path).unwrap();
        server.keep_source_layout(ws, &preview).unwrap();
        assert_eq!(server.workspaces[0].source_status, None);
        assert_eq!(server.workspaces[0].definition_source.as_deref(), Some(ORIGINAL));
        assert!(server.prepare_layout_export(0, &path).unwrap().question.unwrap().contains("File changed"));
        let (tx, _rx) = super::super::queue::channel(32);
        let mut restored = super::super::tests::server(tx);
        server.keep_and_close_workspace(ws).unwrap();
        restored.apply_snapshot(crate::server::persistence::snapshot(&server)).unwrap();
        restored.refresh_source_status();
        assert_eq!(restored.workspaces[0].source_status, None);
        std::fs::write(&path, CHANGED).unwrap();
        restored.refresh_source_status();
        assert_eq!(restored.workspaces[0].source_status, Some("File changed"));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn invalid_and_missing_sources_keep_one_saved_row_and_open_its_local_layout() {
        let (mut server, path, ws) = setup("errors");
        for (source, status) in [(Some("layout {"), "File error"), (None, "Source file missing")] {
            if let Some(source) = source {
                std::fs::write(&path, source).unwrap();
            } else {
                std::fs::remove_file(&path).unwrap();
            }
            server.scan_definitions();
            assert_eq!(server.workspaces[0].source_status, Some(status));
            let picker = server.workspace_picker(ws);
            assert_eq!(picker.items.len(), 2);
            assert!(picker.items[0].detail.contains(status));
            assert!(picker.action_keys().contains(&("", "Review")));
            assert!(server.preview_source(&path).is_err());
            server.reopen_workspace(ws).unwrap();
            assert_eq!(server.workspaces[0].counts(), (1, 1));
            server.keep_and_close_workspace(ws).unwrap();
            assert_eq!(std::fs::read_to_string(&path).ok().as_deref(), source);
        }
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn load_and_undo_survive_restart_keep_identity_name_order_and_never_write_the_source() {
        let (mut server, path, ws) = setup("undo");
        server.workspaces[0].name = "renamed locally".into();
        let identity = server.workspaces[0].stable_id.clone();
        server.workspace_order = vec![persist::WorkspaceKey::Id(identity.clone())];
        std::fs::write(&path, CHANGED).unwrap();
        let preview = server.preview_source(&path).unwrap();
        server.replace_source_layout(ws, Some(&preview), None).unwrap();
        assert!(server.workspaces[0].closed());
        assert_eq!(server.workspaces[0].counts(), (1, 2));
        let snap = crate::server::persistence::snapshot(&server);
        let (tx, _rx) = super::super::queue::channel(32);
        let mut restored = super::super::tests::server(tx);
        restored.apply_snapshot(serde_json::from_value(serde_json::to_value(snap).unwrap()).unwrap()).unwrap();
        writer(&mut restored, vec![]);
        let id = restored.workspaces[0].id;
        restored.reopen_workspace(id).unwrap();
        assert_eq!(restored.panes.len(), 2);
        assert!(restored.panes.values().all(|p| p.suspended));
        restored.replace_source_layout(id, None, None).unwrap();
        assert!(restored.panes.is_empty());
        let ws = &restored.workspaces[0];
        assert!(ws.closed());
        assert_eq!(ws.counts(), (1, 1));
        assert_eq!(ws.stable_id, identity);
        assert_eq!(ws.name, "renamed locally");
        assert_eq!(ws.definition.as_ref(), Some(&path));
        assert!(ws.layout_undo.is_none());
        assert_eq!(restored.workspace_order, server.workspace_order);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), CHANGED);
        assert!(restored.replace_source_layout(id, None, None).unwrap_err().to_string().contains("already undone"));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn failed_saves_keep_the_old_layout_and_failed_undo_stays_retryable() {
        for stage in 0..3 {
            let (mut server, path, ws) = setup(&format!("failure-{stage}"));
            std::fs::write(&path, CHANGED).unwrap();
            let preview = server.preview_source(&path).unwrap();
            let mut answers = vec![Ok(()); stage];
            answers.push(Err("disk full".into()));
            let saved = writer(&mut server, answers);
            assert!(server.replace_source_layout(ws, Some(&preview), None).is_err());
            assert_eq!(server.workspaces[0].counts(), (1, 1));
            assert_eq!(server.workspaces[0].definition_source.as_deref(), Some(ORIGINAL));
            assert!(server.workspaces[0].layout_undo.is_none());
            if stage == 0 {
                assert_eq!(server.panes.len(), 1);
            }
            if server.workspaces[0].close_error.is_some() {
                server.keep_and_close_workspace(ws).unwrap();
            }
            server.replace_source_layout(ws, Some(&preview), None).unwrap();
            writer(&mut server, vec![Ok(()), Err("disk full".into()), Ok(())]);
            assert!(server.replace_source_layout(ws, None, None).is_err());
            assert_eq!(server.workspaces[0].counts(), (1, 2));
            assert!(server.workspaces[0].layout_undo.is_some());
            server.replace_source_layout(ws, None, None).unwrap();
            assert_eq!(server.workspaces[0].counts(), (1, 1));
            assert!(!saved.lock().unwrap().is_empty());
            std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn a_source_changed_during_close_cancels_the_intent_and_keeps_programs_running() {
        use super::super::persistence::Persist;
        let (mut server, path, ws) = setup("edit-during-close");
        std::fs::write(&path, CHANGED).unwrap();
        let preview = server.preview_source(&path).unwrap();
        let old_pane = server.workspaces[0].panes()[0];
        server.close_pane(old_pane);
        let pane = server
            .spawn_pane_in(
                ws,
                None,
                crate::pane::PaneSpec { command: vec!["sleep".into(), "60".into()], ..Default::default() },
                crate::protocol::Placement::Tab,
                Some("local pane".into()),
            )
            .unwrap();
        let process = server.panes[&pane].process_hold().unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        server.persister = tx;
        let edited = path.clone();
        let writer = std::thread::spawn(move || {
            let mut first = true;
            let mut last = None;
            while let Ok(job) = rx.recv() {
                if let Persist::Save(_, snap) = &job {
                    last = Some(snap.workspaces[0].clone());
                }
                if let Persist::Flush(done) = job {
                    if first {
                        std::fs::write(&edited, "layout { pane command=\"twice\"; }").unwrap();
                        first = false;
                    }
                    let _ = done.send(Ok(()));
                }
            }
            let saved = last.unwrap();
            assert!(saved.close_pending.is_none());
            assert!(!saved.closed);
        });
        let error = format!("{:#}", server.replace_source_layout(ws, Some(&preview), None).unwrap_err());
        assert!(!error.contains("Programs stopped"));
        assert!(error.contains("File changed again"));
        assert!(!server.workspaces[0].closed());
        assert!(server.workspaces[0].kept.is_none());
        assert!(process.group_running());
        assert_eq!(server.workspaces[0].panes(), vec![pane]);
        server.save_now().unwrap();
        assert_eq!(server.workspaces[0].counts(), (1, 1));
        assert!(server.workspaces[0].layout_undo.is_none());
        assert_eq!(server.workspaces[0].definition_source.as_deref(), Some(ORIGINAL));
        drop(server);
        writer.join().unwrap();
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn programs_changed_during_close_cancel_the_intent_before_signalling() {
        use super::super::persistence::Persist;
        let (mut server, path, ws) = setup("program-during-close");
        let gate = path.parent().unwrap().join("continue");
        let ready = path.parent().unwrap().join("ready");
        let pane = server
            .spawn_pane_in(
                ws,
                None,
                crate::pane::PaneSpec {
                    command: vec![
                        "sh".into(),
                        "-c".into(),
                        "while ! test -f \"$1\"; do :; done; sleep 60 & echo ready > \"$2\"; wait".into(),
                        "sh".into(),
                        gate.display().to_string(),
                        ready.display().to_string(),
                    ],
                    ..Default::default()
                },
                crate::protocol::Placement::Tab,
                None,
            )
            .unwrap();
        let process = server.panes[&pane].process_hold().unwrap();
        let preview = server.preview_source(&path).unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        server.persister = tx;
        let writer = std::thread::spawn(move || {
            let mut first = true;
            let mut last = None;
            while let Ok(job) = rx.recv() {
                if let Persist::Save(_, snap) = &job {
                    last = Some(snap.workspaces[0].clone());
                }
                if let Persist::Flush(done) = job {
                    if first {
                        std::fs::write(&gate, "go").unwrap();
                        let start = std::time::Instant::now();
                        while !ready.exists() {
                            assert!(start.elapsed() < std::time::Duration::from_secs(5));
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        first = false;
                    }
                    let _ = done.send(Ok(()));
                }
            }
            let saved = last.unwrap();
            assert!(saved.close_pending.is_none());
            assert!(!saved.closed);
        });
        let error = server.replace_source_layout(ws, Some(&preview), None).unwrap_err().to_string();
        assert!(error.contains("Programs changed"), "{error}");
        assert!(process.group_running());
        assert!(!server.workspaces[0].closed());
        assert!(server.workspaces[0].kept.is_none());
        server.save_now().unwrap();
        assert!(crate::server::persistence::snapshot(&server).workspaces[0].close_pending.is_none());
        drop(server);
        writer.join().unwrap();
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn timed_out_undo_retains_scrollback_when_the_late_write_cleans_before_rollback() {
        use super::super::persistence::Persist;
        let (mut server, path, ws) = setup("late-undo");
        std::fs::write(&path, CHANGED).unwrap();
        let preview = server.preview_source(&path).unwrap();
        server.replace_source_layout(ws, Some(&preview), None).unwrap();
        server.reopen_workspace(ws).unwrap();
        server.keep_and_close_workspace(ws).unwrap();
        let saved = server.workspaces[0].kept.as_mut().unwrap();
        let persist::NodeSnap::Split { children, .. } = saved.tabs[0].root.as_mut().unwrap() else { panic!() };
        let persist::NodeSnap::Pane(pane) = &mut children[0].0 else { panic!() };
        pane.scrollback = Some("1-2-3.ans".into());
        let dir = path.parent().unwrap().join("scrollback");
        std::fs::create_dir(&dir).unwrap();
        let file = dir.join("1-2-3.ans");
        std::fs::write(&file, "replacement scrollback").unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        server.persister = tx;
        let writer = std::thread::spawn(move || {
            let mut retained = Default::default();
            let mut replacing = false;
            let mut late = None;
            let mut timed_out = None;
            let mut cleaned = false;
            while let Ok(job) = rx.recv() {
                match job {
                    Persist::RetainScrollback(files) => {
                        retained = files;
                        replacing = true;
                    }
                    Persist::Save(_, snap) if replacing && late.is_none() && !cleaned => late = Some(snap),
                    Persist::Flush(done) if late.is_some() => timed_out = Some(done),
                    Persist::Save(_, snap) => {
                        if let Some(late) = late.take() {
                            // Receiving the rollback proves save_now has timed out. Publish
                            // the delayed Undo and clean its files before writing rollback.
                            assert!(timed_out.take().unwrap().send(Ok(())).is_err());
                            assert!(!persist::scrollback_files(&late).contains("1-2-3.ans"));
                            std::fs::write(dir.join("state.json"), serde_json::to_vec(&late).unwrap()).unwrap();
                            persist::clean_scrollback_in(&dir, &late, &retained);
                            assert_eq!(std::fs::read_to_string(&file).unwrap(), "replacement scrollback");
                            assert!(persist::scrollback_files(&snap).contains("1-2-3.ans"));
                            cleaned = true;
                        }
                        std::fs::write(dir.join("state.json"), serde_json::to_vec(&snap).unwrap()).unwrap();
                        persist::clean_scrollback_in(&dir, &snap, &retained);
                    }
                    Persist::Flush(done) => {
                        let _ = done.send(Ok(()));
                    }
                    _ => {}
                }
            }
            assert!(cleaned);
            assert_eq!(std::fs::read_to_string(file).unwrap(), "replacement scrollback");
        });
        let error = server.replace_source_layout(ws, None, None).unwrap_err().to_string();
        assert!(error.contains("did not finish in time"), "{error}");
        assert!(server.workspaces[0].layout_undo.is_some());
        assert_eq!(server.workspaces[0].counts(), (1, 2));
        drop(server);
        writer.join().unwrap();
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn exporting_a_closed_load_keeps_its_prepared_layout_and_export_baseline_after_restart() {
        let (mut server, path, ws) = setup("export-before-open");
        std::fs::write(&path, CHANGED).unwrap();
        let preview = server.preview_source(&path).unwrap();
        server.replace_source_layout(ws, Some(&preview), None).unwrap();
        let save = server.prepare_layout_export(0, &path).unwrap();
        server.write_definition(0, "personal", &save).unwrap();
        let exported = std::fs::read_to_string(&path).unwrap();
        assert_ne!(exported, CHANGED);
        let (tx, _rx) = super::super::queue::channel(32);
        let mut restored = super::super::tests::server(tx);
        restored.apply_snapshot(crate::server::persistence::snapshot(&server)).unwrap();
        restored.refresh_source_status();
        assert_eq!(restored.workspaces[0].source_status, None);
        assert_eq!(restored.workspaces[0].definition_source.as_deref(), Some(exported.as_str()));
        restored.reopen_workspace(restored.workspaces[0].id).unwrap();
        assert_eq!(restored.panes.len(), 2);
        assert!(restored.panes.values().all(|p| p.suspended));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn unavailable_accounts_and_failed_keep_layout_leave_the_workspace_unchanged() {
        let (mut server, path, ws) = setup("account-and-keep");
        std::fs::write(&path, "layout { pane agent=\"claude:missing-account\"; }").unwrap();
        let preview = server.preview_source(&path).unwrap();
        let before = serde_json::to_value(crate::server::persistence::snapshot(&server)).unwrap();
        assert!(
            server
                .replace_source_layout(ws, Some(&preview), None)
                .unwrap_err()
                .to_string()
                .contains("unavailable account")
        );
        assert_eq!(serde_json::to_value(crate::server::persistence::snapshot(&server)).unwrap(), before);
        writer(&mut server, vec![Err("disk full".into())]);
        assert!(server.keep_source_layout(ws, &preview).unwrap_err().to_string().contains("Retry"));
        assert!(server.workspaces[0].source_reviewed.is_none());
        assert_eq!(server.panes.len(), 1);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn undo_scrollback_is_retained_and_missing_fields_keep_old_snapshots_compatible() {
        let (mut server, path, ws) = setup("scrollback");
        server.keep_and_close_workspace(ws).unwrap();
        let old = server.workspaces[0].kept.as_mut().unwrap();
        let persist::NodeSnap::Pane(pane) = old.tabs[0].root.as_mut().unwrap() else { panic!() };
        pane.scrollback = Some("1-2-3.ans".into());
        std::fs::write(&path, CHANGED).unwrap();
        let preview = server.preview_source(&path).unwrap();
        server.replace_source_layout(ws, Some(&preview), None).unwrap();
        let snap = crate::server::persistence::snapshot(&server);
        assert!(persist::scrollback_files(&snap).contains("1-2-3.ans"));
        server.config.save_scrollback = false;
        assert!(persist::scrollback_files(&crate::server::persistence::snapshot(&server)).is_empty());
        let copy = snap.workspaces[0].duplicate("copy").unwrap();
        assert!(copy.layout_undo.is_none());
        assert!(copy.source_reviewed.is_none());
        let mut json = serde_json::to_value(snap).unwrap();
        json["workspaces"][0].as_object_mut().unwrap().remove("layout_undo");
        json["workspaces"][0].as_object_mut().unwrap().remove("source_reviewed");
        let old: persist::SessionSnapshot = serde_json::from_value(json).unwrap();
        assert!(old.workspaces[0].layout_undo.is_none());
        assert!(old.workspaces[0].source_reviewed.is_none());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
