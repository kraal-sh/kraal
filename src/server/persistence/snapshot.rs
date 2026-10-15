use crate::layout::{Axis, Node, PaneId};
use crate::persist::{NodeSnap, PaneSnap, SessionSnapshot, TabSnap, VERSION, WorkspaceSnap};
use crate::server::Server;

pub(in crate::server) fn snapshot(server: &Server) -> SessionSnapshot {
    let pane_snap = |id: PaneId| -> PaneSnap {
        if let Some(pane) = server.edited_pane_snapshot(id) {
            return pane;
        }
        let p = &server.panes[&id];
        let mut spec = p.spec.clone();
        server.accounts.bind_pane(&mut spec);
        if !p.suspended {
            spec.startup_input = None;
        }
        if let crate::pane::Content::Files(f) = &p.content {
            use crate::files::Side;
            spec.files = Some([f.location(Side::Left).saved(), f.location(Side::Right).saved()]);
        }
        if spec.preview.is_none()
            && let Some(cwd) = p.current_cwd()
        {
            spec.cwd = Some(cwd);
        }
        // An agent that exited keeps its conversation, not what kind of agent it was: the
        // command it ran tells, so that a restart can still resume it.
        let agent = p.agent.kind.clone().or_else(|| {
            let program = spec.command.first()?.rsplit('/').next()?;
            p.agent.session_id.as_ref()?;
            crate::accounts::agents().find(|k| k.command == program).map(|k| k.name.to_string())
        });
        PaneSnap {
            name: p.name.clone(),
            spec,
            agent,
            agent_session: p.agent.session_id.clone(),
            stopped: p.exit_code.is_some() || p.suspended,
            scrollback: server.scrollback.get(&id).and_then(|s| s.file.clone()),
        }
    };
    fn node(n: &Node, f: &dyn Fn(PaneId) -> PaneSnap) -> NodeSnap {
        match n {
            Node::Leaf(id) => NodeSnap::Pane(Box::new(f(*id))),
            Node::Split { axis, children } => NodeSnap::Split {
                axis: *axis,
                children: children.iter().map(|(c, w)| (node(c, f), *w)).collect(),
                stacked: false,
                open: None,
            },
            Node::Stack { panes, open } => NodeSnap::Split {
                axis: Axis::Column,
                children: panes.iter().map(|p| (NodeSnap::Pane(Box::new(f(*p))), 1.0)).collect(),
                stacked: true,
                open: panes.iter().position(|p| p == open),
            },
        }
    }
    let mut workspaces: Vec<WorkspaceSnap> = server
        .workspaces
        .iter()
        .map(|ws| {
            if let Some(kept) = &ws.kept {
                let mut kept = kept.clone();
                kept.name = ws.name.clone();
                kept.definition = ws.definition.clone();
                // Older snapshots kept the recipe in definition_source.
                if kept.unopened && kept.pending_layout.is_none() {
                    kept.pending_layout = kept.definition_source.clone();
                }
                kept.definition_source = ws.definition_source.clone();
                kept.source_reviewed = ws.source_reviewed.clone();
                kept.layout_undo = ws.layout_undo.clone();
                kept.delete_pending = ws.delete_pending;
                kept.bind_accounts(&server.accounts);
                return kept;
            }
            let mut saved = WorkspaceSnap {
                id: ws.stable_id.clone(),
                name: ws.name.clone(),
                definition: ws.definition.clone(),
                definition_source: ws.definition_source.clone(),
                source_reviewed: ws.source_reviewed.clone(),
                layout_undo: ws.layout_undo.clone(),
                cwd: ws.cwd.clone(),
                active: ws.active,
                // Typing is never broadcast after a restart: it starts again only when asked.
                sync: crate::server::SyncScope::Off,
                restore_error: None,
                pending_layout: None,
                closed: false,
                empty: false,
                close_pending: None,
                delete_pending: ws.delete_pending,
                unopened: false,
                tabs: ws
                    .tabs
                    .iter()
                    .map(|t| {
                        // Login panes and shown paste buffers may hold secrets and must not return.
                        let floating: Vec<&crate::server::Floating> = t
                            .floating
                            .iter()
                            .filter(|f| {
                                !server.panes.get(&f.pane).is_some_and(|p| p.shows_text() || p.spec.login.is_some())
                            })
                            .collect();
                        let mut kept = t.root.as_ref().map(|r| r.leaves()).unwrap_or_default();
                        kept.extend(floating.iter().map(|f| f.pane));
                        TabSnap {
                            name: t.name.clone(),
                            root: t.root.as_ref().map(|r| node(r, &pane_snap)),
                            floating: floating.iter().map(|f| (pane_snap(f.pane), f.rect)).collect(),
                            focus: t.focus.and_then(|f| kept.iter().position(|p| *p == f)),
                            show_floating: t.show_floating && !floating.is_empty(),
                            zoomed: t.zoom.is_some(),
                            sync: false,
                            preset: t.preset,
                        }
                    })
                    .collect(),
            };
            let has_panes = |t: &&TabSnap| t.root.is_some() || !t.floating.is_empty();
            if let Some(undo) = &mut saved.layout_undo {
                undo.bind_accounts(&server.accounts);
            }
            saved.active = saved.tabs.iter().take(ws.active).filter(has_panes).count();
            saved.tabs.retain(|t| t.root.is_some() || !t.floating.is_empty());
            saved.empty = saved.tabs.is_empty();
            saved
        })
        .chain(server.unrestored.iter().cloned())
        .collect();
    let mut deleted_workspaces = server.deleted_workspaces.clone();
    for deleted in &mut deleted_workspaces {
        deleted.workspace.bind_accounts(&server.accounts);
    }
    if !server.config.save_scrollback {
        for ws in workspaces.iter_mut().chain(deleted_workspaces.iter_mut().map(|d| &mut d.workspace)) {
            ws.clear_scrollback();
        }
    }
    SessionSnapshot {
        workspace_order: server.workspace_order.clone(),
        version: VERSION,
        workspaces,
        deleted_workspaces,
        removed_sources: server.removed_sources.clone(),
        tunnels: server.tunnels.iter().map(|t| t.spec.clone()).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pane::{Login, LoginState, PaneSpec, ShownText};
    use crate::protocol::Placement;
    use crate::server::persistence::SavedScrollback;
    use crate::workspace::SyncScope;

    #[test]
    fn capture_omits_login_and_paste_panes_and_turns_broadcasting_off() {
        let (tx, _rx) = crate::server::event_channel(4096);
        let mut server = crate::server::tests::server(tx);
        let ws = server.new_workspace("kept".into(), None);
        let files = PaneSpec { files: Some(["/missing".into(), "/missing".into()]), ..Default::default() };
        server.spawn_pane_in(ws, None, files.clone(), Placement::Tab, None).unwrap();
        let login = server.spawn_pane_in(ws, None, files, Placement::Floating, Some("login".into())).unwrap();
        server.panes.get_mut(&login).unwrap().spec.login = Some(Login {
            account: serde_json::from_value(serde_json::json!({
                "id": "account", "name": "work", "agent": "claude", "home": "/missing"
            }))
            .unwrap(),
            state: LoginState::Running,
            lock: None,
        });
        let paste = server
            .spawn_pane_in(
                ws,
                None,
                PaneSpec {
                    text: Some(ShownText { title: "paste".into(), text: "private text".into() }),
                    ..Default::default()
                },
                Placement::Floating,
                None,
            )
            .unwrap();
        for id in [login, paste] {
            server.scrollback.insert(
                id,
                SavedScrollback { file: Some("private.ans".into()), taken: std::time::Instant::now(), retry: false },
            );
        }
        server.workspaces[0].sync = SyncScope::Workspace;
        let snap = snapshot(&server);
        assert_eq!(snap.workspaces[0].sync, SyncScope::Off);
        let tab = &snap.workspaces[0].tabs[0];
        assert!(!tab.sync);
        assert!(tab.root.is_some());
        assert!(tab.floating.is_empty());
        assert!(!tab.show_floating);
        assert_eq!(tab.focus, None);
        assert!(crate::persist::scrollback_files(&snap).is_empty());
        assert!(!serde_json::to_string(&snap).unwrap().contains("private"));
    }
}
