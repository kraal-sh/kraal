//! Session state on disk, so a server restart (or a reboot) brings the layout back.
//!
//! Processes cannot survive a restart; panes come back as fresh processes in their last working
//! directory. Command panes (e.g. `ssh host`) rerun their command, and agents with a known
//! session id are resumed (`claude --resume <id>`).

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::layout::{Axis, Node, PaneId};
use crate::pane::PaneSpec;
use crate::paths;
use crate::server::{FloatRect, Floating, Server, Tab};

const VERSION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub version: u32,
    pub workspaces: Vec<WorkspaceSnap>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct WorkspaceSnap {
    pub name: String,
    pub definition: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub active: usize,
    pub tabs: Vec<TabSnap>,
    #[serde(default)]
    pub sync: crate::server::SyncScope,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct TabSnap {
    pub name: String,
    pub root: Option<NodeSnap>,
    pub floating: Vec<(PaneSnap, FloatRect)>,
    /// Index of the focused pane in tiled-then-floating order.
    pub focus: Option<usize>,
    pub show_floating: bool,
    pub zoomed: bool,
    pub sync: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum NodeSnap {
    Pane(Box<PaneSnap>),
    Split { axis: Axis, children: Vec<(NodeSnap, f32)> },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaneSnap {
    pub name: Option<String>,
    pub spec: PaneSpec,
    pub agent: Option<String>,
    pub agent_session: Option<String>,
    /// The command had exited (or never started) when the snapshot was taken.
    #[serde(default)]
    pub stopped: bool,
}

pub fn snapshot(server: &Server) -> SessionSnapshot {
    let pane_snap = |id: PaneId| -> PaneSnap {
        let p = &server.panes[&id];
        let mut spec = p.spec.clone();
        spec.startup_input = None;
        if let crate::pane::Content::Files(f) = &p.content {
            use crate::files::Side;
            spec.files = Some([f.location(Side::Left).display(), f.location(Side::Right).display()]);
        }
        if spec.preview.is_none()
            && let Some(cwd) = p.current_cwd()
        {
            spec.cwd = Some(cwd);
        }
        PaneSnap {
            name: p.name.clone(),
            spec,
            agent: p.agent.kind.clone(),
            agent_session: p.agent.session_id.clone(),
            stopped: p.exit_code.is_some(),
        }
    };
    fn node(n: &Node, f: &dyn Fn(PaneId) -> PaneSnap) -> NodeSnap {
        match n {
            Node::Leaf(id) => NodeSnap::Pane(Box::new(f(*id))),
            Node::Split { axis, children } => {
                NodeSnap::Split { axis: *axis, children: children.iter().map(|(c, w)| (node(c, f), *w)).collect() }
            }
        }
    }
    let workspaces = server
        .workspaces
        .iter()
        .map(|ws| WorkspaceSnap {
            name: ws.name.clone(),
            definition: ws.definition.clone(),
            cwd: ws.cwd.clone(),
            active: ws.active,
            // Marks are not kept, so marked sync does not survive a restart.
            sync: if ws.sync == crate::server::SyncScope::Marked { crate::server::SyncScope::Off } else { ws.sync },
            tabs: ws
                .tabs
                .iter()
                .map(|t| TabSnap {
                    name: t.name.clone(),
                    root: t.root.as_ref().map(|r| node(r, &pane_snap)),
                    floating: t.floating.iter().map(|f| (pane_snap(f.pane), f.rect)).collect(),
                    focus: t.focus.and_then(|f| t.panes().iter().position(|p| *p == f)),
                    show_floating: t.show_floating,
                    zoomed: t.zoom.is_some(),
                    sync: false,
                })
                .collect(),
        })
        .collect();
    SessionSnapshot { version: VERSION, workspaces }
}

pub fn save(session: &str, snap: &SessionSnapshot) -> Result<()> {
    let path = paths::session_state_path(session);
    if let Some(dir) = path.parent() {
        paths::ensure_private_dir(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    paths::write_private(&tmp, &serde_json::to_vec_pretty(snap)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn load(session: &str) -> Result<Option<SessionSnapshot>> {
    let path = paths::session_state_path(session);
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let snap: SessionSnapshot = serde_json::from_slice(&data).with_context(|| format!("parsing {}", path.display()))?;
    if snap.version != VERSION {
        anyhow::bail!("unsupported state version {}", snap.version);
    }
    Ok(Some(snap))
}

/// Shell input that resumes an agent conversation.
pub fn resume_command(agent: &str, session: &str) -> Option<String> {
    let safe = session.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if !safe {
        return None;
    }
    match agent {
        "claude" => Some(format!("claude --resume {session}\r")),
        "codex" => Some(format!("codex resume {session}\r")),
        _ => None,
    }
}

pub fn apply(server: &mut Server, snap: SessionSnapshot) -> Result<()> {
    for ws in snap.workspaces {
        let panes_before: std::collections::HashSet<PaneId> = server.panes.keys().copied().collect();
        let workspaces_before: Vec<u32> = server.workspaces.iter().map(|w| w.id).collect();
        let name = ws.name.clone();
        if let Err(e) = apply_workspace(server, ws) {
            server.rollback(&panes_before, &workspaces_before);
            server.warnings.push(format!("could not restore workspace {name}: {e:#}"));
        }
    }
    server.relayout();
    Ok(())
}

fn apply_workspace(server: &mut Server, ws: WorkspaceSnap) -> Result<()> {
    let mut sync = ws.sync;
    {
        let id = server.new_workspace(ws.name, ws.definition);
        let wi = server.ws_index(id).unwrap();
        server.workspaces[wi].cwd = ws.cwd;
        for t in ws.tabs {
            let tab_id = server.alloc_id();
            let mut tab = Tab::new(tab_id, t.name);
            if let Some(root) = &t.root {
                tab.root = Some(build(server, root)?);
            }
            for (p, rect) in &t.floating {
                // Transfer windows are not worth bringing back.
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
            if t.sync {
                // Snapshots from before workspace-wide sync.
                sync = crate::server::SyncScope::Tab;
            }
            server.workspaces[wi].tabs.push(tab);
        }
        if server.workspaces[wi].tabs.is_empty() {
            server.workspaces.remove(wi);
        } else {
            let n = server.workspaces[wi].tabs.len();
            server.workspaces[wi].active = ws.active.min(n - 1);
            server.workspaces[wi].sync = sync;
        }
    }
    Ok(())
}

fn build(server: &mut Server, n: &NodeSnap) -> Result<Node> {
    Ok(match n {
        NodeSnap::Pane(p) => Node::Leaf(spawn(server, p)?),
        NodeSnap::Split { axis, children } => {
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
    let mut spec = p.spec.clone();
    if spec.account.is_some() {
        // Agents started from the agent manager resume on their account.
        spec = server.resume_spec(&spec, p.agent_session.as_deref());
    } else if spec.command.is_empty()
        && let (Some(agent), Some(session)) = (&p.agent, &p.agent_session)
    {
        spec.startup_input = resume_command(agent, session);
    }
    // Reconnecting SSH and resuming agents is safe; any other command (a deploy, a cleanup…) waits
    // for Enter, and so does a command that had already finished.
    let is_ssh = spec.command.first().is_some_and(|c| c.rsplit('/').next() == Some("ssh"));
    let auto = is_ssh || spec.account.is_some();
    let id = if !spec.command.is_empty() && (p.stopped || !auto) {
        let why = if p.stopped { "it was not running before the restart" } else { "restored session" };
        server.spawn_pane_suspended(p.name.clone(), spec, why)?
    } else {
        server.spawn_pane(p.name.clone(), spec)?
    };
    if let (Some(agent), Some(pane)) = (&p.agent, server.panes.get_mut(&id))
        && p.agent_session.is_some()
    {
        pane.agent.session_id = p.agent_session.clone();
        let _ = agent;
    }
    Ok(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resume_commands_are_safe() {
        assert_eq!(resume_command("claude", "abc-123").as_deref(), Some("claude --resume abc-123\r"));
        assert_eq!(resume_command("claude", "x; rm -rf /"), None);
        assert_eq!(resume_command("aider", "abc"), None);
    }
}
