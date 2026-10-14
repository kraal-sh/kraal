//! CLI workspace operations use the same durable mutations as the Workspaces list.

use super::Server;
use super::persistence::ClosePane;
use super::ui::Mode;
use super::workspace_lifecycle::DeleteTarget;
use crate::protocol::{Request, Response, WorkspaceInfo};

impl Server {
    pub(super) fn workspace_request(&mut self, request: Request) -> Result<Response, String> {
        let result = self.answer_workspace(request);
        self.refresh_workspace_pickers();
        self.mark_dirty();
        result
    }

    fn workspace_target(&self, reference: &str) -> Result<DeleteTarget, String> {
        if let Some(w) = self.workspaces.iter().find(|w| w.stable_id == reference) {
            return Ok(DeleteTarget::Workspace(w.id));
        }
        if let Some(wi) = self.named_workspace(reference).map_err(|e| format!("{e:#}"))? {
            return Ok(DeleteTarget::Workspace(self.workspaces[wi].id));
        }
        let picker = self.workspace_picker(0);
        let mut sources = picker.items.iter().filter(|i| i.label == reference).filter_map(|i| i.action.definition());
        let path = sources
            .next()
            .ok_or_else(|| format!("Workspace {reference:?} unavailable. Run `kraal workspaces` to choose one."))?;
        if sources.next().is_some() {
            return Err(format!("Several files are named {reference:?}. Select the file in Workspaces."));
        }
        Ok(DeleteTarget::Source(path.to_path_buf()))
    }

    fn answer_workspace(&mut self, request: Request) -> Result<Response, String> {
        match request {
            Request::ListDeletedWorkspaces => Ok(Response::Workspaces(
                self.deleted_workspaces.iter().rev().map(|d| deleted_info(&d.workspace)).collect(),
            )),
            Request::CreateWorkspace { name, cwd } => {
                crate::workspace::validate_name(&name, false).map_err(|e| e.to_string())?;
                self.add_workspace(name.trim(), Some(cwd))?;
                Ok(Response::Ok)
            }
            Request::RenameWorkspace { workspace, name } => {
                let DeleteTarget::Workspace(ws) = self.workspace_target(&workspace)? else {
                    return Err("This file has not been imported. Open it before renaming its workspace.".into());
                };
                crate::workspace::validate_name(&name, false).map_err(|e| e.to_string())?;
                self.rename_workspace(ws, name.trim())?;
                Ok(Response::Ok)
            }
            Request::RestoreWorkspace { workspace, name } => {
                let mut matches = self.deleted_workspaces.iter().filter(|d| d.workspace.id == workspace);
                let exact = matches.next();
                let mut matches = self.deleted_workspaces.iter().filter(|d| d.workspace.name == workspace);
                let deleted = if let Some(exact) = exact {
                    exact
                } else {
                    let first = matches
                        .next()
                        .ok_or("Workspace unavailable. Run `kraal workspaces --deleted` to choose one.")?;
                    if matches.next().is_some() {
                        return Err("Several deleted workspaces have this name. Run `kraal workspaces --deleted` and use an ID.".into());
                    }
                    first
                };
                let identity = deleted.workspace.id.clone();
                let name = name.unwrap_or_else(|| deleted.workspace.name.clone());
                crate::workspace::validate_name(&name, false)
                    .map_err(|e| e.to_string())
                    .and_then(|()| self.restore_deleted_workspace(&identity, name.trim()))
                    .map_err(|e| format!("{e}\nRetry `kraal workspaces restore {identity} --name NAME`."))?;
                Ok(Response::Ok)
            }
            Request::CloseWorkspace { workspace, expected } => self.stop_workspace_request(&workspace, expected, false),
            Request::DeleteWorkspace { workspace, expected } => self.stop_workspace_request(&workspace, expected, true),
            _ => unreachable!("workspace lifecycle request"),
        }
    }

