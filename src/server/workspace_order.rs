//! Positions belong to the session, including files that have not been imported yet.

use super::ui::{ConfirmAction, Mode, PickAction, Picker};
use super::{ClientId, Server};
use crate::persist::WorkspaceKey;
use crate::theme::NoticeKind;

impl Server {
    pub(super) fn workspace_key(&self, action: &PickAction) -> Option<WorkspaceKey> {
        if let Some(ws) = action.workspace() {
            return self.ws_index(ws).map(|wi| WorkspaceKey::Id(self.workspaces[wi].stable_id.clone()));
        }
        action.definition().map(|p| WorkspaceKey::Source(p.to_path_buf()))
    }

    pub(super) fn workspace_position(&self, action: &PickAction) -> usize {
        let workspace = action.workspace().and_then(|ws| self.ws_index(ws)).map(|wi| &self.workspaces[wi]);
        WorkspaceKey::position(
            &self.workspace_order,
            workspace.map(|w| w.stable_id.as_str()),
            workspace.and_then(|w| w.definition.as_deref()).or_else(|| action.definition()),
        )
    }

    pub(crate) fn sort_workspaces(&mut self) {
        self.workspaces.sort_by_key(|w| {
            WorkspaceKey::position(&self.workspace_order, Some(&w.stable_id), w.definition.as_deref())
        });
    }

    pub(super) fn bind_workspace_order(&mut self, ws: u32) {
        let Some(wi) = self.ws_index(ws) else { return };
        let w = &self.workspaces[wi];
        let identity = WorkspaceKey::Id(w.stable_id.clone());
        if self.workspace_order.contains(&identity) {
            let before = self.workspace_order.len();
            self.workspace_order
                .retain(|key| !matches!(key, WorkspaceKey::Source(path) if w.definition.as_ref() == Some(path)));
            if self.workspace_order.len() != before {
                self.mark_persist();
            }
            return;
        }
        if let Some(key) = self
            .workspace_order
            .iter_mut()
            .find(|key| matches!(key, WorkspaceKey::Source(path) if w.definition.as_ref() == Some(path)))
        {
            *key = identity;
            self.mark_persist();
        }
    }

    pub(super) fn updated_workspace_order(&self) -> Vec<WorkspaceKey> {
        let mut order = self.workspace_order.clone();
        for item in self.workspace_picker(0).items {
            if let Some(key) = self.workspace_key(&item.action)
                && !order.contains(&key)
            {
                order.push(key);
            }
        }
        order
    }

    pub(super) fn move_picker_workspace(&mut self, id: ClientId, p: Picker, up: bool) -> Mode {
        let rows: Vec<_> = p
            .items
            .iter()
            .enumerate()
            .filter(|(_, i)| i.action.workspace().is_some() || i.action.definition().is_some())
            .collect();
        let Some(at) = rows.iter().position(|(i, _)| Some(i) == p.filtered.get(p.selected)) else {
            return Mode::Picker(p);
        };
        let next = if up { at.checked_sub(1) } else { at.checked_add(1) };
        let Some((_, neighbor)) = next.and_then(|i| rows.get(i)) else { return Mode::Picker(p) };
        let (Some(workspace), Some(neighbor)) =
            (self.workspace_key(&rows[at].1.action), self.workspace_key(&neighbor.action))
        else {
            let current = self.clients.get(&id).map_or(0, |c| c.workspace);
            let mut picker = self.workspace_picker(current);
            picker.restore(p.state());
            return picker.result(NoticeKind::Warning, "Workspace unavailable. Select a workspace to move.");
        };
        let mode = self.reorder_workspace(id, workspace, neighbor, up);
        self.return_to_workspaces(id, mode, p.state())
    }

    pub(super) fn reorder_workspace(
        &mut self,
        id: ClientId,
        workspace: WorkspaceKey,
        neighbor: WorkspaceKey,
        before: bool,
    ) -> Mode {
        self.reorder_workspace_checked(id, workspace, neighbor, before, None)
    }

