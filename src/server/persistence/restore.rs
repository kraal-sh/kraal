use std::time::Instant;

use anyhow::{Context, Result};

use super::SavedScrollback;
use crate::layout::{Node, PaneId};
use crate::paths;
use crate::persist::{
    NodeSnap, PaneSnap, SessionSnapshot, WorkspaceKey, WorkspaceSnap, resume_command, scrollback_name,
};
use crate::server::{Floating, Server, Tab};

pub(in crate::server) fn apply(server: &mut Server, mut snap: SessionSnapshot) -> Result<()> {
    server.workspace_order = snap.workspace_order;
    server.deleted_workspaces = snap.deleted_workspaces;
    server.removed_sources = snap.removed_sources;
    for deleted in &mut server.deleted_workspaces {
        deleted.workspace.bind_accounts(&server.accounts);
    }
    for t in &snap.tunnels {
        let host = if t.remote_host.contains(':') { format!("[{}]", t.remote_host) } else { t.remote_host.clone() };
        let remote = format!("{host}:{}", t.remote_port);
        // A tunnel through a connection kraal knows by no name goes through that same connection.
        let added = match &t.via {
            Some(via) => server.add_tunnel_to(via.clone(), &remote, Some(t.local)),
            None => server.add_tunnel(&t.server, &remote, Some(t.local)),
        };
        if let Err(e) = added {
            server.warnings.push(format!("tunnel {}: {e:#}", t.describe()));
        }
    }
    snap.workspaces
        .sort_by_key(|w| WorkspaceKey::position(&server.workspace_order, Some(&w.id), w.definition.as_deref()));
    for mut ws in snap.workspaces {
        ws.bind_accounts(&server.accounts);
        let panes_before: std::collections::HashSet<PaneId> = server.panes.keys().copied().collect();
        let workspaces_before: Vec<u32> = server.workspaces.iter().map(|w| w.id).collect();
        if let Err(e) = apply_workspace(server, &ws) {
            server.rollback(&panes_before, &workspaces_before);
            ws.restore_error = Some(format!("{e:#}"));
            server.warnings.push(format!(
                "could not restore workspace {}: {e:#}; it stays saved; fix the cause and restart the session",
                ws.name
            ));
            server.unrestored.push(ws);
        }
    }
    server.sort_workspaces();
    server.relayout();
    Ok(())
}

pub(in crate::server) fn apply_workspace(server: &mut Server, ws: &WorkspaceSnap) -> Result<()> {
    let closed = ws.closed || ws.close_pending.is_some() || ws.delete_pending;
    let id = if closed {
        server.new_workspace_record(ws.name.clone(), ws.definition.clone())
    } else {
        server.new_workspace(ws.name.clone(), ws.definition.clone())
    };
    let wi = server.ws_index(id).unwrap();
    server.workspaces[wi].stable_id = ws.id.clone();
    server.workspaces[wi].name = ws.name.clone();
    server.workspaces[wi].cwd = ws.cwd.clone();
    server.workspaces[wi].definition_source = ws.definition_source.clone();
    server.workspaces[wi].source_reviewed = ws.source_reviewed.clone();
    server.workspaces[wi].layout_undo = ws.layout_undo.clone();
    server.workspaces[wi].delete_pending = ws.delete_pending;
    if closed {
        server.workspaces[wi].kept = Some(ws.clone());
    } else {
        restore_workspace_tabs(server, wi, ws)?;
    }
    Ok(())
}

pub(in crate::server) fn restore_workspace_tabs(server: &mut Server, wi: usize, ws: &WorkspaceSnap) -> Result<()> {
    if ws.empty {
        return Ok(());
    }
    for t in &ws.tabs {
        let tab_id = server.alloc_id();
        let mut tab = Tab::new(tab_id, t.name.clone());
        tab.preset = t.preset;
        if let Some(root) = &t.root {
            tab.root = Some(build(server, root)?);
        }
        for (p, rect) in &t.floating {
            if p.spec.close_on_success {
                continue;
            }
            let pane = spawn(server, p)?;
            tab.floating.push(Floating { pane, rect: *rect });
        }
        let panes = tab.panes();
        if panes.is_empty() {
            continue;
        }
        let focus = t.focus.and_then(|i| panes.get(i).copied()).unwrap_or(panes[0]);
        tab.show_floating = t.show_floating && !tab.floating.is_empty();
        tab.set_focus(focus);
        if t.zoomed && !tab.is_floating(focus) {
            tab.zoom = Some(focus);
        }
        server.workspaces[wi].tabs.push(tab);
    }
    anyhow::ensure!(!server.workspaces[wi].tabs.is_empty(), "the saved workspace has no restorable panes");
    server.workspaces[wi].active = ws.active.min(server.workspaces[wi].tabs.len() - 1);
    Ok(())
}

