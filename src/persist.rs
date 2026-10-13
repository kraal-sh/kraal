//! Session state on disk, so a server restart (or a reboot) brings the layout back.
//!
//! Processes cannot survive a restart; panes come back as fresh shells in their last working
//! directory. Interactive SSH panes reconnect and agents with a known session id are resumed
//! (`claude --resume <id>`); other command panes wait for Enter rather than run again by
//! themselves. Typing is never broadcast after a restart. A workspace that cannot be restored
//! (its shell gone, say) stays saved as it was, to come back at a later start.
//!
//! What each pane showed is saved too, unless `save_scrollback` is off: a file per pane in the
//! session's scrollback directory, named by the state and given back to the pane replacing it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::layout::{Axis, Node, PaneId, Preset};
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSnap {
    pub name: String,
    pub definition: Option<PathBuf>,
    pub cwd: Option<PathBuf>,
    pub active: usize,
    pub tabs: Vec<TabSnap>,
    #[serde(default)]
    pub sync: crate::server::SyncScope,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabSnap {
    pub name: String,
    pub root: Option<NodeSnap>,
    pub floating: Vec<(PaneSnap, FloatRect)>,
    /// Index of the focused pane in tiled-then-floating order.
    pub focus: Option<usize>,
    pub show_floating: bool,
    pub zoomed: bool,
    pub sync: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<Preset>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum NodeSnap {
    Pane(Box<PaneSnap>),
    /// A stack is a split of its panes marked `stacked`, with the index of the one open.
    Split {
        axis: Axis,
        children: Vec<(NodeSnap, f32)>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        stacked: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        open: Option<usize>,
    },
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
}

/// What is saved of a pane's scrollback.
#[derive(Debug, Clone)]
pub struct SavedScrollback {
    /// Its file in the session's scrollback directory, once there was text to save.
    pub file: Option<String>,
    /// When it was read from the pane: output since is not in it.
    pub taken: Instant,
    /// Writing it failed: it is saved again, whether the pane printed since or not.
    pub retry: bool,
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
        })
        .chain(server.unrestored.iter().cloned())
        .collect();
    SessionSnapshot {
        version: VERSION,
        revision: REVISION,
        workspaces,
        tunnels: server.tunnels.iter().map(|t| t.spec.clone()).collect(),
    }
}

/// Write the state, in place once a crash of the machine cannot undo it (and so the scrollback
/// files it names before it, which are written first).
pub fn save(session: &str, snap: &SessionSnapshot) -> Result<()> {
    let path = paths::session_state_path(session);
    let dir = path.parent().context("no directory for the state")?;
    durable_dir(dir)?;
    paths::write_private(&path, &serde_json::to_vec_pretty(snap)?)?;
    paths::sync_dir(dir);
    Ok(())
}

/// Write a pane's scrollback, as [`crate::pane::Pane::scrollback_ansi`] gives it, in place once
/// a crash of the machine cannot undo it: before the state names it.
pub fn save_scrollback(session: &str, file: &str, text: &[u8]) -> Result<()> {
    let dir = paths::scrollback_dir(session);
    durable_dir(&dir)?;
    paths::write_private(&dir.join(file), text)?;
    paths::sync_dir(&dir);
    Ok(())
}

/// [`paths::ensure_private_dir`], each directory it makes recorded in its parent for good.
fn durable_dir(dir: &Path) -> Result<()> {
    let missing: Vec<&Path> = dir.ancestors().take_while(|d| !d.as_os_str().is_empty() && !d.exists()).collect();
    paths::ensure_private_dir(dir)?;
    for made in missing.iter().rev() {
        if let Some(parent) = made.parent() {
            paths::sync_dir(parent);
        }
    }
    Ok(())
}

/// The scrollback files `snap` names.
pub fn scrollback_files(snap: &SessionSnapshot) -> HashSet<&str> {
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
    clean_scrollback_in(&paths::scrollback_dir(session), snap);
}

/// [`clean_scrollback`] in `dir`: only files kraal makes there, and never through a link that
/// stands in its place (with files of another directory).
fn clean_scrollback_in(dir: &Path, snap: &SessionSnapshot) {
    let keep = scrollback_files(snap);
    // Gone once nothing is left in it.
    if matches!(paths::remove_files_in(dir, |name| made_by_kraal(name) && !keep.contains(name)), Ok(0)) {
        let _ = std::fs::remove_dir(dir);
    }
}

/// Whether kraal names a file so in a scrollback directory: a pane's text
/// (`<time>-<pid>-<pane>.ans`), or one being written (`.<that name>.<pid>.<n>.tmp`).
fn made_by_kraal(name: &str) -> bool {
    let number = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if let Some(stem) = name.strip_suffix(".ans") {
        let parts: Vec<&str> = stem.split('-').collect();
        return parts.len() == 3 && parts.iter().all(|p| number(p));
    }
    let Some(inner) = name.strip_prefix('.').and_then(|n| n.strip_suffix(".tmp")) else { return false };
    let mut parts = inner.rsplitn(3, '.');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(n), Some(pid), Some(file)) => number(n) && number(pid) && !file.starts_with('.') && made_by_kraal(file),
        _ => false,
    }
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
    let snap: SessionSnapshot = serde_json::from_slice(&data).with_context(|| format!("parsing {}", path.display()))?;
    if snap.version != VERSION {
        anyhow::bail!("unsupported state version {}", snap.version);
    }
    Ok(Some(snap))
}

