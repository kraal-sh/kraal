//! Workspace copies and recoverable deletion are durable before their result is shown.

use std::path::{Path, PathBuf};

use super::ui::{ConfirmAction, Mode, PickAction, PickItem, Picker, PickerKind, Prompt, PromptAction};
use super::{ClientId, Server};
use crate::persist::{DeletedWorkspace, WorkspaceSnap};

#[derive(Clone)]
pub enum DeleteTarget {
    Workspace(u32),
    Source(PathBuf),
}

impl Server {
    pub(super) fn duplicate_workspace(&mut self, source: u32, name: &str) -> Result<u32, String> {
        let wi = self
            .ws_index(source)
            .ok_or("This workspace is no longer available. Cancel and choose another workspace.")?;
        let workspace = &self.workspaces[wi];
        if workspace.delete_pending || workspace.kept.as_ref().is_some_and(|s| s.close_pending.is_some()) {
            return Err("This workspace has an unfinished Close or Delete. Cancel and finish that action, then retry Duplicate.".into());
        }
        let copy = crate::persist::snapshot(self).workspaces[wi]
            .duplicate(name)
            .map_err(|e| format!("Couldn't copy the layout. Fix the saved layout and retry.\n{e:#}"))?;
        crate::persist::apply_workspace(self, &copy).map_err(|e| format!("Couldn't copy the workspace.\n{e:#}"))?;
        let ws = self.workspaces.last().unwrap().id;
        if let Err(e) = self.save_now() {
            self.workspaces.pop();
            // A timed-out writer can still publish the copy. Queue its removal after it.
            if let Err(rollback) = self.save_now() {
                self.mark_persist();
                return Err(format!(
                    "Couldn't save the copy or confirm its rollback. The saved session may still contain {name}.\nFix the save error and retry.\n{e}\n{rollback}"
                ));
            }
            return Err(format!("Couldn't save the copy. No workspace was added.\n{e}"));
        }
        Ok(ws)
    }

    pub(super) fn source_removed(&self, path: &Path) -> bool {
        self.removed_sources.iter().any(|p| crate::paths::same_file(p, path))
    }

    pub(super) fn ask_delete_workspace(&self, target: DeleteTarget) -> Mode {
        let (name, source, programs) = match &target {
            DeleteTarget::Workspace(ws) => {
                let Some(wi) = self.ws_index(*ws) else {
                    return self.workspace_picker(0).result("Workspace unavailable");
                };
                let workspace = &self.workspaces[wi];
                if workspace.delete_pending {
                    return Mode::confirm(
                        workspace.close_error.clone().unwrap_or_else(|| "Finish deleting this workspace?".into()),
                        ConfirmAction::RetryDeleteWorkspace(target),
                    );
                }
                (workspace.name.clone(), workspace.definition.clone(), self.stops(Some(*ws)))
            }
            DeleteTarget::Source(path) => {
                (path.file_stem().unwrap_or_default().to_string_lossy().into_owned(), Some(path.clone()), None)
            }
        };
        let stop = programs.is_some();
        let mut message =
            format!("Delete {name}?\nYou can restore it from Deleted workspaces.\nProject files will be kept.");
        if let Some(source) = source {
            message.push_str(&format!("\nSource file {} will be kept.", crate::paths::shorten_home(&source)));
        }
        if let Some(programs) = programs {
            message.push_str(&format!("\n{programs}\nUnsaved program work cannot be restored."));
        }
        Mode::confirm(message, ConfirmAction::DeleteWorkspace { target, stop })
    }

    pub(super) fn delete_workspace(&mut self, client: ClientId, target: DeleteTarget) -> Mode {
        let result = match &target {
            DeleteTarget::Workspace(ws) => self.keep_and_delete_workspace(*ws),
            DeleteTarget::Source(path) => self.delete_workspace_source(path),
        };
        match result {
            Ok(identity) => {
                self.refresh_workspace_pickers();
                self.mark_dirty();
                self.toast(client, "Workspace deleted · Undo");
                if let Some(c) = self.clients.get_mut(&client) {
                    c.toast_action = Some((0, super::attention::ToastAction::RestoreWorkspace(identity.clone())));
                }
                let mut picker = self.workspace_picker(self.clients.get(&client).map_or(0, |c| c.workspace));
                picker.undo = Some(identity);
                picker.result("Workspace deleted")
            }
            Err(message) => {
                if let DeleteTarget::Workspace(ws) = target
                    && let Some(wi) = self.ws_index(ws)
                    && self.workspaces[wi].delete_pending
                {
                    self.workspaces[wi].close_error = Some(message.clone());
                }
                self.refresh_workspace_pickers();
                self.mark_dirty();
                Mode::confirm(message, ConfirmAction::RetryDeleteWorkspace(target))
            }
        }
    }

    pub(super) fn keep_and_delete_workspace(&mut self, ws: u32) -> Result<String, String> {
        let wi = self.ws_index(ws).ok_or("This workspace is no longer available.")?;
        let retry = self.workspaces[wi].delete_pending;
        let was_kept = self.workspaces[wi].kept.clone();
        self.workspaces[wi].delete_pending = true;
        if let Err(e) = self.keep_and_close_workspace(ws) {
            // A failed preparation must cancel even a writer that finishes after its timeout.
            let prepared = self.workspaces[wi].kept.as_ref().is_some_and(|s| s.close_pending.is_some() || s.closed);
            if !retry && (!prepared || was_kept.as_ref().is_some_and(|s| s.closed)) {
                self.workspaces[wi].delete_pending = false;
                self.workspaces[wi].kept = was_kept;
                if let Err(rollback) = self.save_now() {
                    self.mark_persist();
                    return Err(format!(
                        "Couldn't confirm cancellation of the deletion. Nothing was stopped.\nFix the save error and retry Delete.\n{e}\n{rollback}"
                    ));
                }
                return Err(format!("Couldn't save the deletion.\nNothing was stopped or deleted.\n{e}"));
            }
            return Err(format!(
                "Couldn't finish deleting the workspace. Its layout is kept.\nRetry Delete to finish.\n{e}"
            ));
        }
        let mut saved = crate::persist::snapshot(self).workspaces.remove(wi);
        saved.delete_pending = false;
        let identity = saved.id.clone();
        let order_position = self
            .updated_workspace_order()
            .iter()
            .position(|key| *key == crate::persist::WorkspaceKey::Id(identity.clone()));
        let sources = self.removed_sources.clone();
        if let Some(path) = &saved.definition {
            self.remove_workspace_source(path);
        }
        let workspace = self.workspaces.remove(wi);
        self.deleted_workspaces.push(DeletedWorkspace {
            workspace: saved,
            position: wi,
            order_position,
            deleted_at: crate::accounts::now(),
        });
        if let Err(e) = self.save_now() {
            self.deleted_workspaces.pop();
            self.removed_sources = sources;
            self.workspaces.insert(wi, workspace);
            self.mark_persist();
            return Err(format!(
                "Programs stopped. Couldn't save the final deletion.\nThe prepared layout is kept. Retry Delete.\n{e}"
            ));
        }
        let fallback = self.workspaces.iter().find(|w| !w.closed() && !w.delete_pending).map_or(0, |w| w.id);
        let clients: Vec<_> = self.clients.values().filter(|c| c.workspace == ws).map(|c| c.id).collect();
        for id in clients {
            let mode = (fallback == 0).then(|| Mode::Picker(self.workspace_picker(0)));
            let c = self.clients.get_mut(&id).unwrap();
            c.workspace = fallback;
            if let Some(mode) = mode {
                c.mode = mode;
            }
        }
        self.scan_definitions();
        self.relayout();
        Ok(identity)
    }

