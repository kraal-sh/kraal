//! File imports are reviewed before programs or saved workspaces change.

use std::path::Path;

use anyhow::{Context, Result};

use super::Server;
use crate::{accounts, paths, workspace};
use workspace::{ImportAccount, ImportPreview};

impl Server {
    pub(super) fn preview_import(&self, path: &Path) -> Result<ImportPreview> {
        if let Some(ws) = self.definition_workspace(path)? {
            let name = &self.workspaces[self.ws_index(ws).unwrap()].name;
            anyhow::bail!("This file is already used by {name}. Open {name} from Workspaces.");
        }
        if let Some(ws) =
            self.unrestored.iter().find(|w| w.definition.as_deref().is_some_and(|p| paths::same_file(p, path)))
        {
            anyhow::bail!("This file is already used by {}. Fix its restore error and restart the session.", ws.name);
        }
        self.preview_source(path)
    }

    pub(super) fn preview_source(&self, path: &Path) -> Result<ImportPreview> {
        let mut preview = ImportPreview::read(path)?;
        for pane in preview.definition.panes() {
            if pane.command.is_some() || !matches!(pane.kind, workspace::PaneKind::Terminal) {
                continue;
            }
            let Some(agent) = &pane.agent else { continue };
            let (kind, name) = agent.split_once(':').map_or((agent.as_str(), None), |(k, n)| (k, Some(n)));
            if accounts::agent_kind(kind).is_none() {
                continue;
            }
            let found = match (&pane.account_id, name) {
                (Some(id), _) => self.accounts.by_id(id).filter(|a| a.agent == kind).is_some(),
                (None, Some(name)) => self.accounts.get(kind, name).is_some(),
                (None, None) => !self.accounts.for_agent(kind).is_empty(),
            };
            if found {
                continue;
            }
            let reference = pane.account_id.as_deref().unwrap_or(agent).to_string();
            if preview.accounts.iter().any(|a| a.reference == reference) {
                continue;
            }
            preview.accounts.push(ImportAccount {
                reference,
                agent: kind.into(),
                choices: self.accounts.for_agent(kind).into_iter().map(|a| (a.id, a.name)).collect(),
            });
        }
        Ok(preview)
    }

    pub(super) fn import_layout(
        &mut self,
        path: &Path,
        name: &str,
        expected: &paths::FileRevision,
        choices: &[(String, String)],
    ) -> Result<u32> {
        let preview = self.preview_import(path)?;
        anyhow::ensure!(preview.revision == *expected, "File changed since preview. Preview it again before adding.");
        anyhow::ensure!(!name.trim().is_empty(), "Type a name.");
        anyhow::ensure!(!name.chars().any(char::is_control), "Choose a name without control characters.");
        let used = self.workspaces.iter().any(|w| w.name == name)
            || self.unrestored.iter().any(|w| w.name == name)
            || self.definitions().iter().any(|(p, def)| {
                !paths::same_file(p, &preview.path)
                    && def.as_ref().map_or_else(|_| p.file_stem().is_some_and(|s| s == name), |d| d.name == name)
            });
        anyhow::ensure!(!used, "This name is already used. Choose another name.");
        for missing in &preview.accounts {
            anyhow::ensure!(
                choices.iter().any(|(reference, id)| reference == &missing.reference
                    && missing.choices.iter().any(|(allowed, _)| allowed == id)),
                "Choose a {} account for {}. Add one in Accounts if none is available, then Retry.",
                missing.agent,
                missing.reference
            );
        }
        let mut def = preview.definition;
        def.name = name.into();
        def.visit_panes(|pane| {
            pane.start_suspended = true;
            if let Some(agent) = &pane.agent {
                let reference = pane.account_id.as_deref().unwrap_or(agent);
                if let Some((_, id)) = choices.iter().find(|(r, _)| r == reference)
                    && let Some(a) = self.accounts.by_id(id)
                    && agent.split(':').next() == Some(a.agent.as_str())
                {
                    pane.agent = Some(format!("{}:{}", a.agent, a.name));
                    pane.account_id = Some(a.id);
                }
            }
        });
        suspend_import(&mut def);
        let panes = self.panes.keys().copied().collect();
        let workspaces = self.workspaces.iter().map(|w| w.id).collect::<Vec<_>>();
        let removed = self.removed_sources.clone();
        let order = self.updated_workspace_order();
        let id = self
            .start_definition(&def, Some(preview.path.clone()))
            .context("Couldn't open the layout. No workspace was added. Fix the error and Retry.")?;
        let wi = self.ws_index(id).unwrap();
        self.workspaces[wi].definition_source = Some(String::from_utf8(expected.bytes.clone().unwrap())?);
        let previous_order = std::mem::replace(&mut self.workspace_order, order);
        self.bind_workspace_order(id);
        self.sort_workspaces();
        self.removed_sources.retain(|p| !paths::same_file(p, &preview.path));
        if let Err(error) = self.save_now() {
            self.rollback(&panes, &workspaces);
            self.workspace_order = previous_order;
            self.workspaces.sort_by_key(|w| workspaces.iter().position(|id| *id == w.id));
            self.removed_sources = removed;
            if let Err(rollback) = self.save_now() {
                self.mark_persist();
                anyhow::bail!(
                    "Couldn't save the workspace or confirm its rollback. The saved session may still contain {name}.\nFix the save error and Retry.\n{error}\n{rollback}"
                );
            }
            anyhow::bail!(
                "Couldn't save the workspace. No workspace was added. Fix the save error and Retry.\n{error}"
            );
        }
        self.refresh_workspace_pickers();
        self.mark_dirty();
        Ok(id)
    }
}