/// What stopping the server (to restart, or to quit) stops in a pane, given what runs in it other
/// than a shell at its prompt (`program`, its command line) and the jobs in the background of its
/// terminal (`jobs`). At the next start, a shell at its prompt comes back in its directory, an SSH
/// shell reconnects and an agent resumes its conversation (a turn in progress stops); any other
/// program stops, and a pane's own command waits for Enter. Every job in the background stops:
/// the terminal hangs up.
pub fn stops_with_server(
    pane: &crate::protocol::PaneInfo,
    program: Option<&[String]>,
    jobs: &[Vec<String>],
) -> Vec<String> {
    stops_in_foreground(pane, program).into_iter().chain(jobs_stopping_with_server(pane, jobs)).collect()
}

/// What stopping the server stops in the background of a pane's terminal (see
/// [`stops_with_server`]). It is all that stops in the pane of a command that asks to stop the
/// server: what is in the foreground there is that command.
pub fn jobs_stopping_with_server(pane: &crate::protocol::PaneInfo, jobs: &[Vec<String>]) -> Vec<String> {
    if !runs_in_terminal(pane) {
        return Vec::new();
    }
    jobs.iter().map(|job| format!("{} (in the background)", command_line(job))).collect()
}

/// Whether a program runs in the pane's terminal: not a preview, a command waiting for Enter or
/// one that ended.
fn runs_in_terminal(pane: &crate::protocol::PaneInfo) -> bool {
    pane.kind == "terminal" && !pane.suspended && pane.exit_code.is_none()
}

/// What stopping the server stops in the foreground of a pane's terminal (see
/// [`stops_with_server`]).
fn stops_in_foreground(pane: &crate::protocol::PaneInfo, program: Option<&[String]>) -> Option<String> {
    if !runs_in_terminal(pane) {
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
    Some(command_line(argv))
}

/// `argv` as a shell reads it, cut to 60 columns.
fn command_line(argv: &[String]) -> String {
    crate::text::truncate(&crate::shell::join(argv), 60)
}

/// A line of the list of what stopping the server stops: the pane, then `what`.
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
    for ws in snap.workspaces {
        let panes_before: std::collections::HashSet<PaneId> = server.panes.keys().copied().collect();
        let workspaces_before: Vec<u32> = server.workspaces.iter().map(|w| w.id).collect();
        if let Err(e) = apply_workspace(server, &ws, snap.revision) {
            server.rollback(&panes_before, &workspaces_before);
            server.warnings.push(format!("could not restore workspace {}: {e:#}; it stays saved", ws.name));
            server.unrestored.push(ws);
        }
    }
    server.relayout();
    Ok(())
}