    fn workspace_stop_revision(&self, ws: u32, delete: bool) -> String {
        let w = &self.workspaces[self.ws_index(ws).unwrap()];
        let panes: Vec<_> = w.panes().iter().filter_map(|p| self.panes.get(p).map(ClosePane::read)).collect();
        serde_json::to_string(&(delete, &w.stable_id, &w.name, panes)).unwrap()
    }

    fn stop_workspace_request(
        &mut self,
        reference: &str,
        expected: Option<String>,
        delete: bool,
    ) -> Result<Response, String> {
        let target = self.workspace_target(reference)?;
        let (workspace, revision, question) = match &target {
            DeleteTarget::Workspace(ws) => {
                let w = &self.workspaces[self.ws_index(*ws).unwrap()];
                if !delete && w.delete_pending {
                    return Err(
                        "This workspace has an unfinished Delete. Retry `kraal workspaces delete` with its name or ID."
                            .into(),
                    );
                }
                let revision = self.workspace_stop_revision(*ws, delete);
                let question = if delete {
                    let Mode::Confirm(q) = self.ask_delete_workspace(target.clone()) else { unreachable!() };
                    Some(q.message)
                } else {
                    self.stops(Some(*ws)).map(|stops| format!("Close {}?\n{stops}\n\nThe layout will be kept.\nUnsaved work in these programs may be lost.", w.name))
                };
                (w.stable_id.clone(), revision, question)
            }
            DeleteTarget::Source(path) => {
                if !delete {
                    return Err("This workspace file is already Closed. No programs were stopped.".into());
                }
                let revision = crate::paths::FileRevision::read(path).map_err(|e| format!("{e:#}"))?;
                #[cfg(test)]
                let revision = tests::source_revision(revision);
                let revision = serde_json::to_string(&(delete, path, revision))
                    .map_err(|e| format!("Couldn't prepare workspace deletion: {e}"))?;
                let Mode::Confirm(q) = self.ask_delete_workspace(target.clone()) else { unreachable!() };
                (reference.to_string(), revision, Some(q.message))
            }
        };
        let Some(expected) = expected else {
            return Ok(Response::WorkspacePreview { workspace, revision, question });
        };
        if expected != revision {
            return Err(
                "The workspace or its programs changed. Nothing was stopped. Run the command again to review them."
                    .into(),
            );
        }
        match target {
            DeleteTarget::Workspace(ws) => {
                let check = |server: &Self| {
                    if server.workspace_stop_revision(ws, delete) == expected {
                        Ok(())
                    } else {
                        Err("The workspace or its programs changed. Nothing was stopped. Run the command again to review them.".into())
                    }
                };
                let result = if delete {
                    self.keep_and_delete_workspace_checked(ws, check).map(|_| ())
                } else {
                    self.keep_and_close_workspace_checked(ws, check)
                };
                if let Err(error) = result {
                    if let Some(wi) = self.ws_index(ws)
                        && (!delete || self.workspaces[wi].delete_pending)
                    {
                        self.workspaces[wi].close_error = Some(error.clone());
                    }
                    return Err(error);
                }
            }
            DeleteTarget::Source(path) => {
                self.delete_workspace_source(&path)?;
            }
        }
        Ok(Response::Ok)
    }
}