pub(super) fn suspend_import(def: &mut workspace::WorkspaceDef) {
    def.visit_panes(|pane| {
        pane.start_suspended = true;
        // Legacy agent hints were shell input; represent them as a waiting command on import.
        if let Some(agent) = &pane.agent
            && accounts::agent_kind(agent.split(':').next().unwrap()).is_none()
            && pane.command.is_none()
        {
            pane.command = Some("sh".into());
            pane.args = vec!["-c".into(), agent.clone()];
            pane.agent = None;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Request, Response};

    fn setup(name: &str) -> (Server, std::path::PathBuf) {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = super::super::tests::server(tx);
        server.new_workspace("existing".into(), None);
        let dir = std::env::temp_dir().join(format!("kraal-w10-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        (server, std::fs::canonicalize(dir).unwrap())
    }

    #[test]
    fn preview_is_read_only_and_import_waits_then_saves_without_changing_the_source() {
        let (mut server, dir) = setup("import");
        let path = dir.join("recipe.kdl");
        let source = "// handwritten\nlayout { pane command=\"touch\" { args \"never\"; }; pane agent=\"unmanaged-agent\"; mystery; }";
        std::fs::write(&path, source).unwrap();
        let before = crate::persist::snapshot(&server);
        let Response::ImportPreview(preview) = server
            .request(
                0,
                Request::ImportWorkspace { path: path.clone(), name: None, expected: None, accounts: Vec::new() },
            )
            .unwrap()
            .unwrap()
        else {
            panic!()
        };
        assert_eq!(preview.definition.panes().len(), 2);
        assert!(preview.summary().contains("mystery"));
        assert!(server.panes.is_empty());
        assert_eq!(
            serde_json::to_value(crate::persist::snapshot(&server)).unwrap(),
            serde_json::to_value(before).unwrap()
        );
        let saved = super::super::persistence::tests::writer(&mut server, vec![Ok(())]);
        let id = server.import_layout(&path, "personal", &preview.revision, &[]).unwrap();
        let wi = server.ws_index(id).unwrap();
        assert_eq!(server.workspaces[wi].name, "personal");
        assert!(server.panes.values().all(|p| p.suspended));
        assert_eq!(saved.lock().unwrap().last().unwrap()["workspaces"][wi]["definition_source"], source);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        for alias in [dir.join("link.kdl"), dir.join("hard.kdl")] {
            if alias.ends_with("link.kdl") {
                std::os::unix::fs::symlink(&path, &alias).unwrap();
            } else {
                std::fs::hard_link(&path, &alias).unwrap();
            }
            assert!(server.preview_import(&alias).unwrap_err().to_string().contains("already used by personal"));
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn importing_an_ordered_file_keeps_its_slot_after_rollback_retry_and_restart() {
        use crate::persist::{self, WorkspaceKey};

        let (mut server, dir) = setup("import-order");
        let path = dir.join("recipe.kdl");
        std::fs::write(&path, "layout { pane command=\"true\"; }").unwrap();
        server.config.workspace_dirs = vec![dir.display().to_string()];
        let source = WorkspaceKey::Source(path.clone());
        let existing = WorkspaceKey::Id(server.workspaces[0].stable_id.clone());
        server.workspace_order = vec![source, existing.clone()];
        let before = server.workspace_order.clone();
        let saved =
            super::super::persistence::tests::writer(&mut server, vec![Err("disk full".into()), Ok(()), Ok(())]);
        let preview = server.preview_import(&path).unwrap();
        assert!(server.import_layout(&path, "personal", &preview.revision, &[]).is_err());
        assert_eq!(server.workspace_order, before);
        assert_eq!(server.workspaces.len(), 1);
        let rollback: persist::SessionSnapshot =
            serde_json::from_value(saved.lock().unwrap().last().unwrap().clone()).unwrap();
        assert_eq!(rollback.workspace_order, before);
        let id = server.import_layout(&path, "personal", &preview.revision, &[]).unwrap();
        assert_eq!(server.workspaces[0].id, id);
        let order = vec![WorkspaceKey::Id(server.workspaces[0].stable_id.clone()), existing];
        assert_eq!(server.workspace_order, order);
        let snap = serde_json::from_value(saved.lock().unwrap().last().unwrap().clone()).unwrap();
        let (tx, _rx) = super::super::queue::channel(32);
        let mut restored = super::super::tests::server(tx);
        persist::apply(&mut restored, snap).unwrap();
        assert_eq!(restored.workspace_order, order);
        assert_eq!(restored.workspaces[0].name, "personal");
        assert_eq!(restored.workspaces[1].name, "existing");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_account_requires_an_explicit_choice_and_exports_have_no_conversation_or_private_name() {
        let (mut server, dir) = setup("accounts");
        let account: accounts::Account = serde_json::from_value(serde_json::json!({
            "id": "local-id", "name": "local-private", "agent": "claude", "home": dir.join("claude")
        }))
        .unwrap();
        let account = server.accounts.add(account).unwrap();
        let path = dir.join("agent.kdl");
        std::fs::write(&path, "layout { pane agent=\"claude:foreign\" account_id=\"foreign-id\"; }").unwrap();
        let preview = server.preview_import(&path).unwrap();
        assert_eq!(preview.accounts[0].choices, [(account.id.clone(), account.name.clone())]);
        assert!(server.import_layout(&path, "agent", &preview.revision, &[]).is_err());
        super::super::persistence::tests::writer(&mut server, vec![]);
        let id = server
            .import_layout(&path, "agent", &preview.revision, &[("foreign-id".into(), account.id.clone())])
            .unwrap();
        let pane = server.panes.values().next().unwrap();
        assert!(pane.suspended);
        assert_eq!(pane.spec.account_id.as_ref(), Some(&account.id));
        let session = pane.spec.session.clone().unwrap();
        let wi = server.ws_index(id).unwrap();
        let exported = dir.join("share.kdl");
        let save = server.prepare_layout_export(wi, &exported).unwrap();
        server.write_definition(wi, "agent", &save).unwrap();
        let bytes = std::fs::read_to_string(&exported).unwrap();
        assert!(bytes.contains("agent=\"claude\""));
        for absent in [account.id.as_str(), account.name.as_str(), session.as_str(), "--resume", "--session-id"] {
            assert!(!bytes.contains(absent), "{bytes}");
        }
        let legacy = server.prepare_definition_save(wi, "agent").unwrap();
        server.write_definition(wi, "agent", &legacy).unwrap();
        let bytes = std::fs::read_to_string(&path).unwrap();
        assert!(bytes.contains(&account.id) && bytes.contains("claude:local-private"));
        assert!(!bytes.contains(&session));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn import_ignores_agent_hints_for_commands_and_previews_without_managed_accounts() {
        let (mut server, dir) = setup("ignored-agent-hints");
        assert!(server.accounts.for_agent("claude").is_empty());
        let path = dir.join("recipe.kdl");
        let document = dir.join("readme.md");
        std::fs::write(&document, "# Preview").unwrap();
        std::fs::write(
            &path,
            format!(
                "layout {{ pane command=\"claude\" agent=\"claude\"; pane preview={:?} agent=\"claude\"; }}",
                document.to_str().unwrap()
            ),
        )
        .unwrap();
        let preview = server.preview_import(&path).unwrap();
        assert!(preview.accounts.is_empty());
        super::super::persistence::tests::writer(&mut server, vec![]);
        let ws = server.import_layout(&path, "hints", &preview.revision, &[]).unwrap();
        let wi = server.ws_index(ws).unwrap();
        let panes = server.workspaces[wi].panes();
        assert_eq!(panes.len(), 2);
        let command = &server.panes[&panes[0]];
        assert_eq!(command.spec.command, ["claude"]);
        assert!(command.suspended);
        assert!(command.spec.account_id.is_none());
        assert_eq!(server.panes[&panes[1]].spec.preview.as_ref(), Some(&document));
        assert!(server.panes[&panes[1]].spec.account_id.is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn batch_publication_refuses_a_destination_opened_after_its_revision_was_checked() {
        let (mut server, dir) = setup("batch-ownership");
        let path = dir.join("recipe.kdl");
        let source = "layout { pane command=\"true\" start_suspended=true; }";
        std::fs::write(&path, source).unwrap();
        server.config.workspace_dirs.push(dir.display().to_string());
        let Response::Workspaces(before) = server.request(0, Request::ListWorkspaces).unwrap().unwrap() else {
            panic!()
        };
        assert!(before.iter().any(|w| w.definition.as_ref() == Some(&path) && w.id.is_none()));
        let expected = paths::FileRevision::read(&path).unwrap();
        server.open_definition(&path).unwrap();
        assert_eq!(paths::FileRevision::read(&path).unwrap(), expected);
        let error = server
            .request(
                0,
                Request::PublishWorkspaceFile {
                    path: path.clone(),
                    source: "layout { pane command=\"false\"; }".into(),
                    expected,
                },
            )
            .unwrap()
            .unwrap_err();
        assert!(error.contains("belongs to workspace"), "{error}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        assert!(!path.with_extension("kdl.bak").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn export_confirmation_keeps_the_selected_identity_when_display_or_name_changes() {
        for explicit in [false, true] {
            let (mut server, dir) = setup(if explicit { "export-named" } else { "export-default" });
            let (client, _peer) = super::super::tests::client(&mut server);
            server.primary = Some(client);
            let ws = server.clients[&client].workspace;
            let wi = server.ws_index(ws).unwrap();
            server.workspaces[wi].cwd = Some(dir.join("selected"));
            let identity = server.workspaces[wi].stable_id.clone();
            let name = server.workspaces[wi].name.clone();
            let path = dir.join("copy.kdl");
            std::fs::write(&path, "layout { pane; }").unwrap();
            let Response::ExportPreview { identity: selected, revision, question, .. } = server
                .request(
                    client,
                    Request::ExportWorkspace {
                        workspace: explicit.then(|| name.clone()),
                        identity: None,
                        path: path.clone(),
                        expected: None,
                    },
                )
                .unwrap()
                .unwrap()
            else {
                panic!()
            };
            assert_eq!(selected, identity);
            assert!(question.is_some());
            server.workspaces[wi].name = "renamed".into();
            let other = server.new_workspace(name.clone(), None);
            server.clients.get_mut(&client).unwrap().workspace = other;
            super::super::persistence::tests::writer(&mut server, vec![]);
            let request = Request::ExportWorkspace {
                workspace: explicit.then(|| name.clone()),
                identity: Some(selected),
                path: path.clone(),
                expected: Some(revision),
            };
            assert!(matches!(server.request(client, request).unwrap().unwrap(), Response::Path(_)));
            assert_eq!(workspace::load_file(&path).unwrap().cwd, Some(dir.join("selected")));
            // Losing the selected identity must not make its reused name a substitute.
            server.workspaces.remove(wi);
            let expected = paths::FileRevision::read(&path).unwrap();
            let error = server
                .request(
                    client,
                    Request::ExportWorkspace {
                        workspace: explicit.then_some(name),
                        identity: Some(identity),
                        path: path.clone(),
                        expected: Some(expected.clone()),
                    },
                )
                .unwrap()
                .unwrap_err();
            assert!(error.contains("no such workspace"), "{error}");
            assert_eq!(paths::FileRevision::read(&path).unwrap(), expected);
            std::fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn hard_link_exports_stay_out_of_the_workspace_listing_after_restart() {
        let (mut server, dir) = setup("hard-link-export");
        let path = dir.join("source.kdl");
        let copy = dir.join("copy.kdl");
        let source = "layout { pane command=\"true\"; }";
        std::fs::write(&path, source).unwrap();
        std::fs::hard_link(&path, &copy).unwrap();
        server.config.workspace_dirs.push(dir.display().to_string());
        super::super::persistence::tests::writer(&mut server, vec![]);
        let preview = server.preview_import(&path).unwrap();
        let ws = server.import_layout(&path, "kept", &preview.revision, &[]).unwrap();
        let wi = server.ws_index(ws).unwrap();
        let list = |server: &mut Server| {
            let Response::Workspaces(list) = server.request(0, Request::ListWorkspaces).unwrap().unwrap() else {
                panic!()
            };
            list.into_iter().map(|w| (w.id, w.name, w.definition)).collect::<Vec<_>>()
        };
        let before = list(&mut server);
        assert_eq!(before.len(), 2);
        let save = server.prepare_layout_export(wi, &copy).unwrap();
        server.write_definition(wi, "kept", &save).unwrap();
        assert!(!paths::same_file(&path, &copy));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        assert_eq!(server.workspaces[wi].definition.as_ref(), Some(&path));
        assert_eq!(list(&mut server), before);
        let bytes = serde_json::to_vec(&crate::persist::snapshot(&server)).unwrap();
        let (tx, _rx) = super::super::queue::channel(32);
        let mut recovered = super::super::tests::server(tx);
        recovered.config.workspace_dirs = server.config.workspace_dirs.clone();
        recovered.apply_snapshot(serde_json::from_slice(&bytes).unwrap()).unwrap();
        assert_eq!(list(&mut recovered), before);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn stale_preview_name_collisions_and_missing_accounts_add_nothing() {
        let (mut server, dir) = setup("refused");
        let path = dir.join("recipe.kdl");
        std::fs::write(&path, "layout { pane command=\"true\"; }").unwrap();
        let preview = server.preview_import(&path).unwrap();
        std::fs::write(&path, "layout { pane command=\"false\"; }").unwrap();
        assert!(
            server
                .import_layout(&path, "new", &preview.revision, &[])
                .unwrap_err()
                .to_string()
                .contains("File changed")
        );
        let preview = server.preview_import(&path).unwrap();
        for name in ["", "new\nline"] {
            assert!(server.import_layout(&path, name, &preview.revision, &[]).is_err());
        }
        let existing = server.workspaces[0].name.clone();
        assert!(
            server
                .import_layout(&path, &existing, &preview.revision, &[])
                .unwrap_err()
                .to_string()
                .contains("already used")
        );
        std::fs::write(&path, "layout { pane agent=\"claude:private\" account_id=\"foreign-id\"; }").unwrap();
        let preview = server.preview_import(&path).unwrap();
        assert_eq!(preview.accounts.len(), 1);
        assert_eq!(preview.accounts[0].reference, "foreign-id");
        let error = server.import_layout(&path, "new", &preview.revision, &[]).unwrap_err().to_string();
        assert!(error.contains("Choose a claude account"), "{error}");
        assert!(server.panes.is_empty());
        assert_eq!(server.workspaces.len(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_import_save_rolls_back_and_retry_removes_a_source_exclusion() {
        let (mut server, dir) = setup("rollback");
        let path = dir.join("recipe.kdl");
        std::fs::write(&path, "layout { pane command=\"true\"; }").unwrap();
        server.removed_sources.push(path.clone());
        let saved =
            super::super::persistence::tests::writer(&mut server, vec![Err("disk full".into()), Ok(()), Ok(())]);
        let preview = server.preview_import(&path).unwrap();
        let error = server.import_layout(&path, "new", &preview.revision, &[]).unwrap_err().to_string();
        assert!(error.contains("No workspace was added"));
        assert!(server.panes.is_empty());
        assert_eq!(server.removed_sources.as_slice(), std::slice::from_ref(&path));
        assert_eq!(server.workspaces.len(), 1);
        server.import_layout(&path, "new", &preview.revision, &[]).unwrap();
        assert!(server.removed_sources.is_empty());
        assert_eq!(server.workspaces.len(), 2);
        assert_eq!(saved.lock().unwrap().last().unwrap()["workspaces"].as_array().unwrap().len(), 2);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_export_preparation_keeps_the_file_backup_and_listing_and_retry_writes_once() {
        let (mut server, dir) = setup("export-prepare");
        let path = dir.join("manual.kdl");
        let backup = path.with_extension("kdl.bak");
        std::fs::write(&path, "// manual\nlayout { pane; }").unwrap();
        std::fs::write(&backup, "previous backup").unwrap();
        super::super::persistence::tests::writer(&mut server, vec![Err("disk full".into()), Ok(()), Ok(())]);
        let save = server.prepare_layout_export(0, &path).unwrap();
        let error = server.write_definition(0, "existing", &save).unwrap_err().to_string();
        assert!(error.contains("The file is unchanged"));
        assert!(server.removed_sources.is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "// manual\nlayout { pane; }");
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "previous backup");
        server.write_definition(0, "existing", &save).unwrap();
        assert_eq!(std::fs::read_to_string(&backup).unwrap(), "// manual\nlayout { pane; }");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn export_of_a_closed_workspace_keeps_identity_source_and_list_and_checks_revisions() {
        let (mut server, dir) = setup("export");
        let source = dir.join("source.kdl");
        let dest = dir.join("copy.kdl");
        let original = "// keep this\nlayout { pane command=\"true\"; }";
        std::fs::write(&source, original).unwrap();
        server.config.workspace_dirs.push(dir.display().to_string());
        super::super::persistence::tests::writer(&mut server, vec![Ok(()); 4]);
        let preview = server.preview_import(&source).unwrap();
        let id = server.import_layout(&source, "kept", &preview.revision, &[]).unwrap();
        let wi = server.ws_index(id).unwrap();
        let identity = server.workspaces[wi].stable_id.clone();
        let mut snap = crate::persist::snapshot(&server).workspaces.remove(wi);
        snap.closed = true;
        server.workspaces[wi].kept = Some(snap);
        server.workspaces[wi].tabs.clear();
        server.panes.clear();
        let save = server.prepare_layout_export(wi, &dest).unwrap();
        server.write_definition(wi, "kept", &save).unwrap();
        assert!(server.panes.is_empty());
        assert!(server.workspaces[wi].kept.is_some());
        assert_eq!(server.workspaces[wi].stable_id, identity);
        assert_eq!(server.workspaces[wi].definition.as_ref(), Some(&source));
        assert_eq!(workspace::load_file(&dest).unwrap().panes()[0].command.as_deref(), Some("true"));
        assert!(!server.definitions().iter().any(|(p, _)| p == &dest));
        assert_eq!(std::fs::read_to_string(&source).unwrap(), original);
        let save = server.prepare_layout_export(wi, &source).unwrap();
        assert!(save.question.as_ref().unwrap().contains("Comments and unsupported KDL"));
        std::fs::write(&source, "second edit").unwrap();
        assert!(server.write_definition(wi, "kept", &save).is_err());
        let save = server.prepare_layout_export(wi, &source).unwrap();
        server.write_definition(wi, "kept", &save).unwrap();
        assert_eq!(std::fs::read_to_string(source.with_extension("kdl.bak")).unwrap(), "second edit");
        assert_eq!(server.workspaces[wi].definition_source, Some(std::fs::read_to_string(&source).unwrap()));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