    pub(super) fn reorder_workspace_checked(
        &mut self,
        id: ClientId,
        workspace: WorkspaceKey,
        neighbor: WorkspaceKey,
        before: bool,
        order: Option<Vec<WorkspaceKey>>,
    ) -> Mode {
        let current = self.clients.get(&id).map_or(0, |c| c.workspace);
        let mut picker = self.workspace_picker(current);
        let keys: Vec<_> = picker.items.iter().filter_map(|i| self.workspace_key(&i.action)).collect();
        picker.select(|i| self.workspace_key(&i.action).as_ref() == Some(&workspace));
        let (Some(at), Some(next)) =
            (keys.iter().position(|k| *k == workspace), keys.iter().position(|k| *k == neighbor))
        else {
            return picker.result(NoticeKind::Warning, "Workspace unavailable. Select a workspace to move.");
        };
        if order.as_ref().is_some_and(|order| *order != keys) {
            return picker.result(NoticeKind::Warning, "Order changed. Drag the workspace again.");
        }
        if at == next || (before && at + 1 == next) || (!before && next + 1 == at) {
            return Mode::Picker(picker);
        }
        if order.is_none() && ((before && next + 1 != at) || (!before && at + 1 != next)) {
            return picker.result(NoticeKind::Warning, "Order changed. Choose Move up or Move down again.");
        }
        let old = self.workspace_order.clone();
        let previous: Vec<_> = self.workspaces.iter().map(|w| w.id).collect();
        // Retain deleted entries' slots so Undo can still put them back among these rows.
        for key in &keys {
            if !self.workspace_order.contains(key) {
                self.workspace_order.push(key.clone());
            }
        }
        let mut moved = keys.clone();
        moved.remove(at);
        let next = moved.iter().position(|k| *k == neighbor).unwrap();
        moved.insert(next + usize::from(!before), workspace.clone());
        let mut moved = moved.into_iter();
        for key in &mut self.workspace_order {
            if keys.contains(key) {
                *key = moved.next().unwrap();
            }
        }
        self.sort_workspaces();
        if let Err(error) = self.save_now() {
            self.workspace_order = old;
            self.workspaces.sort_by_key(|w| previous.iter().position(|id| *id == w.id));
            // A timed-out writer may still finish; the original order must follow that write.
            let message = match self.save_now() {
                Ok(()) => format!("Couldn't save the order. Previous order kept.\n{error}"),
                Err(rollback) => {
                    self.mark_persist();
                    format!(
                        "Couldn't save the order or confirm its rollback. Previous order kept on screen.\nThe saved order may differ. Fix the save error and retry.\n{error}\n{rollback}"
                    )
                }
            };
            return Mode::confirm(message, ConfirmAction::ReorderWorkspace { workspace, neighbor, before, order });
        }
        self.refresh_workspace_pickers();
        self.mark_dirty();
        let mut picker = self.workspace_picker(current);
        picker.select(|i| self.workspace_key(&i.action).as_ref() == Some(&workspace));
        Mode::Picker(picker)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist;
    use crate::server::{persistence, queue, tests, workspace_lifecycle::DeleteTarget};

    fn names(server: &Server) -> Vec<String> {
        server
            .workspace_picker(0)
            .items
            .into_iter()
            .filter(|i| i.action.workspace().is_some() || i.action.definition().is_some())
            .map(|i| i.label)
            .collect()
    }

    #[test]
    fn workspace_files_keep_their_order_through_import_close_delete_restore_and_restart() {
        let (tx, _rx) = queue::channel(32);
        let mut server = tests::server(tx);
        let (client, _peer) = tests::client(&mut server);
        let current = server.clients[&client].workspace;
        let dir = std::env::temp_dir().join(format!("kraal-workspace-order-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = "// Preserve the source.\nlayout { pane command=\"true\" start_suspended=true; }\n";
        for name in ["a", "b"] {
            std::fs::write(dir.join(format!("{name}.kdl")), source).unwrap();
        }
        std::fs::write(dir.join("broken.kdl"), "invalid {").unwrap();
        server.config.workspace_dirs.push(dir.display().to_string());
        let a = WorkspaceKey::Source(crate::workspace::definition_path(&dir.join("a.kdl")));
        let b = WorkspaceKey::Source(crate::workspace::definition_path(&dir.join("b.kdl")));
        let main = server.workspace_key(&PickAction::SwitchWorkspace(current)).unwrap();
        let saved = persistence::tests::writer(&mut server, vec![]);
        assert!(matches!(server.reorder_workspace(client, b.clone(), a.clone(), true), Mode::Picker(_)));
        assert!(matches!(server.reorder_workspace(client, b.clone(), main, true), Mode::Picker(_)));
        assert_eq!(names(&server), ["b", "test", "a", "broken"]);
        assert!(server.panes.is_empty());
        assert_eq!(server.workspaces.len(), 1);
        let session = format!("workspace-order-{}", std::process::id());
        let snap: persist::SessionSnapshot =
            serde_json::from_value(saved.lock().unwrap().last().unwrap().clone()).unwrap();
        persist::save(&session, &snap).unwrap();
        let (tx, _rx2) = queue::channel(32);
        let mut restored = tests::server(tx);
        restored.config.workspace_dirs = server.config.workspace_dirs.clone();
        persist::apply(&mut restored, persist::load(&session).unwrap().unwrap()).unwrap();
        assert_eq!(names(&restored), ["b", "test", "a", "broken"]);
        let _writer = persistence::tests::writer(&mut restored, vec![]);
        let ws = restored.open_definition(&dir.join("b.kdl")).unwrap();
        assert_eq!(names(&restored), ["b", "test", "a", "broken"]);
        let identity = restored.workspaces[0].stable_id.clone();
        assert_eq!(restored.workspace_order[0], WorkspaceKey::Id(identity.clone()));
        assert!(!restored.workspace_order.contains(&b));
        restored.keep_and_close_workspace(ws).unwrap();
        restored.reopen_workspace(ws).unwrap();
        assert_eq!(names(&restored), ["b", "test", "a", "broken"]);
        restored.keep_and_delete_workspace(ws).unwrap();
        assert_eq!(names(&restored), ["test", "a", "broken"]);
        restored.restore_deleted_workspace(&identity, "personal").unwrap();
        assert_eq!(names(&restored), ["personal", "test", "a", "broken"]);
        assert!(restored.workspaces[0].closed());
        assert!(matches!(restored.delete_workspace(0, DeleteTarget::Source(dir.join("a.kdl"))), Mode::Picker(_)));
        let deleted = restored.deleted_workspaces[0].workspace.id.clone();
        restored.restore_deleted_workspace(&deleted, "a").unwrap();
        assert_eq!(names(&restored), ["personal", "test", "a", "broken"]);
        for name in ["a", "b"] {
            let path = dir.join(format!("{name}.kdl"));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
            assert!(!path.with_extension("kdl.bak").exists());
        }
        std::fs::remove_file(crate::paths::session_state_path(&session)).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn undoing_a_new_workspace_after_reordering_files_keeps_the_combined_slot() {
        let (tx, _rx) = queue::channel(32);
        let mut server = tests::server(tx);
        let a = server.new_workspace("a".into(), None);
        let dir = std::env::temp_dir().join(format!("kraal-undo-order-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.kdl");
        std::fs::write(&path, "layout { pane command=\"true\" start_suspended=true; }\n").unwrap();
        server.config.workspace_dirs = vec![dir.display().to_string()];
        let x = WorkspaceKey::Source(crate::workspace::definition_path(&path));
        let a_key = server.workspace_key(&PickAction::SwitchWorkspace(a)).unwrap();
        let saved = persistence::tests::writer(&mut server, vec![]);
        server.reorder_workspace(0, x, a_key, true);
        let b = server.new_workspace("b".into(), None);
        assert_eq!(names(&server), ["x", "a", "b"]);
        let identity = server.keep_and_delete_workspace(b).unwrap();
        assert_eq!(names(&server), ["x", "a"]);
        let snap = saved.lock().unwrap().last().unwrap().clone();
        server.restore_deleted_workspace(&identity, "b").unwrap();
        assert_eq!(names(&server), ["x", "a", "b"]);
        let (tx, _rx2) = queue::channel(32);
        let mut restored = tests::server(tx);
        restored.config.workspace_dirs = server.config.workspace_dirs.clone();
        persist::apply(&mut restored, serde_json::from_value(snap).unwrap()).unwrap();
        let _writer = persistence::tests::writer(&mut restored, vec![]);
        let ws = restored.restore_deleted_workspace(&identity, "b").unwrap();
        assert_eq!(names(&restored), ["x", "a", "b"]);
        assert!(restored.workspaces[restored.ws_index(ws).unwrap()].closed());
        assert!(restored.panes.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn importing_a_file_keeps_its_initial_position_without_a_previous_move() {
        let (tx, _rx) = queue::channel(32);
        let mut server = tests::server(tx);
        server.new_workspace("first".into(), None);
        let dir = std::env::temp_dir().join(format!("kraal-workspace-initial-order-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["a", "b"] {
            std::fs::write(dir.join(format!("{name}.kdl")), "layout { pane command=\"true\" start_suspended=true; }\n")
                .unwrap();
        }
        server.config.workspace_dirs.push(dir.display().to_string());
        assert_eq!(names(&server), ["first", "a", "b"]);
        server.open_definition(&dir.join("b.kdl")).unwrap();
        assert_eq!(names(&server), ["first", "a", "b"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn workspace_order_can_be_saved_when_only_unimported_files_exist() {
        let (tx, _rx) = queue::channel(32);
        let mut server = tests::server(tx);
        server.workspace_order =
            vec![WorkspaceKey::Source("/layouts/b.kdl".into()), WorkspaceKey::Source("/layouts/a.kdl".into())];
        let saved = persistence::tests::writer(&mut server, vec![]);
        server.save_now().unwrap();
        assert_eq!(saved.lock().unwrap().len(), 1);
        assert_eq!(saved.lock().unwrap()[0]["workspaces"], serde_json::json!([]));
        let snap: persist::SessionSnapshot = serde_json::from_value(saved.lock().unwrap()[0].clone()).unwrap();
        assert_eq!(snap.workspace_order, server.workspace_order);
    }

    #[test]
    fn stale_workspace_moves_do_not_move_a_different_neighbor_or_a_reused_name() {
        let (tx, _rx) = queue::channel(32);
        let mut server = tests::server(tx);
        let a = server.new_workspace("a".into(), None);
        let b = server.new_workspace("b".into(), None);
        let c = server.new_workspace("c".into(), None);
        let key = |server: &Server, ws| server.workspace_key(&PickAction::SwitchWorkspace(ws)).unwrap();
        let target = key(&server, c);
        let saved = persistence::tests::writer(&mut server, vec![]);
        let Mode::Picker(p) = server.reorder_workspace(0, target.clone(), key(&server, a), true) else { panic!() };
        assert_eq!(p.message.as_deref(), Some("Order changed. Choose Move up or Move down again."));
        server.workspaces.pop();
        server.new_workspace("c".into(), None);
        let Mode::Picker(p) = server.reorder_workspace(0, target, key(&server, b), true) else { panic!() };
        assert_eq!(p.message.as_deref(), Some("Workspace unavailable. Select a workspace to move."));
        assert!(saved.lock().unwrap().is_empty());
        assert_eq!(names(&server), ["a", "b", "c"]);
    }

    #[test]
    fn restoring_a_workspace_deleted_before_the_first_move_keeps_its_previous_slot() {
        let (tx, _rx) = queue::channel(32);
        let mut server = tests::server(tx);
        let a = server.new_workspace("a".into(), None);
        let b = server.new_workspace("b".into(), None);
        let c = server.new_workspace("c".into(), None);
        let _writer = persistence::tests::writer(&mut server, vec![]);
        let deleted = server.keep_and_delete_workspace(a).unwrap();
        assert!(server.workspace_order.is_empty());
        let key = |server: &Server, ws| server.workspace_key(&PickAction::SwitchWorkspace(ws)).unwrap();
        assert!(matches!(server.reorder_workspace(0, key(&server, c), key(&server, b), true), Mode::Picker(_)));
        server.restore_deleted_workspace(&deleted, "a").unwrap();
        assert_eq!(names(&server), ["a", "c", "b"]);
        assert!(server.workspaces[0].closed());
    }
}