fn build(server: &mut Server, n: &NodeSnap) -> Result<Node> {
    Ok(match n {
        NodeSnap::Pane(p) => Node::Leaf(spawn(server, p)?),
        NodeSnap::Split { children, stacked: true, open, .. } => {
            let mut panes = Vec::new();
            for (c, _) in children {
                panes.extend(build(server, c)?.leaves());
            }
            let open = open.and_then(|i| panes.get(i)).or(panes.first()).copied().context("an empty stack")?;
            let mut node = Node::Stack { panes, open };
            node.normalize();
            node
        }
        NodeSnap::Split { axis, children, .. } => {
            let mut out = Vec::new();
            for (c, w) in children {
                out.push((build(server, c)?, *w));
            }
            let mut node = Node::Split { axis: *axis, children: out };
            node.normalize();
            node
        }
    })
}

fn spawn(server: &mut Server, p: &PaneSnap) -> Result<PaneId> {
    // An agent that was still waiting for its account waits again, and is resolved again.
    if p.spec.pending_agent.is_some() {
        let id = server.spawn_pane_suspended(p.name.clone(), p.spec.clone(), "choosing the account")?;
        restore_scrollback(server, id, p);
        server.resolve_pending_agent(id);
        return Ok(id);
    }
    let mut spec = p.spec.clone();
    if p.agent.as_deref().is_some_and(|agent| crate::agent_registry::find(agent).is_some()) {
        spec.agent_clipboard();
    }
    let mut resumed = false;
    if spec.account.is_some() {
        // Agents started from the agent manager resume on their account; a one-shot run or a
        // prompt on the command line waits for Enter instead of running again by itself.
        (spec, resumed) = server.resume_spec(&spec, p.agent_session.as_deref());
    } else if let (Some(agent), Some(session)) = (&p.agent, &p.agent_session) {
        if spec.command.is_empty() {
            spec.startup_input = resume_command(agent, session);
            spec.agent_clipboard();
        } else if let Some(kind) = crate::accounts::agent_kind(agent)
            && let Some((command, exact)) = crate::accounts::resume_argv(kind, &spec.command, session)
        {
            // An agent run as a plain command (`kraal run -- claude`) resumes too.
            spec.command = command;
            resumed = exact;
        }
    }
    // Reconnecting an SSH shell and resuming agents is safe; any other command (a deploy, a
    // cleanup, a tool run on a server…) waits for Enter, and so does one that had finished.
    let ssh_shell = spec.command.first().is_some_and(|c| c.rsplit('/').next() == Some("ssh"))
        && !crate::servers::ssh_runs_command(&spec.command[1..]);
    let auto = ssh_shell || resumed;
    let id = if p.stopped || (!spec.command.is_empty() || p.spec.startup_input.is_some()) && !auto {
        let why = if p.stopped { "it was not running before the restart" } else { "restored session" };
        server.spawn_pane_suspended(p.name.clone(), spec, why)?
    } else {
        server.spawn_pane(p.name.clone(), spec)?
    };
    restore_scrollback(server, id, p);
    if let (Some(agent), Some(pane)) = (&p.agent, server.panes.get_mut(&id))
        && p.agent_session.is_some()
    {
        pane.agent.session_id = p.agent_session.clone();
        let _ = agent;
    }
    Ok(id)
}