    fn remove_workspace_source(&mut self, path: &Path) {
        if !self.source_removed(path) {
            self.removed_sources.push(crate::workspace::definition_path(path));
        }
    }

    fn delete_workspace_source(&mut self, path: &Path) -> Result<String, String> {
        if self.source_removed(path) {
            return Err("This source was already deleted. Open Deleted workspaces to restore it.".into());
        }
        if self.definition_workspace(path).map_err(|e| format!("{e:#}"))?.is_some() {
            return Err("This file was opened meanwhile. Cancel and select its workspace to delete it.".into());
        }
        if let Some(saved) =
            self.unrestored.iter().find(|w| w.definition.as_deref().is_some_and(|p| crate::paths::same_file(p, path)))
        {
            return Err(format!(
                "This file belongs to workspace {:?}, which could not be restored.\nFix the restore error and restart, then delete that workspace.\n{}",
                saved.name,
                saved.restore_error.as_deref().unwrap_or("its panes could not be started")
            ));
        }
        let source = crate::paths::read_regular_text(path, u64::MAX)
            .map_err(|e| format!("Couldn't keep the source. Nothing was deleted. Fix the file and retry.\n{e:#}"))?;
        let name = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let definition = crate::workspace::parse_workspace(&name, &source).map_err(|e| {
            format!("Couldn't keep the layout. Nothing was deleted. Fix the source file and retry.\n{e:#}")
        })?;
        let pending_layout = crate::workspace::bind_source_accounts(&source, &self.accounts)
            .map_err(|e| format!("Couldn't keep the accounts. Nothing was deleted.\n{e:#}"))?;
        let identity = crate::accounts::new_uuid();
        let saved = WorkspaceSnap {
            id: identity.clone(),
            name,
            definition: Some(crate::workspace::definition_path(path)),
            definition_source: Some(source),
            source_reviewed: None,
            layout_undo: None,
            cwd: definition.cwd,
            active: 0,
            tabs: Vec::new(),
            closed: true,
            empty: false,
            close_pending: None,
            delete_pending: false,
            unopened: true,
            sync: super::SyncScope::Off,
            restore_error: None,
            pending_layout: Some(pending_layout),
        };
        let position = self
            .workspace_picker(0)
            .items
            .iter()
            .position(|i| matches!(&i.action, PickAction::OpenDefinition(p) if crate::paths::same_file(p, path)))
            .unwrap_or(self.workspaces.len());
        self.deleted_workspaces.push(DeletedWorkspace {
            workspace: saved,
            position,
            order_position: self.updated_workspace_order().iter().position(
                |key| matches!(key, crate::persist::WorkspaceKey::Source(p) if crate::paths::same_file(p, path)),
            ),
            deleted_at: crate::accounts::now(),
        });
        let order = self.workspace_order.clone();
        for key in &mut self.workspace_order {
            if matches!(key, crate::persist::WorkspaceKey::Source(p) if crate::paths::same_file(p, path)) {
                *key = crate::persist::WorkspaceKey::Id(identity.clone());
            }
        }
        let sources = self.removed_sources.clone();
        self.remove_workspace_source(path);
        if let Err(e) = self.save_now() {
            self.deleted_workspaces.pop();
            self.workspace_order = order;
            self.removed_sources = sources;
            if let Err(rollback) = self.save_now() {
                self.mark_persist();
                return Err(format!(
                    "Couldn't confirm cancellation of the deletion. The source file is kept.\nFix the save error and retry.\n{e}\n{rollback}"
                ));
            }
            return Err(format!("Couldn't save the deletion. Nothing was stopped or deleted.\n{e}"));
        }
        self.scan_definitions();
        Ok(identity)
    }

    pub(super) fn deleted_workspace_picker(&self) -> Picker {
        let items = self
            .deleted_workspaces
            .iter()
            .rev()
            .map(|d| {
                let (tabs, panes) = d.workspace.counts();
                let days = crate::accounts::now().saturating_sub(d.deleted_at).max(0) / 86400;
                let when = if days == 0 {
                    "Deleted today".into()
                } else {
                    format!("Deleted {days} day{} ago", super::input::plural(days as usize))
                };
                PickItem::new(
                    &d.workspace.name,
                    format!(
                        "{when} · {tabs} tab{} · {panes} pane{}",
                        super::input::plural(tabs),
                        super::input::plural(panes)
                    ),
                    PickAction::RestoreWorkspace(d.workspace.id.clone()),
                )
            })
            .collect();
        Picker::new(PickerKind::DeletedWorkspaces, "Deleted workspaces", items)
    }

    pub(super) fn restore_workspace_mode(&mut self, client: ClientId, identity: &str, from: Option<&Picker>) -> Mode {
        let Some(saved) = self.deleted_workspaces.iter().find(|d| d.workspace.id == identity) else {
            return self.deleted_workspace_picker().result("Workspace already restored");
        };
        let name = saved.workspace.name.clone();
        if self.workspace_name_used(&name, None) {
            let base = format!("{name}-restored");
            let proposed = std::iter::once(base.clone())
                .chain((2..).map(|n| format!("{base}-{n}")))
                .find(|n| !self.workspace_name_used(n, None))
                .unwrap();
            let mut p = Prompt::new("Restore workspace", &proposed, PromptAction::RestoreWorkspace(identity.into()));
            p.error = Some(format!("{name} is already used. Choose another name."));
            self.workspace_restore_back(&mut p, from);
            return Mode::Prompt(p);
        }
        self.restore_workspace_result(client, identity, &name, from)
    }

    pub(super) fn restore_workspace_result(
        &mut self,
        client: ClientId,
        identity: &str,
        name: &str,
        from: Option<&Picker>,
    ) -> Mode {
        let had_source =
            self.deleted_workspaces.iter().any(|d| d.workspace.id == identity && d.workspace.definition.is_some());
        match self.restore_deleted_workspace(identity, name) {
            Ok(ws) => {
                self.refresh_workspace_pickers();
                self.mark_dirty();
                self.toast(client, "Workspace restored");
                let mut picker = self.workspace_picker(0);
                picker.select(|i| i.action.workspace() == Some(ws));
                let workspace = &self.workspaces[self.ws_index(ws).unwrap()];
                if had_source && workspace.definition.is_none() {
                    picker.result("Workspace restored · Source belongs to another workspace · Enter Open")
                } else {
                    picker.result("Workspace restored · Enter Open")
                }
            }
            Err(error) => {
                let mut p = Prompt::new("Restore workspace", name, PromptAction::RestoreWorkspace(identity.into()));
                p.error = Some(error);
                self.workspace_restore_back(&mut p, from);
                Mode::Prompt(p)
            }
        }
    }