fn apply_workspace(server: &mut Server, ws: &WorkspaceSnap, revision: u32) -> Result<()> {
    {
        let id = server.new_workspace(ws.name.clone(), ws.definition.clone());
        let wi = server.ws_index(id).unwrap();
        server.workspaces[wi].cwd = ws.cwd.clone();
        for t in &ws.tabs {
            let tab_id = server.alloc_id();
            // Older versions saved a tab without a name as "tab N"; unnamed, it shows what its
            // pane runs.
            let old_default = revision == 0 && t.name.strip_prefix("tab ").is_some_and(|n| n.parse::<u32>().is_ok());
            let mut tab = Tab::new(tab_id, if old_default { String::new() } else { t.name.clone() });
            tab.preset = t.preset;
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
            server.workspaces[wi].tabs.push(tab);
        }
        if server.workspaces[wi].tabs.is_empty() {
            server.workspaces.remove(wi);
        } else {
            let n = server.workspaces[wi].tabs.len();
            server.workspaces[wi].active = ws.active.min(n - 1);
        }
    }
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
        let saved = SavedScrollback { file: Some(file.to_string()), taken: Instant::now(), retry: false };
        server.scrollback.insert(id, saved);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let stops =
            |pane: &crate::protocol::PaneInfo, program: Option<&[String]>| stops_with_server(pane, program, &[]);
        let none: Vec<String> = Vec::new();
        let shell = pane(serde_json::json!({}));
        assert_eq!(stops(&shell, None), none, "a shell at its prompt");
        let sleep = argv(&["sleep", "1000"]);
        assert_eq!(stops(&shell, Some(&sleep)), ["sleep 1000"]);
        assert_eq!(stopping_line(&shell, "sleep 1000"), "sleep 1000  (pane 3 \"build\" in w)");
        // A pane's own command stops (it waits for Enter after the restart), unless it is an
        // SSH shell, which reconnects; and nothing runs in a pane whose program ended.
        let command = pane(serde_json::json!({"command": ["make", "watch"]}));
        assert_eq!(stops(&command, None), ["make watch"]);
        let ssh = pane(serde_json::json!({"command": ["ssh", "web-1"]}));
        assert_eq!(stops(&ssh, Some(&argv(&["ssh", "web-1"]))), none);
        let ssh_job = pane(serde_json::json!({"command": ["ssh", "web-1", "journalctl", "-f"]}));
        assert_eq!(stops(&ssh_job, None).len(), 1);
        let ended = pane(serde_json::json!({"command": ["make"], "exit_code": 0}));
        assert_eq!(stops(&ended, None), none);
        let waiting = pane(serde_json::json!({"command": ["make"], "suspended": true}));
        assert_eq!(stops(&waiting, None), none);
        let preview = pane(serde_json::json!({"kind": "preview:markdown"}));
        assert_eq!(stops(&preview, None), none);
        // Agents resume; one in the middle of a turn loses that turn.
        let idle = pane(serde_json::json!({"agent": "claude", "state": "idle"}));
        assert_eq!(stops(&idle, Some(&argv(&["claude"]))), none);
        let working = pane(serde_json::json!({"agent": "claude", "state": "working"}));
        assert!(stops(&working, Some(&argv(&["claude"])))[0].starts_with("claude: "));
        let long = argv(&[
            "cargo",
            "build",
            "--release",
            "--workspace",
            "--all-features",
            "--target",
            "x86_64-unknown-linux-musl",
        ]);
        let cut = &stops(&shell, Some(&long))[0];
        assert!(cut.chars().count() == 60 && cut.ends_with('…'), "{cut}");
        let wide = &stops(&shell, Some(&argv(&["echo", &"漢字".repeat(40)])))[0];
        assert!(crate::text::columns(wide) <= 60 && wide.ends_with('…'), "{wide}");
        // Jobs in the background stop beside whatever runs in the foreground, an idle agent
        // or a shell at its prompt included; a pane whose program ended has none.
        let jobs = [argv(&["sleep", "600"]), argv(&["tail", "-f", "log"])];
        let both = ["sleep 600 (in the background)", "tail -f log (in the background)"];
        assert_eq!(stops_with_server(&shell, None, &jobs), both);
        assert_eq!(stops_with_server(&idle, Some(&argv(&["claude"])), &jobs), both);
        assert_eq!(stops_with_server(&shell, Some(&sleep), &jobs[1..]), ["sleep 1000", both[1]]);
        assert_eq!(stops_with_server(&ended, None, &jobs), none);
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

    #[test]
    fn cleaning_up_deletes_only_what_kraal_wrote_in_its_own_directory() {
        let root = std::env::temp_dir().join(format!("kraal-clean-scrollback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let names = |snap_names: &[&str]| {
            let mut snap: SessionSnapshot = serde_json::from_str(
                r#"{"version":1,"workspaces":[{"name":"w","definition":null,"cwd":null,"active":0,"tabs":[
                {"name":"","root":null,"floating":[],"focus":0,"show_floating":false,"zoomed":false,"sync":false}]}]}"#,
            )
            .unwrap();
            for name in snap_names {
                let pane = PaneSnap {
                    name: None,
                    spec: PaneSpec::default(),
                    agent: None,
                    agent_session: None,
                    stopped: false,
                    scrollback: Some(name.to_string()),
                };
                let rect = FloatRect { x: 0.0, y: 0.0, w: 1.0, h: 1.0 };
                snap.workspaces[0].tabs[0].floating.push((pane, rect));
            }
            snap
        };
        // A link in place of the session's directory: what it leads to is not kraal's.
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        for f in ["notes.txt", "1-2-3.ans"] {
            std::fs::write(elsewhere.join(f), "mine").unwrap();
        }
        let link = root.join("link");
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();
        clean_scrollback_in(&link, &names(&[]));
        assert!(elsewhere.join("notes.txt").exists() && elsewhere.join("1-2-3.ans").exists());
        assert!(link.exists());
        // In its own directory: the files of panes gone and of writes cut short go, the others stay.
        let dir = root.join("dir");
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["1-2-3.ans", "4-5-6.ans", ".4-5-6.ans.77.0.tmp", "notes.txt", "x.ans", ".x.ans.1.2.tmp"] {
            std::fs::write(dir.join(f), "text").unwrap();
        }
        std::os::unix::fs::symlink(elsewhere.join("1-2-3.ans"), dir.join("7-8-9.ans")).unwrap();
        clean_scrollback_in(&dir, &names(&["1-2-3.ans"]));
        let mut left: Vec<String> =
            std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, [".x.ans.1.2.tmp", "1-2-3.ans", "7-8-9.ans", "notes.txt", "x.ans"]);
        assert!(elsewhere.join("1-2-3.ans").exists(), "a link is not followed");
        // Nothing left: the directory goes.
        let empty = root.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::write(empty.join("1-2-3.ans"), "text").unwrap();
        clean_scrollback_in(&empty, &names(&[]));
        assert!(!empty.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
