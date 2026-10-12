//! Session state on disk, so a server restart (or a reboot) brings the layout back.
//!
//! Processes cannot survive a restart (a live restart, see `server::upgrade`, keeps them); panes
//! come back as fresh shells in their last working directory. Interactive SSH panes reconnect
//! and agents with a known session id are resumed (`claude --resume <id>`); other command panes
//! wait for Enter rather than run again by themselves. Typing is never broadcast after a restart.
//!
//! What each pane showed is saved too, unless `save_scrollback` is off: a file per pane in the
//! session's scrollback directory, named by the state and given back to the pane replacing it.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::layout::{Axis, Node, PaneId};
use crate::pane::PaneSpec;
use crate::paths;
use crate::server::{FloatRect, Floating, Server, Tab};

const VERSION: u32 = 1;
const REVISION: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub version: u32,
    /// Changes that older versions read without noticing: 1 = a tab without a name is saved
    /// without one (before, as "tab N").
    #[serde(default)]
    pub revision: u32,
    pub workspaces: Vec<WorkspaceSnap>,
    #[serde(default)]
    pub tunnels: Vec<crate::server::net::TunnelSpec>,
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
    /// The file in the session's scrollback directory holding what the pane showed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scrollback: Option<String>,
    /// The pane's id, in the state a live restart hands over (its programs know it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<PaneId>,
}

/// What is saved of a pane's scrollback.
#[derive(Debug, Clone)]
pub struct SavedScrollback {
    /// Its file in the session's scrollback directory, once there was text to save.
    pub file: Option<String>,
    /// When it was read from the pane: output since is not in it.
    pub taken: Instant,
}

/// The session's state; with `live`, each pane names its id (see [`PaneSnap::id`]).
pub fn snapshot(server: &Server, live: bool) -> SessionSnapshot {
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
        // An agent that exited keeps its conversation, not what kind of agent it was: the
        // command it ran tells, so that a restart can still resume it.
        let agent = p.agent.kind.clone().or_else(|| {
            let program = spec.command.first()?.rsplit('/').next()?;
            p.agent.session_id.as_ref()?;
            crate::accounts::AGENTS.iter().find(|k| k.command == program).map(|k| k.name.to_string())
        });
        PaneSnap {
            name: p.name.clone(),
            spec,
            agent,
            agent_session: p.agent.session_id.clone(),
            stopped: p.exit_code.is_some(),
            scrollback: server.scrollback.get(&id).and_then(|s| s.file.clone()),
            id: live.then_some(id),
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
            // Typing is never broadcast after a restart: it starts again only when asked.
            sync: crate::server::SyncScope::Off,
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
    SessionSnapshot {
        version: VERSION,
        revision: REVISION,
        workspaces,
        tunnels: server.tunnels.iter().map(|t| t.spec.clone()).collect(),
    }
}

pub fn save(session: &str, snap: &SessionSnapshot) -> Result<()> {
    let path = paths::session_state_path(session);
    if let Some(dir) = path.parent() {
        paths::ensure_private_dir(dir)?;
    }
    paths::write_private(&path, &serde_json::to_vec_pretty(snap)?)?;
    Ok(())
}

/// Write a pane's scrollback, as [`crate::pane::Pane::scrollback_ansi`] gives it.
pub fn save_scrollback(session: &str, file: &str, text: &[u8]) -> Result<()> {
    let dir = paths::scrollback_dir(session);
    paths::ensure_private_dir(&dir)?;
    paths::write_private(&dir.join(file), text)?;
    Ok(())
}

/// The scrollback files `snap` names.
fn scrollback_files(snap: &SessionSnapshot) -> HashSet<&str> {
    fn walk<'a>(n: &'a NodeSnap, out: &mut HashSet<&'a str>) {
        match n {
            NodeSnap::Pane(p) => out.extend(p.scrollback.as_deref()),
            NodeSnap::Split { children, .. } => children.iter().for_each(|(c, _)| walk(c, out)),
        }
    }
    let mut out = HashSet::new();
    for tab in snap.workspaces.iter().flat_map(|w| &w.tabs) {
        if let Some(root) = &tab.root {
            walk(root, &mut out);
        }
        out.extend(tab.floating.iter().filter_map(|(p, _)| p.scrollback.as_deref()));
    }
    out
}