fn deleted_info(w: &crate::persist::WorkspaceSnap) -> WorkspaceInfo {
    let (tabs, panes) = w.counts();
    WorkspaceInfo {
        id: Some(w.id.clone()),
        name: w.name.clone(),
        open: false,
        definition: w.definition.clone(),
        tabs,
        panes,
        working: 0,
        blocked: 0,
        done: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::{persistence::tests::writer, queue, tests};

    thread_local! {
        static SOURCE_REVISION_TARGET: std::cell::RefCell<Option<std::path::PathBuf>> = const { std::cell::RefCell::new(None) };
    }

    pub(super) fn source_revision(mut revision: crate::paths::FileRevision) -> crate::paths::FileRevision {
        if let Some(target) = SOURCE_REVISION_TARGET.with_borrow_mut(Option::take) {
            revision.target = target;
        }
        revision
    }

    fn request(server: &mut Server, request: Request) -> Result<Response, String> {
        server.request(0, request).unwrap()
    }

    fn preview(server: &mut Server, workspace: &str, delete: bool) -> (String, String, Option<String>) {
        let request = if delete {
            Request::DeleteWorkspace { workspace: workspace.into(), expected: None }
        } else {
            Request::CloseWorkspace { workspace: workspace.into(), expected: None }
        };
        let Response::WorkspacePreview { workspace, revision, question } = self::request(server, request).unwrap()
        else {
            panic!()
        };
        (workspace, revision, question)
    }

    #[test]
    fn commands_create_validate_and_rename_using_the_durable_form_engine() {
        let (tx, _rx) = queue::channel(32);
        let mut server = tests::server(tx);
        let (client, _peer) = tests::client(&mut server);
        let current = server.clients[&client].workspace;
        let saved = writer(&mut server, vec![]);
        let cwd = std::env::temp_dir();
        for name in ["", "  ", "test", "new\n"] {
            assert!(request(&mut server, Request::CreateWorkspace { name: name.into(), cwd: cwd.clone() }).is_err());
        }
        assert_eq!(server.workspaces.len(), 1);
        request(&mut server, Request::CreateWorkspace { name: "api".into(), cwd: cwd.clone() }).unwrap();
        let id = server.workspaces[1].stable_id.clone();
        assert_eq!(server.workspaces[1].cwd, Some(cwd));
        assert_eq!(server.workspaces[1].panes().len(), 1);
        assert_eq!(server.clients[&client].workspace, current);
        assert_eq!(saved.lock().unwrap().last().unwrap()["workspaces"][1]["name"], "api");
        request(&mut server, Request::RenameWorkspace { workspace: id.clone(), name: "backend".into() }).unwrap();
        assert_eq!(server.workspaces[1].stable_id, id);
        assert_eq!(server.workspaces[1].name, "backend");
        assert_eq!(
            request(&mut server, Request::RenameWorkspace { workspace: id.clone(), name: "\tnew".into() }).unwrap_err(),
            "Choose a name without control characters."
        );
        assert_eq!(server.workspaces[1].name, "backend");
        assert!(request(&mut server, Request::RenameWorkspace { workspace: id, name: "test".into() }).is_err());
    }

    #[test]
    fn close_delete_and_restore_keep_identity_layout_and_recovery_after_restart() {
        let (tx, _rx) = queue::channel(32);
        let mut server = tests::server(tx);
        let ws = server.new_workspace("api".into(), None);
        let saved = writer(&mut server, vec![]);
        let (id, revision, question) = preview(&mut server, "api", false);
        assert!(question.is_none());
        assert!(!server.workspaces[0].closed());
        request(&mut server, Request::CloseWorkspace { workspace: id.clone(), expected: Some(revision) }).unwrap();
        assert!(server.workspaces[0].closed());
        let (_, revision, question) = preview(&mut server, &id, true);
        let question = question.unwrap();
        assert!(question.contains("Project files will be kept"));
        assert!(question.contains("Deleted workspaces"));
        assert_eq!(server.workspaces[0].id, ws);
        request(&mut server, Request::DeleteWorkspace { workspace: id.clone(), expected: Some(revision) }).unwrap();
        assert!(server.workspaces.is_empty());
        let snap = serde_json::from_value(saved.lock().unwrap().last().unwrap().clone()).unwrap();
        let (tx, _rx2) = queue::channel(32);
        let mut restored = tests::server(tx);
        crate::persist::apply(&mut restored, snap).unwrap();
        let _saved = writer(&mut restored, vec![]);
        let Response::Workspaces(deleted) = request(&mut restored, Request::ListDeletedWorkspaces).unwrap() else {
            panic!()
        };
        assert_eq!(deleted[0].id.as_deref(), Some(id.as_str()));
        restored.new_workspace("api".into(), None);
        assert!(
            request(&mut restored, Request::RestoreWorkspace { workspace: id.clone(), name: None })
                .unwrap_err()
                .contains("--name")
        );
        assert!(
            request(&mut restored, Request::RestoreWorkspace { workspace: id.clone(), name: Some("new\n".into()) })
                .unwrap_err()
                .starts_with("Choose a name without control characters.")
        );
        assert_eq!(restored.deleted_workspaces.len(), 1);
        request(&mut restored, Request::RestoreWorkspace { workspace: id.clone(), name: Some("recovered".into()) })
            .unwrap();
        assert!(restored.deleted_workspaces.is_empty());
        let w = restored.workspaces.iter().find(|w| w.stable_id == id).unwrap();
        assert!(w.closed());
        assert_eq!(w.name, "recovered");
        assert!(restored.panes.is_empty());
    }

    #[test]
    fn stale_previews_never_stop_a_renamed_or_replacement_workspace() {
        let (tx, _rx) = queue::channel(32);
        let mut server = tests::server(tx);
        let ws = server.new_workspace("api".into(), None);
        let _saved = writer(&mut server, vec![]);
        let (id, revision, _) = preview(&mut server, "api", true);
        server.rename_workspace(ws, "renamed").unwrap();
        let error =
            request(&mut server, Request::DeleteWorkspace { workspace: id.clone(), expected: Some(revision.clone()) })
                .unwrap_err();
        assert!(error.contains("changed"));
        assert!(!server.workspaces[0].closed());
        server.keep_and_delete_workspace(ws).unwrap();
        server.new_workspace("api".into(), None);
        assert!(request(&mut server, Request::DeleteWorkspace { workspace: id, expected: Some(revision) }).is_err());
        assert!(!server.workspaces[0].closed());
    }

    #[test]
    fn source_delete_checks_the_preview_and_restore_keeps_the_retained_contents() {
        let (tx, _rx) = queue::channel(32);
        let mut server = tests::server(tx);
        let dir = std::env::temp_dir().join(format!("kraal-cli-source-delete-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("recipe.kdl");
        let source = "// Keep me.\nlayout { pane command=\"never-run\"; }\n";
        std::fs::write(&path, source).unwrap();
        server.config.workspace_dirs.push(dir.display().to_string());
        let _saved = writer(&mut server, vec![]);
        assert!(
            request(&mut server, Request::RenameWorkspace { workspace: "recipe".into(), name: "renamed".into() })
                .unwrap_err()
                .contains("not been imported")
        );
        let (_, revision, question) = preview(&mut server, "recipe", true);
        assert!(question.unwrap().contains("Source file"));
        std::fs::write(&path, format!("{source}\n")).unwrap();
        assert!(
            request(&mut server, Request::DeleteWorkspace { workspace: "recipe".into(), expected: Some(revision) })
                .unwrap_err()
                .contains("changed")
        );
        let (_, revision, _) = preview(&mut server, "recipe", true);
        request(&mut server, Request::DeleteWorkspace { workspace: "recipe".into(), expected: Some(revision) })
            .unwrap();
        assert!(server.source_removed(&path));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("{source}\n"));
        std::fs::remove_file(&path).unwrap();
        request(&mut server, Request::RestoreWorkspace { workspace: "recipe".into(), name: None }).unwrap();
        assert!(server.workspaces[0].closed());
        assert!(server.workspaces[0].kept.as_ref().unwrap().pending_layout.is_some());
        assert!(server.panes.is_empty());
        assert!(!path.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn source_delete_preview_rejects_a_non_utf8_symlink_target_without_mutating_it() {
        use std::os::unix::{ffi::OsStrExt, fs::symlink};

        let (tx, _rx) = queue::channel(32);
        let mut server = tests::server(tx);
        let dir = std::env::temp_dir().join(format!("kraal-cli-source-delete-non-utf8-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("source");
        let path = dir.join("api.kdl");
        let source = "layout { pane command=\"never-run\"; }\n";
        std::fs::write(&target, source).unwrap();
        symlink(&target, &path).unwrap();
        server.config.workspace_dirs.push(dir.display().to_string());
        let saved = writer(&mut server, vec![]);
        assert_eq!(crate::paths::FileRevision::read(&path).unwrap().target, target.canonicalize().unwrap());
        // macOS filesystems can reject non-UTF-8 names before the request runs. Supply the
        // resolved target at the serialization boundary, keeping the real file and symlink.
        SOURCE_REVISION_TARGET.with_borrow_mut(|target| {
            *target = Some(dir.join(std::ffi::OsStr::from_bytes(b"source-\xff")));
        });
        let error =
            request(&mut server, Request::DeleteWorkspace { workspace: "api".into(), expected: None }).unwrap_err();
        assert!(error.contains("Couldn't prepare workspace deletion"), "{error}");
        SOURCE_REVISION_TARGET.with_borrow(|target| assert!(target.is_none()));
        assert_eq!(std::fs::read_link(&path).unwrap(), target);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), source);
        assert!(!server.source_removed(&path));
        assert!(server.deleted_workspaces.is_empty());
        assert!(saved.lock().unwrap().is_empty());
        let Response::Workspaces(workspaces) = request(&mut server, Request::ListWorkspaces).unwrap() else { panic!() };
        assert!(workspaces.iter().any(|w| w.name == "api"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn duplicate_deleted_names_require_an_identity_and_new_panes_invalidate_stop_approval() {
        let (tx, _rx) = queue::channel(32);
        let mut server = tests::server(tx);
        let ws = server.new_workspace("api".into(), None);
        let _saved = writer(&mut server, vec![]);
        let (_, revision, _) = preview(&mut server, "api", false);
        server
            .spawn_pane_in(
                ws,
                None,
                crate::pane::PaneSpec { command: vec!["sleep".into(), "60".into()], ..Default::default() },
                crate::protocol::Placement::Tab,
                None,
            )
            .unwrap();
        assert!(
            request(&mut server, Request::CloseWorkspace { workspace: "api".into(), expected: Some(revision) })
                .unwrap_err()
                .contains("changed")
        );
        assert_eq!(server.panes.len(), 1);
        let first = server.keep_and_delete_workspace(ws).unwrap();
        let ws = server.new_workspace("api".into(), None);
        server.keep_and_delete_workspace(ws).unwrap();
        assert!(
            request(&mut server, Request::RestoreWorkspace { workspace: "api".into(), name: None })
                .unwrap_err()
                .contains("use an ID")
        );
        request(&mut server, Request::RestoreWorkspace { workspace: first.clone(), name: None }).unwrap();
        assert_eq!(server.workspaces[0].stable_id, first);
        assert!(server.workspaces[0].closed());
        assert_eq!(server.deleted_workspaces.len(), 1);
    }

    #[test]
    fn cli_errors_keep_the_old_name_and_recoverable_deletion() {
        let (tx, _rx) = queue::channel(32);
        let mut server = tests::server(tx);
        let ws = server.new_workspace("api".into(), None);
        let _saved = writer(&mut server, vec![Err("disk full".into()), Ok(())]);
        let error = request(&mut server, Request::RenameWorkspace { workspace: "api".into(), name: "changed".into() })
            .unwrap_err();
        assert!(error.contains("Still named api"));
        assert_eq!(server.workspaces[0].name, "api");
        let id = server.keep_and_delete_workspace(ws).unwrap();
        let _saved = writer(&mut server, vec![Err("disk full".into()), Ok(())]);
        let error = request(&mut server, Request::RestoreWorkspace { workspace: id.clone(), name: None }).unwrap_err();
        assert!(error.contains("stays in Deleted workspaces"));
        assert_eq!(server.deleted_workspaces[0].workspace.id, id);
        assert!(server.workspaces.is_empty());
    }
}