    pub(super) fn restore_deleted_workspace(&mut self, identity: &str, name: &str) -> Result<u32, String> {
        let at = self
            .deleted_workspaces
            .iter()
            .position(|d| d.workspace.id == identity)
            .ok_or("This workspace was already restored. Cancel to return to the list.")?;
        if self.workspace_name_used(name, None) {
            return Err("This name is already used. Choose another name.".into());
        }
        let deleted = self.deleted_workspaces.remove(at);
        let mut saved = deleted.workspace.clone();
        // A replacement keeps its source. The restored copy keeps its contents and can
        // be saved under another file name after Open.
        if saved.definition.as_deref().is_some_and(|path| {
            self.workspaces.iter().any(|w| w.definition.as_deref().is_some_and(|p| crate::paths::same_file(p, path)))
                || self
                    .unrestored
                    .iter()
                    .any(|w| w.definition.as_deref().is_some_and(|p| crate::paths::same_file(p, path)))
        }) {
            saved.definition = None;
        }
        saved.name = name.into();
        saved.closed = true;
        saved.delete_pending = false;
        saved.close_pending = None;
        saved.restore_error = None;
        crate::persist::apply_workspace(self, &saved).map_err(|e| format!("{e:#}"))?;
        let workspace = self.workspaces.pop().unwrap();
        let ws = workspace.id;
        let position = deleted.position.min(self.workspaces.len());
        self.workspaces.insert(position, workspace);
        let sources = self.removed_sources.clone();
        if let Some(path) = &saved.definition {
            self.removed_sources.retain(|p| !crate::paths::same_file(p, path));
        }
        let order = self.workspace_order.clone();
        self.bind_workspace_order(ws);
        let key = crate::persist::WorkspaceKey::Id(saved.id.clone());
        if !self.workspace_order.contains(&key) {
            self.workspace_order = self.updated_workspace_order();
            self.workspace_order.retain(|k| *k != key);
            self.workspace_order
                .insert(deleted.order_position.unwrap_or(deleted.position).min(self.workspace_order.len()), key);
        }
        if let Err(e) = self.save_now() {
            self.workspaces.remove(position);
            self.workspace_order = order;
            self.deleted_workspaces.insert(at, deleted);
            self.removed_sources = sources;
            if let Err(rollback) = self.save_now() {
                self.mark_persist();
                return Err(format!(
                    "Couldn't confirm the restore or its rollback. No programs were started.\nFix the save error and retry.\n{e}\n{rollback}"
                ));
            }
            return Err(format!("Couldn't save the restore. The workspace stays in Deleted workspaces.\n{e}"));
        }
        self.sort_workspaces();
        self.scan_definitions();
        Ok(ws)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::PaneSpec;
    use crate::persist::{self, NodeSnap};
    use crate::protocol::Placement;
    use crate::server::persistence::tests::writer;

    fn until(server: &mut Server, rx: &super::super::queue::EventReceiver, done: impl Fn(&Server) -> bool) {
        let start = std::time::Instant::now();
        loop {
            while let Ok(event) = rx.try_recv() {
                server.handle(event);
            }
            if done(server) {
                break;
            }
            assert!(start.elapsed() < crate::testing::PATIENCE, "the copied pane did not reach the expected state");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[test]
    fn duplicate_managed_claude_recovers_its_new_conversation_without_hooks() {
        let (tx, rx) = super::super::queue::channel(4096);
        let mut server = super::super::tests::server(tx);
        let path = source(&mut server);
        let dir = path.parent().unwrap();
        let program = dir.join("claude");
        crate::testing::write_script(
            &program,
            r#"#!/bin/sh
mkdir -p "$CLAUDE_CONFIG_DIR/projects/work"
printf '{"message":1}\n' >> "$CLAUDE_CONFIG_DIR/projects/work/$2.jsonl"
printf '%s\n' "$*" >> "$CLAUDE_CONFIG_DIR/args-$2"
while IFS= read -r line; do :; done
"#,
        );
        let account = server
            .accounts
            .add(
                serde_json::from_value(serde_json::json!({
                    "name": "work", "agent": "claude", "home": dir
                }))
                .unwrap(),
            )
            .unwrap();
        let ws = server.new_workspace("api".into(), None);
        let mut spec = server.agent_spec(&account, Some(dir.into()), None, &[]).unwrap();
        spec.command[0] = program.display().to_string();
        let original = spec.session.clone().unwrap();
        server.spawn_pane_in(ws, None, spec, Placement::Tab, None).unwrap();
        writer(&mut server, vec![]);
        let copied = server.duplicate_workspace(ws, "api-copy").unwrap();
        server.reopen_workspace(copied).unwrap();
        let pane = server.workspaces[server.ws_index(copied).unwrap()].panes()[0];
        let session = server.panes[&pane].spec.session.clone().expect("the copy reserves its own conversation");
        assert_ne!(session, original);
        let launches = dir.join(format!("args-{session}"));
        assert!(!launches.exists());
        let (client, _peer) = super::super::tests::client(&mut server);
        server.reveal(client, pane);
        until(&mut server, &rx, |s| s.panes[&pane].exit_code.is_some());
        server.client_input(client, b"\r");
        let fresh = format!("--session-id {session}\n");
        until(&mut server, &rx, |_| std::fs::read_to_string(&launches).is_ok_and(|s| s == fresh));
        assert!(!server.panes[&pane].agent.hooked);
        assert!(server.panes[&pane].agent.session_id.is_none());
        server.keep_and_close_workspace(copied).unwrap();
        server.reopen_workspace(copied).unwrap();
        let resumed = format!("{fresh}--resume {session}\n");
        until(&mut server, &rx, |_| std::fs::read_to_string(&launches).is_ok_and(|s| s == resumed));
        let saved = persist::snapshot(&server).workspaces.remove(server.ws_index(copied).unwrap());
        server.keep_and_close_workspace(copied).unwrap();
        server.keep_and_close_workspace(ws).unwrap();
        let (tx, rx) = super::super::queue::channel(4096);
        let mut restarted = super::super::tests::server(tx);
        restarted.accounts.add(account).unwrap();
        persist::apply_workspace(&mut restarted, &saved).unwrap();
        let resumed = format!("{resumed}--resume {session}\n");
        until(&mut restarted, &rx, |_| std::fs::read_to_string(&launches).is_ok_and(|s| s == resumed));
        writer(&mut restarted, vec![]);
        restarted.keep_and_close_workspace(restarted.workspaces[0].id).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn duplicate_keeps_open_closed_and_empty_sources_and_opens_with_new_panes() {
        for (closed, empty) in [(false, false), (true, false), (false, true)] {
            let (tx, _rx) = super::super::queue::channel(4096);
            let mut server = super::super::tests::server(tx);
            let path = source(&mut server);
            let original_file = std::fs::read(&path).unwrap();
            let def = crate::workspace::parse_workspace("api", r#"layout {
                cwd "/tmp"
                tab name="work" {
                    pane stacked=true {
                        pane name="ssh" command="ssh" start_suspended=true { args "host"; }
                        pane name="agent" command="claude" start_suspended=true expanded=true { args "--resume" "original" "--model" "opus"; }
                    }
                    floating_panes { pane name="job" command="sleep" start_suspended=true { args "60"; }; }
                }
                tab name="tests" focus=true { pane command="cargo" start_suspended=true { args "test"; }; }
            }"#).unwrap();
            let ws = if empty {
                server.new_workspace("api".into(), Some(path.clone()))
            } else {
                server.start_definition_inner(&def, Some(path.clone())).unwrap()
            };
            let saved = writer(&mut server, vec![]);
            if closed {
                server.keep_and_close_workspace(ws).unwrap();
            }
            let source_before = serde_json::to_value(persist::snapshot(&server).workspaces.remove(0)).unwrap();
            let pane_ids: std::collections::HashSet<_> = server.panes.keys().copied().collect();
            let tab_ids: Vec<_> = server.workspaces[0].tabs.iter().map(|t| t.id).collect();
            let copied = server.duplicate_workspace(ws, "api-copy").unwrap();
            assert_eq!(server.panes.keys().copied().collect::<std::collections::HashSet<_>>(), pane_ids);
            let copy = &server.workspaces[1];
            assert_eq!(copy.id, copied);
            assert!(copy.closed());
            assert_ne!(copy.stable_id, server.workspaces[0].stable_id);
            assert!(copy.definition.is_none());
            assert_eq!(copy.counts(), server.workspaces[0].counts());
            let persisted = saved.lock().unwrap().last().unwrap().clone();
            assert_eq!(persisted["workspaces"][1]["closed"], true);
            assert_eq!(persisted["workspaces"][1]["id"], copy.stable_id);
            assert_eq!(persisted["workspaces"][0], source_before);
            server.reopen_workspace(copied).unwrap();
            for tab in &server.workspaces[1].tabs {
                assert!(!tab_ids.contains(&tab.id));
                for id in tab.panes() {
                    assert!(!pane_ids.contains(&id));
                    let pane = &server.panes[&id];
                    assert!(pane.suspended);
                    assert!(!pane.spec.command.iter().any(|s| s == "original" || s == "--resume"));
                    assert!(pane.agent.session_id.is_none());
                }
            }
            assert_eq!(serde_json::to_value(persist::snapshot(&server).workspaces.remove(0)).unwrap(), source_before);
            let (tx, _rx) = super::super::queue::channel(4096);
            let mut restarted = super::super::tests::server(tx);
            persist::apply(&mut restarted, serde_json::from_value(persisted).unwrap()).unwrap();
            assert!(restarted.workspaces[1].closed());
            let ws = restarted.workspaces[1].id;
            restarted.reopen_workspace(ws).unwrap();
            assert!(restarted.workspaces[1].tabs.iter().flat_map(|t| t.panes()).all(|p| restarted.panes[&p].suspended));
            assert_eq!(std::fs::read(&path).unwrap(), original_file);
            assert!(!path.with_extension("kdl.bak").exists());
            std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn duplicate_save_failure_rolls_back_and_retry_adds_only_one_copy() {
        for rollback_fails in [false, true] {
            let (tx, _rx) = super::super::queue::channel(4096);
            let mut server = super::super::tests::server(tx);
            let ws = workspace(&mut server);
            let before = serde_json::to_value(persist::snapshot(&server)).unwrap();
            let saved = writer(
                &mut server,
                vec![Err("disk full".into()), if rollback_fails { Err("still full".into()) } else { Ok(()) }],
            );
            let error = server.duplicate_workspace(ws, "api-copy").unwrap_err();
            assert!(error.contains(if rollback_fails {
                "saved session may still contain api-copy"
            } else {
                "No workspace was added"
            }));
            assert_eq!(serde_json::to_value(persist::snapshot(&server)).unwrap(), before);
            assert_eq!(saved.lock().unwrap().last().unwrap(), &before);
            let copied = server.duplicate_workspace(ws, "api-copy").unwrap();
            assert_eq!(server.workspaces.len(), 2);
            assert!(server.workspaces[server.ws_index(copied).unwrap()].closed());
            assert_eq!(saved.lock().unwrap().last().unwrap()["workspaces"].as_array().unwrap().len(), 2);
        }
    }

    #[test]
    fn duplicate_refuses_a_disappeared_source_or_unfinished_lifecycle_action() {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = super::super::tests::server(tx);
        let ws = server.new_workspace("api".into(), None);
        server.workspaces[0].delete_pending = true;
        assert!(server.duplicate_workspace(ws, "copy").unwrap_err().contains("finish that action"));
        server.workspaces.clear();
        assert!(server.duplicate_workspace(ws, "copy").unwrap_err().contains("choose another workspace"));
        assert!(server.workspaces.is_empty());
    }

    #[test]
    fn duplicate_of_a_restored_unimported_file_uses_its_kept_contents_and_waits() {
        let (tx, _rx) = super::super::queue::channel(4096);
        let mut server = super::super::tests::server(tx);
        let path = source(&mut server);
        writer(&mut server, vec![]);
        let deleted = server.delete_workspace_source(&path).unwrap();
        let restored = server.restore_deleted_workspace(&deleted, "api").unwrap();
        std::fs::remove_file(&path).unwrap();
        let copied = server.duplicate_workspace(restored, "api-copy").unwrap();
        assert!(server.panes.is_empty());
        server.reopen_workspace(copied).unwrap();
        assert!(server.panes.values().all(|p| p.suspended));
        assert!(server.workspaces[server.ws_index(copied).unwrap()].definition.is_none());
        assert!(!path.exists());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn duplicate_retained_agents_wait_for_enter_in_the_shell_and_respect_command_precedence() {
        for (agent, command) in [("q chat", false), ("q chat", true), ("claude:missing", true)] {
            let (tx, rx) = super::super::queue::channel(4096);
            let mut server = super::super::tests::server(tx);
            let path = source(&mut server);
            let dir = path.parent().unwrap();
            let launched = dir.join("launches");
            crate::testing::write_script(&dir.join("q"), "#!/bin/sh\nprintf '%s\\n' \"$*\" > launches\n");
            let shell = dir.join("shell");
            crate::testing::write_script(
                &shell,
                &format!(
                    "#!/bin/sh\nexport PATH={}:\"$PATH\"\nexec sh\n",
                    crate::shell::quote(&dir.display().to_string())
                ),
            );
            server.env.shell = shell.display().to_string();
            let command = if command { r#"command="sh" { args "-c" "printf explicit > launches"; }"# } else { "" };
            std::fs::write(
                &path,
                format!("layout {{ cwd {:?}; pane agent={agent:?} {command}; }}", dir.display().to_string()),
            )
            .unwrap();
            writer(&mut server, vec![]);
            let deleted = server.delete_workspace_source(&path).unwrap();
            let restored = server.restore_deleted_workspace(&deleted, "api").unwrap();
            let copied = server.duplicate_workspace(restored, "api-copy").unwrap();
            assert!(server.panes.is_empty());
            server.reopen_workspace(copied).unwrap();
            let pane = server.workspaces[server.ws_index(copied).unwrap()].panes()[0];
            assert!(server.panes[&pane].suspended);
            assert!(!launched.exists());
            // Saving and reopening before Enter must keep the deferred shell input too.
            server.keep_and_close_workspace(copied).unwrap();
            server.reopen_workspace(copied).unwrap();
            let pane = server.workspaces[server.ws_index(copied).unwrap()].panes()[0];
            let (client, _peer) = super::super::tests::client(&mut server);
            server.reveal(client, pane);
            until(&mut server, &rx, |s| s.panes[&pane].exit_code.is_some());
            assert!(!launched.exists());
            if command.is_empty() {
                assert!(server.panes[&pane].text(None).contains("q chat"));
                assert!(server.panes[&pane].spec.command.is_empty());
                assert_eq!(server.panes[&pane].spec.startup_input.as_deref(), Some("q chat\r"));
            } else {
                assert_eq!(server.panes[&pane].spec.command, ["sh", "-c", "printf explicit > launches"]);
                assert!(server.panes[&pane].spec.startup_input.is_none());
            }
            server.client_input(client, b"\r");
            let expected = if command.is_empty() { "chat\n" } else { "explicit" };
            until(&mut server, &rx, |_| std::fs::read_to_string(&launched).is_ok_and(|s| s == expected));
            assert!(!server.panes[&pane].suspended);
            server.keep_and_close_workspace(copied).unwrap();
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    fn workspace(server: &mut Server) -> u32 {
        let ws = server.new_workspace("api".into(), None);
        for place in [Placement::Tab, Placement::Right, Placement::Floating, Placement::Tab] {
            server
                .spawn_pane_in(
                    ws,
                    None,
                    PaneSpec { files: Some(["/missing".into(), "/missing".into()]), ..Default::default() },
                    place,
                    Some("kept".into()),
                )
                .unwrap();
        }
        ws
    }

    fn source(server: &mut Server) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kraal-delete-{}", crate::accounts::new_uuid()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("api.kdl");
        std::fs::write(&path, "// keep this source\nlayout { pane command=\"sleep\" { args \"60\"; }; }\n").unwrap();
        server.config.workspace_dirs = vec![dir.display().to_string()];
        path
    }

    #[test]
    fn deleting_open_closed_and_empty_workspaces_restores_the_identity_layout_and_position() {
        for (closed, empty) in [(false, false), (true, false), (false, true)] {
            let (tx, _rx) = super::super::queue::channel(4096);
            let mut server = super::super::tests::server(tx);
            server.new_workspace("before".into(), None);
            let ws = if empty { server.new_workspace("api".into(), None) } else { workspace(&mut server) };
            server.new_workspace("after".into(), None);
            writer(&mut server, vec![]);
            if closed {
                server.keep_and_close_workspace(ws).unwrap();
            }
            let expected = persist::snapshot(&server).workspaces.remove(1);
            let identity = server.keep_and_delete_workspace(ws).unwrap();
            assert_eq!(identity, expected.id);
            assert_eq!(server.workspaces.iter().map(|w| w.name.as_str()).collect::<Vec<_>>(), ["before", "after"]);
            assert!(server.panes.is_empty());
            assert_eq!(server.deleted_workspaces[0].workspace.counts(), expected.counts());
            let restored = server.restore_deleted_workspace(&identity, "api").unwrap();
            assert!(server.deleted_workspaces.is_empty());
            assert_eq!(server.workspaces[1].id, restored);
            assert_eq!(server.workspaces[1].stable_id, identity);
            assert_eq!(server.workspaces[1].counts(), expected.counts());
            assert!(server.workspaces[1].closed());
            assert!(server.panes.is_empty(), "Restore starts no programs");
            server.reopen_workspace(restored).unwrap();
            assert_eq!(server.workspaces[1].counts(), expected.counts());
        }
    }

    #[test]
    fn deleting_the_last_workspace_moves_every_client_to_the_list_and_attach_creates_nothing() {
        let (tx, _rx) = super::super::queue::channel(4096);
        let mut server = super::super::tests::server(tx);
        let (first, _peer) = super::super::tests::client(&mut server);
        let ws = server.clients[&first].workspace;
        let (second, _other) = super::super::tests::client(&mut server);
        server.workspaces.pop();
        server.clients.get_mut(&second).unwrap().workspace = ws;
        writer(&mut server, vec![]);
        server.keep_and_delete_workspace(ws).unwrap();
        for id in [first, second] {
            assert_eq!(server.clients[&id].workspace, 0);
            assert!(matches!(server.clients[&id].mode, Mode::Picker(_)));
        }
        assert!(!server.quitting);
        server.attach(first, 80, 24, None);
        assert!(server.workspaces.is_empty());
        assert!(server.panes.is_empty());
        assert!(matches!(server.clients[&first].mode, Mode::Picker(_)));
    }

    #[test]
    fn a_failed_preparation_preserves_the_original_state_and_a_failed_rollback_is_explicit() {
        for closed in [false, true] {
            for rollback_fails in [false, true] {
                let (tx, _rx) = super::super::queue::channel(4096);
                let mut server = super::super::tests::server(tx);
                let ws = workspace(&mut server);
                writer(&mut server, vec![]);
                if closed {
                    server.keep_and_close_workspace(ws).unwrap();
                }
                let before = serde_json::to_value(persist::snapshot(&server)).unwrap();
                let saved = writer(
                    &mut server,
                    vec![Err("disk full".into()), if rollback_fails { Err("disk full".into()) } else { Ok(()) }],
                );
                let error = server.keep_and_delete_workspace(ws).unwrap_err();
                assert!(error.contains("Nothing was stopped"), "{error}");
                assert_eq!(error.contains("Couldn't confirm cancellation"), rollback_fails);
                assert_eq!(server.panes.len(), if closed { 0 } else { 4 });
                assert_eq!(serde_json::to_value(persist::snapshot(&server)).unwrap(), before);
                assert!(server.deleted_workspaces.is_empty());
                assert_ne!(saved.lock().unwrap().last().unwrap()["workspaces"][0]["delete_pending"], true);
                assert!(server.keep_and_delete_workspace(ws).is_ok());
            }
        }
    }

    #[test]
    fn interrupted_deletion_and_a_failed_final_save_keep_a_recoverable_layout() {
        for failure in [1, 2] {
            let (tx, _rx) = super::super::queue::channel(4096);
            let mut server = super::super::tests::server(tx.clone());
            let ws = workspace(&mut server);
            let mut answers = vec![Ok(()); 3];
            answers[failure] = Err("disk full".into());
            let writes = writer(&mut server, answers);
            let Mode::Confirm(q) = server.delete_workspace(0, DeleteTarget::Workspace(ws)) else { panic!() };
            assert!(q.message.contains("Programs stopped."));
            assert!(!q.message.contains("Nothing was stopped"));
            assert!(server.workspaces[0].delete_pending);
            assert_eq!(server.workspaces[0].status(), "Delete failed");
            assert!(
                matches!(server.ask_close_workspace(0, ws), Mode::Confirm(q) if matches!(q.action, ConfirmAction::RetryDeleteWorkspace(_)))
            );
            assert!(server.reopen_workspace(ws).unwrap_err().to_string().contains("Retry Delete"));
            let prepared = writes.lock().unwrap()[0].clone();
            assert_eq!(prepared["workspaces"][0]["delete_pending"], true);
            assert!(prepared["workspaces"][0]["close_pending"].is_object());
            let mut recovered = super::super::tests::server(tx);
            recovered.apply_snapshot(serde_json::from_value(prepared).unwrap()).unwrap();
            assert!(recovered.panes.is_empty());
            writer(&mut recovered, vec![]);
            let id = recovered.workspaces[0].id;
            recovered.keep_and_delete_workspace(id).unwrap();
            assert!(recovered.workspaces.is_empty());
            assert_eq!(recovered.deleted_workspaces[0].workspace.counts(), (2, 4));
            assert!(server.keep_and_delete_workspace(ws).is_ok());
            assert!(server.workspaces.is_empty());
        }
    }

    #[test]
    fn a_crash_between_save_and_stop_never_relaunches_and_finishes_stopping_the_original_program() {
        use super::super::persistence::Persist;
        let (tx, _rx) = super::super::queue::channel(4096);
        let mut server = super::super::tests::server(tx.clone());
        server.config.save_scrollback = false;
        let ws = server.new_workspace("live".into(), None);
        let pane = server
            .spawn_pane_in(
                ws,
                None,
                PaneSpec { command: vec!["sleep".into(), "60".into()], ..Default::default() },
                Placement::Tab,
                None,
            )
            .unwrap();
        let process = server.panes[&pane].process_hold().unwrap();
        let observed = process.clone();
        let (sender, jobs) = crossbeam_channel::unbounded();
        server.persister = sender;
        let prepared = std::thread::spawn(move || {
            let mut prepared = None;
            while let Ok(job) = jobs.recv() {
                match job {
                    Persist::Save(_, snap) if prepared.is_none() => {
                        assert!(snap.workspaces[0].delete_pending);
                        assert!(snap.workspaces[0].close_pending.is_some());
                        prepared = Some(snap);
                    }
                    Persist::Flush(done) => {
                        assert!(observed.group_running(), "the deletion is on disk, but not acknowledged");
                        let _ = done.send(Err("interrupted before acknowledgment".into()));
                        return prepared.unwrap();
                    }
                    _ => {}
                }
            }
            panic!("no deletion intent")
        });
        assert!(server.keep_and_delete_workspace(ws).is_err());
        assert!(process.group_running());
        let saved = prepared.join().unwrap();
        let mut recovered = super::super::tests::server(tx);
        recovered.apply_snapshot(saved).unwrap();
        assert!(recovered.panes.is_empty());
        writer(&mut recovered, vec![]);
        recovered.keep_and_delete_workspace(recovered.workspaces[0].id).unwrap();
        assert!(!process.group_running());
        assert!(recovered.workspaces.is_empty());
        assert_eq!(recovered.deleted_workspaces[0].workspace.counts(), (1, 1));
        server.close_pane(pane);
    }

    #[test]
    fn deleted_scrollback_survives_saves_and_restart_until_the_user_disables_it() {
        let (tx, _rx) = super::super::queue::channel(4096);
        let mut server = super::super::tests::server(tx.clone());
        let ws = workspace(&mut server);
        writer(&mut server, vec![]);
        server.keep_and_close_workspace(ws).unwrap();
        let saved = server.workspaces[0].kept.as_mut().unwrap();
        let NodeSnap::Pane(pane) = saved.tabs[1].root.as_mut().unwrap() else { panic!() };
        pane.scrollback = Some("1-2-3.ans".into());
        let identity = server.keep_and_delete_workspace(ws).unwrap();
        let snap = persist::snapshot(&server);
        assert!(persist::scrollback_files(&snap).contains("1-2-3.ans"));
        let mut recovered = super::super::tests::server(tx);
        recovered.apply_snapshot(serde_json::from_slice(&serde_json::to_vec(&snap).unwrap()).unwrap()).unwrap();
        assert!(recovered.panes.is_empty());
        writer(&mut recovered, vec![]);
        recovered.restore_deleted_workspace(&identity, "api").unwrap();
        assert!(persist::scrollback_files(&persist::snapshot(&recovered)).contains("1-2-3.ans"));
        recovered.config.save_scrollback = false;
        assert!(persist::scrollback_files(&persist::snapshot(&recovered)).is_empty());
        server.config.save_scrollback = false;
        assert!(persist::scrollback_files(&persist::snapshot(&server)).is_empty());
    }

    #[test]
    fn scanned_sources_stay_removed_after_scans_and_restart_and_restore_the_original_contents() {
        for (imported, missing) in [(false, false), (false, true), (true, false), (true, true)] {
            let (tx, _rx) = super::super::queue::channel(4096);
            let mut server = super::super::tests::server(tx.clone());
            let path = source(&mut server);
            let original = std::fs::read_to_string(&path).unwrap();
            writer(&mut server, vec![]);
            let identity = if imported {
                let ws = workspace(&mut server);
                server.workspaces[0].set_definition(Some(&path));
                server.workspaces[0].definition_source = Some(original.clone());
                server.keep_and_delete_workspace(ws).unwrap()
            } else {
                server.delete_workspace_source(&path).unwrap()
            };
            assert!(server.panes.is_empty());
            assert!(server.definitions().is_empty());
            assert!(server.def_files.is_empty());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
            let mut other = super::super::tests::server(tx.clone());
            other.config.workspace_dirs = server.config.workspace_dirs.clone();
            assert_eq!(other.definitions().len(), 1, "removal belongs to one session");
            let mut recovered = super::super::tests::server(tx);
            recovered.config.workspace_dirs = server.config.workspace_dirs.clone();
            recovered.apply_snapshot(persist::snapshot(&server)).unwrap();
            recovered.scan_definitions();
            assert!(recovered.definitions().is_empty());
            if missing {
                std::fs::remove_file(&path).unwrap();
            } else {
                std::fs::write(&path, "layout { pane; pane; }\n").unwrap();
            }
            writer(&mut recovered, vec![]);
            let ws = recovered.restore_deleted_workspace(&identity, "api").unwrap();
            assert!(recovered.workspaces[0].closed());
            assert!(recovered.panes.is_empty());
            assert_eq!(recovered.workspaces[0].definition_source.as_deref(), Some(original.as_str()));
            assert!(recovered.removed_sources.is_empty());
            if missing {
                assert!(!path.exists(), "Restore never writes the external file");
            } else {
                assert_eq!(std::fs::read_to_string(&path).unwrap(), "layout { pane; pane; }\n");
            }
            // Preparing an explicit Open is enough to verify the retained recipe without running it.
            let _batch = crate::pane::batch();
            recovered.reopen_workspace(ws).unwrap();
            assert_eq!(recovered.workspaces[0].counts(), if imported { (2, 4) } else { (1, 1) });
            std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn deleted_unopened_sources_keep_accounts_through_rename_name_reuse_and_restart() {
        for restart in [false, true] {
            let (tx, _rx) = super::super::queue::channel(4096);
            let mut server = super::super::tests::server(tx.clone());
            let path = source(&mut server);
            let original = "// keep this source\nlayout { pane agent=\"claude:work\"; }\n";
            std::fs::write(&path, original).unwrap();
            let mut accounts = crate::accounts::Accounts::empty();
            let account = accounts
                .add(crate::accounts::Account {
                    id: crate::accounts::new_uuid(),
                    name: "work".into(),
                    home: path.parent().unwrap().join("original").display().to_string(),
                    ..accounts.get("claude", "default").unwrap()
                })
                .unwrap();
            server.accounts = accounts.clone();
            writer(&mut server, vec![]);
            let identity = server.delete_workspace_source(&path).unwrap();
            assert!(server.panes.is_empty());
            let retained = server.deleted_workspaces[0].workspace.pending_layout.as_ref().unwrap();
            let definition = crate::workspace::parse_workspace("api", retained).unwrap();
            assert_eq!(definition.panes()[0].account_id.as_deref(), Some(account.id.as_str()));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
            accounts.rename("claude", "work", "renamed").unwrap();
            let replacement = accounts
                .add(crate::accounts::Account {
                    id: crate::accounts::new_uuid(),
                    home: path.parent().unwrap().join("replacement").display().to_string(),
                    ..account.clone()
                })
                .unwrap();
            assert_ne!(replacement.id, account.id);
            if restart {
                let bytes = serde_json::to_vec(&persist::snapshot(&server)).unwrap();
                let mut recovered = super::super::tests::server(tx);
                recovered.config.workspace_dirs = server.config.workspace_dirs.clone();
                recovered.accounts = accounts.clone();
                recovered.apply_snapshot(serde_json::from_slice(&bytes).unwrap()).unwrap();
                server = recovered;
                writer(&mut server, vec![]);
            }
            server.accounts = accounts.clone();
            let ws = server.restore_deleted_workspace(&identity, "api").unwrap();
            assert!(server.panes.is_empty(), "Restore starts no programs");
            server.refresh_source_status();
            assert_eq!(server.workspaces[server.ws_index(ws).unwrap()].source_status, None);
            assert_eq!(server.workspaces[server.ws_index(ws).unwrap()].definition_source.as_deref(), Some(original));
            // Scans reload the accounts file. Keep this test's account changes in memory.
            server.accounts = accounts;
            let _batch = crate::pane::batch();
            server.reopen_workspace(ws).unwrap();
            let pane = server.panes.values().next().unwrap();
            assert_eq!(pane.spec.account_id.as_deref(), Some(account.id.as_str()));
            assert_eq!(pane.spec.account.as_deref(), Some("claude:renamed"));
            assert!(pane.spec.env.contains(&("CLAUDE_CONFIG_DIR".into(), account.home)));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
            std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
        }
    }

    #[test]
    fn exporting_a_restored_unopened_source_keeps_its_account_recipe_through_restart() {
        let (tx, _rx) = super::super::queue::channel(4096);
        let mut server = super::super::tests::server(tx.clone());
        let path = source(&mut server);
        let original = "layout { pane agent=\"claude:work\" start_suspended=true; }\n";
        std::fs::write(&path, original).unwrap();
        let account = server
            .accounts
            .add(crate::accounts::Account {
                id: crate::accounts::new_uuid(),
                name: "work".into(),
                home: path.parent().unwrap().join("work").display().to_string(),
                ..server.accounts.get("claude", "default").unwrap()
            })
            .unwrap();
        let accounts = server.accounts.clone();
        writer(&mut server, vec![]);
        let identity = server.delete_workspace_source(&path).unwrap();
        let ws = server.restore_deleted_workspace(&identity, "api").unwrap();
        server.accounts = accounts.clone();
        let wi = server.ws_index(ws).unwrap();
        let retained = server.workspaces[wi].kept.as_ref().unwrap().pending_layout.clone().unwrap();
        let save = server.prepare_layout_export(wi, &path).unwrap();
        assert!(save.question.is_some());
        server.write_definition(wi, "api", &save).unwrap();
        assert!(server.panes.is_empty());
        let exported = std::fs::read_to_string(&path).unwrap();
        assert!(exported.contains("agent=\"claude\""));
        assert!(!exported.contains(&account.id));
        assert!(!exported.contains("claude:work"));
        assert_eq!(std::fs::read_to_string(path.with_extension("kdl.bak")).unwrap(), original);
        assert_eq!(server.workspaces[wi].definition_source.as_deref(), Some(exported.as_str()));
        let bytes = serde_json::to_vec(&persist::snapshot(&server)).unwrap();
        let mut recovered = super::super::tests::server(tx);
        recovered.accounts = accounts;
        recovered.apply_snapshot(serde_json::from_slice(&bytes).unwrap()).unwrap();
        assert!(recovered.panes.is_empty());
        let wi = recovered.workspaces.iter().position(|w| w.stable_id == identity).unwrap();
        assert_eq!(recovered.workspaces[wi].kept.as_ref().unwrap().pending_layout.as_deref(), Some(retained.as_str()));
        recovered.refresh_source_status();
        assert_eq!(recovered.workspaces[wi].source_status, None);
        assert_eq!(recovered.workspaces[wi].definition_source.as_deref(), Some(exported.as_str()));
        assert!(!recovered.prepare_layout_export(wi, &path).unwrap().question.unwrap().contains("File changed"));
        recovered.reopen_workspace(recovered.workspaces[wi].id).unwrap();
        let pane = recovered.panes.values().next().unwrap();
        assert!(pane.suspended);
        assert_eq!(pane.spec.account_id.as_deref(), Some(account.id.as_str()));
        assert_eq!(pane.spec.account.as_deref(), Some("claude:work"));
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn restoring_with_a_reused_name_and_source_keeps_the_replacements_ownership() {
        // Saving uses the default workspace directory. Give it its own home so other
        // tests scanning their sources never see these files.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "server::workspace_lifecycle::tests::restore_reused_source_in_its_own_session",
                "--ignored",
                "--nocapture",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    #[ignore = "started by restoring_with_a_reused_name_and_source_keeps_the_replacements_ownership"]
    fn restore_reused_source_in_its_own_session() {
        for imported in [false, true] {
            let (tx, _rx) = super::super::queue::channel(4096);
            let mut server = super::super::tests::server(tx);
            let name = format!("api-{}", crate::accounts::new_uuid());
            let restored_name = format!("{name}-restored");
            let path = crate::paths::workspaces_dir().join(format!("{name}.kdl"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let original =
                "// keep this source\nlayout { pane command=\"sleep\" start_suspended=true { args \"60\"; }; }\n";
            std::fs::write(&path, original).unwrap();
            writer(&mut server, vec![]);
            let identity = if imported {
                let ws = server.open_definition(&path).unwrap();
                server.keep_and_delete_workspace(ws).unwrap()
            } else {
                server.delete_workspace_source(&path).unwrap()
            };
            let replacement = server.new_workspace(name.clone(), None);
            let save = server.prepare_definition_save(0, &name).unwrap();
            assert!(save.question.as_ref().unwrap().contains("Replace file?"));
            server.write_definition(0, &name, &save).unwrap();
            let replaced = std::fs::read_to_string(&path).unwrap();
            assert_ne!(replaced, original);
            let Mode::Picker(result) = server.restore_workspace_result(0, &identity, &restored_name, None) else {
                panic!()
            };
            assert!(result.message.as_ref().unwrap().contains("Source belongs to another workspace"));
            let restored = result.current().unwrap().action.workspace().unwrap();
            let wi = server.ws_index(restored).unwrap();
            assert!(server.workspaces[wi].definition.is_none());
            assert_eq!(server.workspaces[wi].definition_source.as_deref(), Some(original));
            assert_eq!(server.definition_workspace(&path).unwrap(), Some(replacement));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), replaced);
            let ri = server.ws_index(replacement).unwrap();
            // The saved link resolves directory aliases, including macOS's /tmp.
            let save = server.prepare_definition_save(ri, &name).unwrap();
            assert!(save.question.is_some());
            assert_eq!(server.write_definition(ri, &name, &save).unwrap(), path.canonicalize().unwrap());
            server.reopen_workspace(restored).unwrap();
            assert_eq!(server.workspaces[wi].counts(), (1, 1));
            assert_eq!(server.panes[&server.workspaces[wi].panes()[0]].spec.command, ["sleep", "60"]);
            let restored_path = server.save_definition(wi, &restored_name).unwrap();
            assert_ne!(restored_path, path);
            assert_eq!(server.definition_workspace(&restored_path).unwrap(), Some(restored));
            assert_eq!(server.definition_workspace(&path).unwrap(), Some(replacement));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), replaced);
            for file in [&path, &path.with_extension("kdl.bak"), &restored_path] {
                std::fs::remove_file(file).unwrap();
            }
        }
    }

    #[test]
    fn source_delete_failure_keeps_the_file_visible_and_explicit_import_clears_its_removal() {
        let (tx, _rx) = super::super::queue::channel(4096);
        let mut server = super::super::tests::server(tx);
        let path = source(&mut server);
        writer(&mut server, vec![Err("disk full".into()), Ok(())]);
        assert!(server.delete_workspace_source(&path).unwrap_err().contains("Nothing was stopped or deleted"));
        assert!(server.deleted_workspaces.is_empty());
        assert_eq!(server.definitions().len(), 1);
        server.delete_workspace_source(&path).unwrap();
        let _batch = crate::pane::batch();
        server.open_definition(&path).unwrap();
        assert!(server.removed_sources.is_empty());
        assert_eq!(server.deleted_workspaces.len(), 1, "an import does not overwrite the deleted copy");
        assert_eq!(server.definitions().len(), 1);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn a_changed_or_unrestored_source_cannot_discard_a_saved_workspace() {
        let (tx, _rx) = super::super::queue::channel(4096);
        let mut server = super::super::tests::server(tx);
        let path = source(&mut server);
        let original = std::fs::read_to_string(&path).unwrap();
        writer(&mut server, vec![]);
        std::fs::write(&path, "invalid {").unwrap();
        assert!(server.delete_workspace_source(&path).unwrap_err().contains("Fix the source file"));
        assert!(server.removed_sources.is_empty());
        std::fs::write(&path, &original).unwrap();
        let ws = workspace(&mut server);
        server.workspaces[0].set_definition(Some(&path));
        let mut saved = persist::snapshot(&server).workspaces.remove(0);
        saved.restore_error = Some("shell missing".into());
        for pane in server.workspaces[0].panes() {
            server.close_pane(pane);
        }
        server.workspaces.clear();
        server.unrestored.push(saved);
        let error = server.delete_workspace_source(&path).unwrap_err();
        assert!(error.contains("Fix the restore error and restart"));
        assert!(error.contains("shell missing"));
        assert_eq!(server.unrestored.len(), 1);
        assert!(server.deleted_workspaces.is_empty());
        assert!(server.removed_sources.is_empty());
        assert!(server.ws_index(ws).is_none());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn restore_after_restart_proposes_a_free_name_and_a_failed_save_keeps_the_form_and_both_workspaces() {
        let (tx, _rx) = super::super::queue::channel(4096);
        let mut server = super::super::tests::server(tx.clone());
        let ws = workspace(&mut server);
        writer(&mut server, vec![]);
        let identity = server.keep_and_delete_workspace(ws).unwrap();
        let mut recovered = super::super::tests::server(tx);
        recovered.apply_snapshot(persist::snapshot(&server)).unwrap();
        let new = recovered.new_workspace("api".into(), None);
        let new_identity = recovered.workspaces[0].stable_id.clone();
        recovered.new_workspace("api-restored".into(), None);
        let list = recovered.deleted_workspace_picker();
        let Mode::Prompt(mut form) = recovered.restore_workspace_mode(0, &identity, Some(&list)) else { panic!() };
        assert_eq!(form.value, "api-restored-2");
        assert!(form.error.as_ref().unwrap().contains("api is already used"));
        form.cursor = 2;
        writer(&mut recovered, vec![Err("disk full".into()), Ok(())]);
        let Mode::Prompt(form) = recovered.submit_workspace_form(0, form) else { panic!() };
        assert_eq!((form.value.as_str(), form.cursor), ("api-restored-2", 2));
        assert!(form.error.as_ref().unwrap().contains("stays in Deleted workspaces"));
        assert_eq!(recovered.deleted_workspaces.len(), 1);
        assert_eq!(recovered.workspaces.len(), 2);
        let Mode::Picker(result) = recovered.submit_workspace_form(0, form) else { panic!() };
        assert_eq!(result.current().unwrap().label, "api-restored-2");
        assert_eq!(recovered.workspaces[0].stable_id, identity);
        assert_eq!(recovered.workspaces[1].id, new);
        assert_eq!(recovered.workspaces[1].stable_id, new_identity);
        assert!(recovered.panes.is_empty());
        assert!(recovered.restore_deleted_workspace(&identity, "unused").is_err());
    }
}