/// Delete the scrollback files of `session` that `snap`, just written, does not name: those of
/// panes closed since, and of writes cut short.
pub fn clean_scrollback(session: &str, snap: &SessionSnapshot) {
    let dir = paths::scrollback_dir(session);
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    let keep = scrollback_files(snap);
    for e in entries.flatten() {
        if !keep.contains(e.file_name().to_string_lossy().as_ref()) {
            let _ = std::fs::remove_file(e.path());
        }
    }
    // Gone once nothing is left in it.
    let _ = std::fs::remove_dir(&dir);
}

/// Delete all that is saved of the panes' scrollback of `session`. Whether there was any.
pub fn forget_scrollback(session: &str) -> std::io::Result<bool> {
    match std::fs::remove_dir_all(paths::scrollback_dir(session)) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Whether the state may name `file` as a pane's scrollback: a plain name in its directory.
fn scrollback_name(file: &str) -> bool {
    !file.is_empty()
        && !file.starts_with('.')
        && file.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

pub fn load(session: &str) -> Result<Option<SessionSnapshot>> {
    let path = paths::session_state_path(session);
    let data = match std::fs::read(&path) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    // As deep as the layout is: each split nests a few levels, more than serde_json's default
    // limit allows past a few dozen splits.
    let mut de = serde_json::Deserializer::from_slice(&data);
    de.disable_recursion_limit();
    let snap = SessionSnapshot::deserialize(&mut de)
        .and_then(|snap| de.end().map(|()| snap))
        .with_context(|| format!("parsing {}", path.display()))?;
    if snap.version != VERSION {
        anyhow::bail!("unsupported state version {}", snap.version);
    }
    Ok(Some(snap))
}

/// What a restart stops in a pane, if anything, given what runs in it other than a shell at its
/// prompt (`program`, its command line). A shell at its prompt comes back in its directory, an
/// SSH shell reconnects and an agent resumes its conversation (a turn in progress stops); any
/// other program stops, and a pane's own command waits for Enter after the restart.
pub fn stops_on_restart(pane: &crate::protocol::PaneInfo, program: Option<&[String]>) -> Option<String> {
    if pane.kind != "terminal" || pane.suspended || pane.exit_code.is_some() {
        return None;
    }
    if let Some(agent) = &pane.agent {
        return (pane.state == crate::agent::AgentState::Working)
            .then(|| format!("{agent}: the turn in progress (the conversation resumes)"));
    }
    let ssh_shell = pane.command.first().is_some_and(|c| c.rsplit('/').next() == Some("ssh"))
        && !crate::servers::ssh_runs_command(&pane.command[1..]);
    if ssh_shell {
        return None;
    }
    let argv = program.filter(|p| !p.is_empty()).or((!pane.command.is_empty()).then_some(pane.command.as_slice()))?;
    let line = crate::shell::join(argv);
    Some(if line.chars().count() > 60 { format!("{}…", line.chars().take(59).collect::<String>()) } else { line })
}

/// A line of the list of what a restart stops: the pane, then `what`.
pub fn stopping_line(pane: &crate::protocol::PaneInfo, what: &str) -> String {
    match &pane.name {
        Some(name) => format!("{what}  (pane {} \"{name}\" in {})", pane.id, pane.workspace),
        None => format!("{what}  (pane {} in {})", pane.id, pane.workspace),
    }
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

/// The way a pane of the state is made again.
pub type MakePane<'a> = dyn FnMut(&mut Server, &PaneSnap) -> Result<PaneId> + 'a;

pub fn apply(server: &mut Server, snap: SessionSnapshot) -> Result<()> {
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
    apply_layout(server, snap.workspaces, snap.revision, false, &mut spawn);
    Ok(())
}

/// Make the workspaces of a state again, each pane with `make`. After a restart that is not
/// live, a workspace that cannot be made whole is not made at all. After a live one (`live`),
/// every pane that can be made is, in its place, and the others are left out (`make` keeps
/// their programs running, and says so): no pane already made is stopped for another one, and
/// the focus and the tab shown stay on the panes they were on.
pub fn apply_layout(
    server: &mut Server,
    workspaces: Vec<WorkspaceSnap>,
    revision: u32,
    live: bool,
    make: &mut MakePane,
) {
    for ws in workspaces {
        let panes_before: std::collections::HashSet<PaneId> = server.panes.keys().copied().collect();
        let workspaces_before: Vec<u32> = server.workspaces.iter().map(|w| w.id).collect();
        let name = ws.name.clone();
        if let Err(e) = apply_workspace(server, ws, revision, live, make) {
            server.rollback(&panes_before, &workspaces_before);
            server.warnings.push(format!("could not restore workspace {name}: {e:#}"));
        }
    }
    server.relayout();
}

fn apply_workspace(
    server: &mut Server,
    ws: WorkspaceSnap,
    revision: u32,
    live: bool,
    make: &mut MakePane,
) -> Result<()> {
    {
        let id = server.new_workspace(ws.name, ws.definition);
        let wi = server.ws_index(id).unwrap();
        server.workspaces[wi].cwd = ws.cwd;
        let mut active = None;
        for (index, t) in ws.tabs.into_iter().enumerate() {
            let tab_id = server.alloc_id();
            // Older versions saved a tab without a name as "tab N"; unnamed, it shows what its
            // pane runs.
            let old_default = revision == 0 && t.name.strip_prefix("tab ").is_some_and(|n| n.parse::<u32>().is_ok());
            let mut tab = Tab::new(tab_id, if old_default { String::new() } else { t.name.clone() });
            if let Some(root) = &t.root {
                tab.root = build(server, root, live, make)?;
            }
            for (p, rect) in &t.floating {
                // Transfer windows are not worth bringing back (a live restart keeps them as
                // they are, until they close by themselves).
                if p.spec.close_on_success && !live {
                    continue;
                }
                let pane = match make(server, p) {
                    Ok(pane) => pane,
                    Err(_) if live => continue,
                    Err(e) => return Err(e),
                };
                tab.floating.push(Floating { pane, rect: *rect });
            }
            let panes = tab.panes();
            if panes.is_empty() {
                continue;
            }
            let focus = match live {
                // The pane it was, wherever it is now that others may be left out.
                true => kept_focus(&t, &panes),
                false => t.focus.and_then(|i| panes.get(i).copied()),
            }
            .unwrap_or(panes[0]);
            tab.show_floating = t.show_floating && !tab.floating.is_empty();
            tab.set_focus(focus);
            if t.zoomed && !tab.is_floating(focus) {
                tab.zoom = Some(focus);
            }
            if index == ws.active {
                active = Some(server.workspaces[wi].tabs.len());
            }
            server.workspaces[wi].tabs.push(tab);
        }
        if server.workspaces[wi].tabs.is_empty() {
            server.workspaces.remove(wi);
        } else {
            let n = server.workspaces[wi].tabs.len();
            server.workspaces[wi].active = active.unwrap_or(ws.active).min(n - 1);
        }
    }
    Ok(())
}

/// The pane tab `t` had the focus on, among the panes made of it (`made`), when it was made:
/// the state names each pane by its id after a live restart.
fn kept_focus(t: &TabSnap, made: &[PaneId]) -> Option<PaneId> {
    fn walk(n: &NodeSnap, out: &mut Vec<Option<PaneId>>) {
        match n {
            NodeSnap::Pane(p) => out.push(p.id),
            NodeSnap::Split { children, .. } => children.iter().for_each(|(c, _)| walk(c, out)),
        }
    }
    let mut ids = Vec::new();
    if let Some(root) = &t.root {
        walk(root, &mut ids);
    }
    ids.extend(t.floating.iter().map(|(p, _)| p.id));
    t.focus.and_then(|i| ids.get(i).copied().flatten()).filter(|id| made.contains(id))
}

/// The layout `n` describes, each pane made with `make`. A pane that cannot be made fails the
/// whole, or, `live`, is left out.
fn build<S>(
    server: &mut S,
    n: &NodeSnap,
    live: bool,
    make: &mut dyn FnMut(&mut S, &PaneSnap) -> Result<PaneId>,
) -> Result<Option<Node>> {
    Ok(match n {
        NodeSnap::Pane(p) => match make(server, p) {
            Ok(id) => Some(Node::Leaf(id)),
            Err(_) if live => None,
            Err(e) => return Err(e),
        },
        NodeSnap::Split { axis, children } => {
            let mut out = Vec::new();
            for (c, w) in children {
                if let Some(node) = build(server, c, live, make)? {
                    out.push((node, *w));
                }
            }
            if out.is_empty() {
                return Ok(None);
            }
            let mut node = Node::Split { axis: *axis, children: out };
            node.normalize();
            Some(node)
        }
    })
}

/// A pane of the state, its command started again (or waiting for Enter).
pub fn spawn(server: &mut Server, p: &PaneSnap) -> Result<PaneId> {
    // An agent that was still waiting for its account waits again, and is resolved again.
    if p.spec.pending_agent.is_some() {
        let id = server.spawn_pane_suspended(p.name.clone(), p.spec.clone(), "choosing the account")?;
        restore_scrollback(server, id, p);
        server.resolve_pending_agent(id);
        return Ok(id);
    }
    let mut spec = p.spec.clone();
    let mut resumed = false;
    if spec.account.is_some() {
        // Agents started from the agent manager resume on their account; a one-shot run or a
        // prompt on the command line waits for Enter instead of running again by itself.
        (spec, resumed) = server.resume_spec(&spec, p.agent_session.as_deref());
    } else if let (Some(agent), Some(session)) = (&p.agent, &p.agent_session) {
        if spec.command.is_empty() {
            spec.startup_input = resume_command(agent, session);
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
    let id = if !spec.command.is_empty() && (p.stopped || !auto) {
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
        server.scrollback.insert(id, SavedScrollback { file: Some(file.to_string()), taken: Instant::now() });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// After a live restart, a pane that cannot be made is left out of the layout alone; after
    /// a restart that is not live, it fails the whole.
    #[test]
    fn a_pane_that_cannot_be_made_live_is_left_out_alone() {
        let pane = |name: &str| {
            let snap = serde_json::json!({"name": name, "spec": {}, "agent": null, "agent_session": null});
            NodeSnap::Pane(Box::new(serde_json::from_value(snap).unwrap()))
        };
        let inner = NodeSnap::Split { axis: Axis::Column, children: vec![(pane("bad"), 0.5), (pane("c"), 0.5)] };
        let tree = NodeSnap::Split { axis: Axis::Row, children: vec![(pane("a"), 0.5), (inner, 0.5)] };
        let mut make = |made: &mut Vec<String>, p: &PaneSnap| -> Result<PaneId> {
            let name = p.name.clone().unwrap();
            if name == "bad" {
                anyhow::bail!("pane bad: no descriptor to spare");
            }
            made.push(name);
            Ok(made.len() as PaneId)
        };
        let mut made = Vec::new();
        let node = build(&mut made, &tree, true, &mut make).unwrap().unwrap();
        assert_eq!(made, ["a", "c"]);
        assert_eq!(node.leaves(), [1, 2]);
        let mut made = Vec::new();
        assert!(build(&mut made, &tree, false, &mut make).is_err());
    }

    /// After a live restart the focus stays on the pane it was on, even when one before it in
    /// its tab was left out; none when that pane itself was.
    #[test]
    fn the_focus_stays_on_its_pane_when_another_is_left_out() {
        let pane = |id: PaneId| {
            let snap = serde_json::json!({"name": null, "spec": {}, "agent": null, "agent_session": null, "id": id});
            NodeSnap::Pane(Box::new(serde_json::from_value(snap).unwrap()))
        };
        let root = NodeSnap::Split { axis: Axis::Row, children: vec![(pane(1), 0.3), (pane(2), 0.3), (pane(3), 0.3)] };
        let tab = TabSnap {
            name: String::new(),
            root: Some(root),
            floating: Vec::new(),
            focus: Some(1),
            show_floating: false,
            zoomed: false,
            sync: false,
        };
        assert_eq!(kept_focus(&tab, &[2, 3]), Some(2), "pane 1 left out");
        assert_eq!(kept_focus(&tab, &[1, 2, 3]), Some(2));
        assert_eq!(kept_focus(&tab, &[1, 3]), None, "the focused pane left out");
    }

    #[test]
    fn a_restart_stops_programs_not_shells_ssh_shells_or_idle_agents() {
        let pane = |fields: serde_json::Value| -> crate::protocol::PaneInfo {
            let mut info = serde_json::json!({"id": 3, "name": "build", "title": "sh", "workspace": "w", "tab": "t",
                "kind": "terminal", "command": [], "cwd": null, "agent": null, "state": "unknown",
                "exit_code": null, "focused": false, "floating": false, "cols": 80, "rows": 24});
            for (k, v) in fields.as_object().unwrap() {
                info[k] = v.clone();
            }
            serde_json::from_value(info).unwrap()
        };
        let argv = |words: &[&str]| words.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        let shell = pane(serde_json::json!({}));
        assert_eq!(stops_on_restart(&shell, None), None, "a shell at its prompt");
        let sleep = argv(&["sleep", "1000"]);
        assert_eq!(stops_on_restart(&shell, Some(&sleep)).as_deref(), Some("sleep 1000"));
        assert_eq!(stopping_line(&shell, "sleep 1000"), "sleep 1000  (pane 3 \"build\" in w)");
        // A pane's own command stops (it waits for Enter after the restart), unless it is an
        // SSH shell, which reconnects; and nothing runs in a pane whose program ended.
        let command = pane(serde_json::json!({"command": ["make", "watch"]}));
        assert_eq!(stops_on_restart(&command, None).as_deref(), Some("make watch"));
        let ssh = pane(serde_json::json!({"command": ["ssh", "web-1"]}));
        assert_eq!(stops_on_restart(&ssh, Some(&argv(&["ssh", "web-1"]))), None);
        let ssh_job = pane(serde_json::json!({"command": ["ssh", "web-1", "journalctl", "-f"]}));
        assert!(stops_on_restart(&ssh_job, None).is_some());
        let ended = pane(serde_json::json!({"command": ["make"], "exit_code": 0}));
        assert_eq!(stops_on_restart(&ended, None), None);
        let waiting = pane(serde_json::json!({"command": ["make"], "suspended": true}));
        assert_eq!(stops_on_restart(&waiting, None), None);
        let preview = pane(serde_json::json!({"kind": "preview:markdown"}));
        assert_eq!(stops_on_restart(&preview, None), None);
        // Agents resume; one in the middle of a turn loses that turn.
        let idle = pane(serde_json::json!({"agent": "claude", "state": "idle"}));
        assert_eq!(stops_on_restart(&idle, Some(&argv(&["claude"]))), None);
        let working = pane(serde_json::json!({"agent": "claude", "state": "working"}));
        assert!(stops_on_restart(&working, Some(&argv(&["claude"]))).is_some_and(|w| w.starts_with("claude: ")));
        let long = argv(&[
            "cargo",
            "build",
            "--release",
            "--workspace",
            "--all-features",
            "--target",
            "x86_64-unknown-linux-musl",
        ]);
        assert!(stops_on_restart(&shell, Some(&long)).is_some_and(|w| w.chars().count() == 60 && w.ends_with('…')));
    }

    #[test]
    fn resume_commands_are_safe() {
        assert_eq!(resume_command("claude", "abc-123").as_deref(), Some("claude --resume abc-123\r"));
        assert_eq!(resume_command("claude", "x; rm -rf /"), None);
        assert_eq!(resume_command("aider", "abc"), None);
    }

    #[test]
    fn panes_name_their_scrollback_and_older_states_still_load() {
        let old = r#"{"version":1,"workspaces":[{"name":"w","definition":null,"cwd":null,"active":0,"tabs":[
            {"name":"","root":{"Pane":{"name":null,"spec":{},"agent":null,"agent_session":null}},
             "floating":[],"focus":0,"show_floating":false,"zoomed":false,"sync":false}]}]}"#;
        let mut snap: SessionSnapshot = serde_json::from_str(old).unwrap();
        assert!(scrollback_files(&snap).is_empty());
        let Some(NodeSnap::Pane(p)) = &mut snap.workspaces[0].tabs[0].root else { panic!() };
        p.scrollback = Some("1-2-3.ans".into());
        let json = serde_json::to_string(&snap).unwrap();
        let again: SessionSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(scrollback_files(&again), HashSet::from(["1-2-3.ans"]));
        // Only a plain name in the session's directory is read.
        assert!(scrollback_name("1-2-3.ans"));
        for bad in ["", "../x.ans", "a/b", ".hidden", "/etc/passwd"] {
            assert!(!scrollback_name(bad), "{bad}");
        }
    }
}