/// Show in pane `id` what the pane it replaces showed, before anything its program writes (that
/// is read later, on the event loop). The file stays the pane's until its text is saved again.
fn restore_scrollback(server: &mut Server, id: PaneId, p: &PaneSnap) {
    let Some(file) = p.scrollback.as_deref().filter(|f| scrollback_name(f)) else { return };
    if !server.config.save_scrollback {
        return;
    }
    let path = paths::scrollback_dir(&server.session).join(file);
    let text = match paths::read_regular(&path, crate::pane::MAX_SAVED_BYTES as u64) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
        Err(e) => {
            server.warnings.push(format!("could not show the scrollback saved in {}: {e}", paths::shorten_home(&path)));
            return;
        }
    };
    let saved_at = std::fs::metadata(&path).and_then(|m| m.modified()).ok().map(crate::text::unix_seconds);
    let Some(pane) = server.panes.get_mut(&id) else { return };
    // What the pane's history holds: older lines would only scroll out of it.
    let lines = server.env.scrollback + usize::from(pane.rows);
    // A file is one pane's: another naming it gets a new one when its text is saved.
    if pane.restore_scrollback(&text, saved_at, lines)
        && !server.scrollback.values().any(|s| s.file.as_deref() == Some(file))
    {
        let saved = SavedScrollback { file: Some(file.to_string()), taken: Instant::now(), retry: false };
        server.scrollback.insert(id, saved);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::TabSnap;
    use crate::server::persistence::snapshot;

    fn saved() -> SessionSnapshot {
        // Both 0.4.0 and 0.4.1 write this v2 shape, including stacks and floating geometry.
        serde_json::from_str(
            r#"{"version":2,"workspaces":[{
                "id":"kept","name":"personal","definition":"/missing/source.kdl",
                "definition_source":"layout { pane; }","cwd":null,"active":0,"sync":"off",
                "tabs":[{"name":"work","root":{"Split":{"axis":"Column","stacked":true,"open":1,
                    "children":[
                        [{"Pane":{"name":"first","spec":{"command":["never-run"]},
                            "agent":null,"agent_session":null,"stopped":true}},1.0],
                        [{"Pane":{"name":"second","spec":{"command":["never-run"]},
                            "agent":null,"agent_session":null,"stopped":true}},1.0]
                    ]}},"floating":[[
                        {"name":"floating","spec":{"command":["never-run"]},
                            "agent":null,"agent_session":null,"stopped":true},
                        {"x":0.125,"y":0.25,"w":0.5,"h":0.5}
                    ]],"focus":2,"show_floating":true,"zoomed":false,"sync":false}]
            }],"workspace_order":[{"Id":"kept"}],"tunnels":[]}"#,
        )
        .unwrap()
    }

    #[test]
    fn v2_layout_and_undo_round_trip_through_storage_and_runtime() {
        let (tx, _rx) = crate::server::event_channel(4096);
        let mut server = crate::server::tests::server(tx);
        server.session = format!("restore-v2-{}", crate::identity::new_uuid());
        let mut snap = saved();
        let mut undo = snap.workspaces[0].clone();
        undo.closed = true;
        undo.tabs[0].floating.clear();
        snap.workspaces[0].layout_undo = Some(Box::new(undo));
        let expected = serde_json::to_vec_pretty(&snap).unwrap();
        crate::persist::save(&server.session, &snap).unwrap();
        let loaded = crate::persist::load(&server.session).unwrap().unwrap();
        apply(&mut server, loaded).unwrap();
        assert!(server.unrestored.is_empty());
        assert_eq!(server.panes.len(), 3);
        assert!(server.panes.values().all(|p| p.suspended));
        let tab = &server.workspaces[0].tabs[0];
        assert_eq!(tab.focus, Some(tab.floating[0].pane));
        let Some(Node::Stack { panes, open }) = &tab.root else { panic!("not a stack") };
        assert_eq!(*open, panes[1]);
        assert_eq!(serde_json::to_vec_pretty(&snapshot(&server)).unwrap(), expected);
        assert_eq!(std::fs::read(paths::session_state_path(&server.session)).unwrap(), expected);
        crate::persist::forget(&server.session).unwrap();
    }

    #[test]
    fn a_partial_restore_removes_new_panes_and_keeps_the_failed_layout() {
        let (tx, _rx) = crate::server::event_channel(4096);
        let mut server = crate::server::tests::server(tx);
        let mut snap = saved();
        let mut failed = snap.workspaces[0].clone();
        failed.id = "failed".into();
        failed.name = "failed".into();
        failed.tabs.push(TabSnap {
            name: "broken".into(),
            root: Some(NodeSnap::Split {
                axis: crate::layout::Axis::Column,
                children: Vec::new(),
                stacked: true,
                open: None,
            }),
            floating: Vec::new(),
            focus: None,
            show_floating: false,
            zoomed: false,
            sync: false,
            preset: None,
        });
        snap.workspace_order.insert(0, WorkspaceKey::Id(failed.id.clone()));
        snap.workspaces.insert(0, failed.clone());
        apply(&mut server, snap).unwrap();
        assert_eq!(server.workspaces.len(), 1);
        assert_eq!(server.workspaces[0].stable_id, "kept");
        assert_eq!(server.panes.len(), 3);
        assert!(server.panes.keys().all(|id| server.workspaces[0].panes().contains(id)));
        failed.restore_error = Some("an empty stack".into());
        assert_eq!(server.unrestored.len(), 1);
        assert_eq!(serde_json::to_value(&server.unrestored[0]).unwrap(), serde_json::to_value(&failed).unwrap());
        assert_eq!(
            server.warnings,
            [
                "could not restore workspace failed: an empty stack; it stays saved; fix the cause and restart the session"
            ]
        );
        assert_eq!(
            serde_json::to_value(&snapshot(&server).workspaces[1]).unwrap(),
            serde_json::to_value(failed).unwrap()
        );
    }

    #[test]
    fn restored_scrollback_has_one_owner_and_obeys_the_save_setting() {
        for enabled in [false, true] {
            let (tx, _rx) = crate::server::event_channel(4096);
            let mut server = crate::server::tests::server(tx);
            server.session = format!("restore-scrollback-{}", crate::identity::new_uuid());
            server.config.save_scrollback = enabled;
            crate::persist::save_scrollback(&server.session, "1-2-3.ans", b"saved output\r\n").unwrap();
            let mut snap = saved();
            let tab = &mut snap.workspaces[0].tabs[0];
            let pane = &mut tab.floating[0].0;
            pane.scrollback = Some("1-2-3.ans".into());
            tab.root = Some(NodeSnap::Pane(Box::new(pane.clone())));
            apply(&mut server, snap).unwrap();
            assert_eq!(server.panes.len(), 2);
            assert_eq!(server.scrollback.len(), usize::from(enabled));
            for pane in server.panes.values() {
                assert_eq!(pane.text(None).contains("kraal: restored from"), enabled);
                assert_eq!(pane.text(None).contains("saved output"), enabled);
            }
            assert_eq!(crate::persist::scrollback_files(&snapshot(&server)).len(), usize::from(enabled));
            crate::persist::forget_scrollback(&server.session).unwrap();
        }
    }

    #[test]
    fn a_failed_tunnel_does_not_prevent_workspace_restoration() {
        let (tx, _rx) = crate::server::event_channel(4096);
        let mut server = crate::server::tests::server(tx);
        let mut snap = saved();
        snap.tunnels.push(crate::servers::TunnelSpec {
            server: "saved-connection".into(),
            via: Some(crate::servers::Server { host: "example.invalid".into(), ..Default::default() }),
            local: 15432,
            remote_host: "invalid host".into(),
            remote_port: 5432,
        });
        apply(&mut server, snap).unwrap();
        assert!(server.tunnels.is_empty());
        assert!(server.unrestored.is_empty());
        assert_eq!(server.panes.len(), 3);
        assert_eq!(
            server.warnings,
            ["tunnel saved-connection:invalid host:5432 → localhost:15432: not a host: \"invalid host\""]
        );
    }

    #[test]
    fn old_agent_panes_remove_displays_even_when_stopped_without_a_conversation() {
        let (tx, _rx) = crate::server::queue::channel(64);
        let mut server = crate::server::tests::server(tx);
        for (command, account, agent, session, startup, removed) in [
            (vec!["codex"], None, Some("codex"), None, None, true),
            (vec!["gemini"], None, Some("gemini"), None, None, true),
            (vec!["claude"], Some("claude:removed"), Some("claude"), None, None, true),
            (vec![], None, Some("claude"), Some("conversation"), None, true),
            (vec![], None, None, None, Some("gemini\r"), true),
            (vec![], None, None, None, None, false),
        ] {
            let json = serde_json::json!({
                "name": null, "spec": { "command": command, "account": account, "startup_input": startup },
                "agent": agent, "agent_session": session, "stopped": true
            });
            let saved: PaneSnap = serde_json::from_value(json).unwrap();
            assert!(saved.spec.env_remove.is_empty());
            let id = spawn(&mut server, &saved).unwrap();
            for key in ["DISPLAY", "WAYLAND_DISPLAY"] {
                assert_eq!(server.panes[&id].spec.env_remove.iter().any(|name| name == key), removed);
            }
        }
    }

    #[test]
    fn old_scripted_agent_snapshots_remove_displays_without_a_session() {
        let (tx, _rx) = crate::server::queue::channel(4096);
        let mut server = crate::server::tests::server(tx);
        for (agent, program) in [("continue", "cn"), ("letta", "letta"), ("vibe", "vibe"), ("shell", "sh")] {
            let saved = PaneSnap {
                name: None,
                spec: crate::pane::PaneSpec {
                    command: vec![program.into()],
                    env: vec![("DISPLAY".into(), ":0".into()), ("WAYLAND_DISPLAY".into(), "wayland-0".into())],
                    ..Default::default()
                },
                agent: (agent != "shell").then(|| agent.into()),
                agent_session: None,
                stopped: true,
                scrollback: None,
            };
            let pane = spawn(&mut server, &saved).unwrap();
            let spec = &server.panes[&pane].spec;
            for key in ["DISPLAY", "WAYLAND_DISPLAY"] {
                assert_eq!(spec.env_remove.iter().filter(|name| *name == key).count(), usize::from(agent != "shell"));
                assert_eq!(spec.env.iter().any(|(name, _)| name == key), agent == "shell");
            }
        }
    }
}
