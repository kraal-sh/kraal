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
const REVISION: u32 = 2;

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub version: u32,
    /// Changes that older versions read without noticing: 1 = a tab without a name is saved
    /// without one (before, as "tab N"); 2 = durable workspace identities.
    #[serde(default)]
    pub revision: u32,
    pub workspaces: Vec<WorkspaceSnap>,
    #[serde(default)]
    pub tunnels: Vec<crate::server::net::TunnelSpec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSnap {
    #[serde(default)]
    pub id: String,
    pub name: String,
    pub definition: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition_source: Option<String>,
    pub cwd: Option<PathBuf>,
    pub active: usize,
    pub tabs: Vec<TabSnap>,
    #[serde(default)]
    pub sync: crate::server::SyncScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore_error: Option<String>,
}

impl WorkspaceSnap {
    /// Bind every saved pane, including a workspace whose programs cannot be restored yet.
    pub fn bind_accounts(&mut self, accounts: &crate::accounts::Accounts) -> bool {
        fn pane(p: &mut PaneSnap, accounts: &crate::accounts::Accounts) -> bool {
            let before = (p.spec.account.clone(), p.spec.account_id.clone(), p.spec.env.clone());
            accounts.bind_pane(&mut p.spec);
            before != (p.spec.account.clone(), p.spec.account_id.clone(), p.spec.env.clone())
        }
        fn node(n: &mut NodeSnap, accounts: &crate::accounts::Accounts) -> bool {
            match n {
                NodeSnap::Pane(p) => pane(p, accounts),
                NodeSnap::Split { children, .. } => {
                    children.iter_mut().fold(false, |changed, (n, _)| node(n, accounts) || changed)
                }
            }
        }
        let mut changed = false;
        for tab in &mut self.tabs {
            if let Some(root) = &mut tab.root {
                changed |= node(root, accounts);
            }
            for (p, _) in &mut tab.floating {
                changed |= pane(p, accounts);
            }
        }
        changed
    }
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
        server.accounts.bind_pane(&mut spec);
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
            id: ws.stable_id.clone(),
            name: ws.name.clone(),
            definition: ws.definition.clone(),
            definition_source: ws.definition_source.clone(),
            cwd: ws.cwd.clone(),
            active: ws.active,
            // Typing is never broadcast after a restart: it starts again only when asked.
            sync: crate::server::SyncScope::Off,
            restore_error: None,
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

/// Closed sessions still carry old names until their next restore. Bind them while the
/// accounts file still tells what those names meant; a running server binds its own panes.
pub fn bind_accounts(accounts: &crate::accounts::Accounts) -> Result<()> {
    let dir = paths::state_dir().join("sessions");
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).context("reading saved sessions to migrate accounts"),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_none_or(|e| e != "json") {
            continue;
        }
        let Some(session) = path.file_stem().and_then(|s| s.to_str()).filter(|s| paths::valid_session_name(s)) else {
            continue;
        };
        let Some(_lock) = paths::SessionLock::acquire(session)? else { continue };
        // 0.2.0 does not hold a session lock. Its listening socket still owns this snapshot.
        if !paths::nobody_listens(&paths::socket_path(session)) {
            continue;
        }
        let source = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let Some(updated) = bind_saved_accounts(&source, accounts)? else { continue };
        paths::write_private(&path, &updated).with_context(|| format!("migrating accounts in {}", path.display()))?;
    }
    Ok(())
}

fn bind_saved_accounts(source: &[u8], accounts: &crate::accounts::Accounts) -> Result<Option<Vec<u8>>> {
    // An unreadable session is left for the restore path, which already explains how to recover it.
    let Ok(snapshot) = serde_json::from_slice::<SessionSnapshot>(source) else { return Ok(None) };
    if snapshot.version != VERSION || snapshot.revision > REVISION {
        return Ok(None);
    }
    let mut value: serde_json::Value = serde_json::from_slice(source)?;
    fn pane(value: &mut serde_json::Value, accounts: &crate::accounts::Accounts) -> bool {
        let Some(spec) = value.get_mut("spec") else { return false };
        let Ok(mut pane) = serde_json::from_value::<PaneSpec>(spec.clone()) else { return false };
        if pane.account.is_none() || pane.account_id.is_some() {
            return false;
        }
        accounts.bind_pane(&mut pane);
        spec["account_id"] = serde_json::json!(pane.account_id);
        spec["account"] = serde_json::json!(pane.account);
        spec["env"] = serde_json::json!(pane.env);
        true
    }
    fn node(value: &mut serde_json::Value, accounts: &crate::accounts::Accounts) -> bool {
        if let Some(p) = value.get_mut("Pane") {
            return pane(p, accounts);
        }
        let mut changed = false;
        if let Some(children) =
            value.get_mut("Split").and_then(|s| s.get_mut("children")).and_then(serde_json::Value::as_array_mut)
        {
            for child in children {
                changed |= node(&mut child[0], accounts);
            }
        }
        changed
    }
    let mut changed = false;
    for workspace in value["workspaces"].as_array_mut().into_iter().flatten() {
        for tab in workspace["tabs"].as_array_mut().into_iter().flatten() {
            changed |= node(&mut tab["root"], accounts);
            for floating in tab["floating"].as_array_mut().into_iter().flatten() {
                changed |= pane(&mut floating[0], accounts);
            }
        }
    }
    if changed { Ok(Some(serde_json::to_vec_pretty(&value)?)) } else { Ok(None) }
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
    load_from(&path, &paths::scrollback_dir(session)).with_context(|| {
        format!(
            "could not restore {}; the saved session is unchanged; fix the file or its permissions and retry",
            path.display()
        )
    })
}

fn load_from(path: &Path, scrollback: &Path) -> Result<Option<SessionSnapshot>> {
    let data = match paths::read_regular(path, u64::MAX) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && std::fs::symlink_metadata(path).is_err() => {
            return Ok(None);
        }
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let (snap, migrated) = migrate(&data).with_context(|| format!("parsing {}", path.display()))?;
    if let Some(updated) = migrated {
        let backup = backup_state(path, &data, &snap, scrollback)?;
        anyhow::ensure!(paths::read_regular(path, u64::MAX)? == data, "state changed during migration; retry");
        paths::write_private(path, &updated)
            .with_context(|| format!("migrating {}; the original is also in {}", path.display(), backup.display()))?;
        paths::sync_dir(path.parent().context("no directory for the state")?);
    }
    Ok(Some(snap))
}

fn migrate(data: &[u8]) -> Result<(SessionSnapshot, Option<Vec<u8>>)> {
    // Read the original bytes first: Value would silently discard duplicate fields.
    let old: SessionSnapshot = serde_json::from_slice(data)?;
    anyhow::ensure!(old.version == VERSION, "unsupported state version {}; use a kraal that supports it", old.version);
    anyhow::ensure!(old.revision <= REVISION, "unsupported state revision {}; use a newer kraal", old.revision);
    let mut ids = HashSet::new();
    for ws in &old.workspaces {
        if ws.id.is_empty() && old.revision < REVISION {
            continue;
        }
        anyhow::ensure!(
            !ws.id.is_empty()
                && ws.id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
                && ids.insert(ws.id.clone()),
            "invalid or duplicate workspace identity"
        );
    }
    let mut value: serde_json::Value = serde_json::from_slice(data)?;
    let state = value.as_object_mut().context("saved session must be an object")?;
    let workspaces = state
        .get_mut("workspaces")
        .and_then(serde_json::Value::as_array_mut)
        .context("saved workspaces must be an array")?;
    for (ws, saved) in workspaces.iter_mut().zip(&old.workspaces) {
        let ws = ws.as_object_mut().context("saved workspace must be an object")?;
        if saved.id.is_empty() {
            let mut id = crate::accounts::new_uuid();
            while !ids.insert(id.clone()) {
                id = crate::accounts::new_uuid();
            }
            ws.insert("id".into(), serde_json::json!(id));
        }
        let tabs =
            ws.get_mut("tabs").and_then(serde_json::Value::as_array_mut).context("saved tabs must be an array")?;
        for tab in tabs {
            let tab = tab.as_object_mut().context("saved tab must be an object")?;
            if old.revision == 0 && tab.get("name").and_then(serde_json::Value::as_str).is_some_and(old_tab_name) {
                tab.insert("name".into(), serde_json::json!(""));
            }
        }
    }
    if old.revision == REVISION {
        return Ok((old, None));
    }
    state.insert("revision".into(), serde_json::json!(REVISION));
    let snap = serde_json::from_value(value.clone())?;
    Ok((snap, Some(serde_json::to_vec_pretty(&value)?)))
}

fn old_tab_name(name: &str) -> bool {
    name.strip_prefix("tab ").is_some_and(|n| n.parse::<u32>().is_ok())
}

fn backup_state(path: &Path, data: &[u8], snap: &SessionSnapshot, scrollback: &Path) -> Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let backup = path.with_extension(format!("json.before-workspace-ids-{}", crate::accounts::new_uuid()));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&backup)
        .with_context(|| format!("creating migration backup {}", backup.display()))?;
    paths::sync_dir(path.parent().context("no directory for the state")?);
    // The live scrollback is overwritten and pruned later, so the rollback copy must own its bytes.
    for file in scrollback_files(snap).into_iter().filter(|f| scrollback_name(f)) {
        let bytes = match paths::read_regular(&scrollback.join(file), crate::pane::MAX_SAVED_BYTES as u64) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e).context("backing up saved scrollback before migration"),
        };
        let dir = backup.join("scrollback");
        durable_dir(&dir)?;
        paths::write_private(&dir.join(file), &bytes)?;
        paths::sync_dir(&dir);
    }
    // Written last: a backup without state.json was interrupted and cannot be used for rollback.
    paths::write_private(&backup.join("state.json"), data)?;
    paths::sync_dir(&backup);
    Ok(backup)
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

pub fn stops_with_workspace(
    pane: &crate::protocol::PaneInfo,
    program: Option<&[String]>,
    jobs: &[Vec<String>],
    shell_waiting: bool,
) -> Vec<String> {
    // Closing removes the panes: unlike a restart, it cannot promise to resume an agent or SSH shell.
    let foreground = if !runs_in_terminal(pane) {
        None
    } else if let Some(agent) = &pane.agent {
        Some(format!("{agent}: {}", pane.state.label()))
    } else if shell_waiting
        && program.is_some_and(|p| {
            let Some((shell, args)) = p.split_first() else { return false };
            // A shell can lead the foreground group while running a script. Only known
            // interactive/login invocations count as a prompt; keep other arguments visible.
            crate::pane::is_shell(shell)
                && args.iter().all(|a| matches!(a.as_str(), "-i" | "-l" | "-il" | "-li" | "--interactive" | "--login"))
        })
    {
        None
    } else {
        let argv = program.filter(|p| !p.is_empty()).or((!pane.command.is_empty()).then_some(pane.command.as_slice()));
        Some(argv.map(command_line).unwrap_or_else(|| "Unknown program (activity could not be checked)".into()))
    };
    foreground.into_iter().chain(jobs_stopping_with_server(pane, jobs)).collect()
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
    for mut ws in snap.workspaces {
        if ws.id.is_empty() {
            ws.id = crate::accounts::new_uuid();
        }
        ws.bind_accounts(&server.accounts);
        let panes_before: std::collections::HashSet<PaneId> = server.panes.keys().copied().collect();
        let workspaces_before: Vec<u32> = server.workspaces.iter().map(|w| w.id).collect();
        if let Err(e) = apply_workspace(server, &ws, snap.revision) {
            server.rollback(&panes_before, &workspaces_before);
            ws.restore_error = Some(format!("{e:#}"));
            server.warnings.push(format!(
                "could not restore workspace {}: {e:#}; it stays saved; fix the cause and restart the session",
                ws.name
            ));
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
        server.workspaces[wi].stable_id = ws.id.clone();
        server.workspaces[wi].name = ws.name.clone();
        server.workspaces[wi].cwd = ws.cwd.clone();
        server.workspaces[wi].definition_source = ws.definition_source.clone();
        for t in &ws.tabs {
            let tab_id = server.alloc_id();
            // Older versions saved a tab without a name as "tab N"; unnamed, it shows what its
            // pane runs.
            let old_default = revision == 0 && old_tab_name(&t.name);
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
        anyhow::ensure!(!server.workspaces[wi].tabs.is_empty(), "the saved workspace has no restorable panes");
        let n = server.workspaces[wi].tabs.len();
        server.workspaces[wi].active = ws.active.min(n - 1);
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

    fn legacy_state() -> serde_json::Value {
        serde_json::json!({
            "version": 1, "revision": 1, "future": { "keep": true }, "workspaces": [{
                "name": "same", "definition": "/missing/source.kdl", "definition_source": "// original\nlayout { pane; }",
                "cwd": "/missing/project", "active": 0, "future": [1, 2],
                "tabs": [{ "name": "tab 1", "root": { "Pane": {
                    "name": "agent", "agent": "claude", "agent_session": "conversation",
                    "spec": { "account": "claude:renamed", "account_id": "kept-account",
                        "env": [["CLAUDE_CONFIG_DIR", "/original/home"]] }, "scrollback": "1-2-3.ans"
                } }, "floating": [], "focus": 0, "show_floating": false, "zoomed": false, "sync": false }]
            }]
        })
    }

    fn directory(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kraal-state-{name}-{}", crate::accounts::new_uuid()));
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    #[test]
    fn migration_only_adds_identities_and_keeps_order_sources_collisions_and_accounts() {
        let mut source = legacy_state();
        let mut other = source["workspaces"][0].clone();
        other["definition"] = serde_json::json!("/another/source.kdl");
        other["restore_error"] = serde_json::json!("missing shell");
        source["workspaces"].as_array_mut().unwrap().push(other);
        let (snap, updated) = migrate(&serde_json::to_vec(&source).unwrap()).unwrap();
        assert_eq!(snap.workspaces.len(), 2);
        assert_ne!(snap.workspaces[0].id, snap.workspaces[1].id);
        assert!(!snap.workspaces[0].id.is_empty());
        let updated = updated.unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&updated).unwrap();
        for ws in value["workspaces"].as_array_mut().unwrap() {
            ws.as_object_mut().unwrap().remove("id");
        }
        value["revision"] = serde_json::json!(1);
        assert_eq!(value, source, "all existing metadata, including unknown fields, survives conversion");
        let (again, changed) = migrate(&updated).unwrap();
        assert!(changed.is_none());
        assert_eq!(again.workspaces[0].id, snap.workspaces[0].id);
    }

    #[test]
    fn releases_020_and_030_can_read_the_complete_migrated_workspace_array() {
        // These session and workspace fields are shared by both released readers. Neither
        // release rejects an additive revision; changing version would reject the whole session.
        #[derive(Serialize, Deserialize)]
        struct ReleasedWorkspace {
            name: String,
            definition: Option<PathBuf>,
            cwd: Option<PathBuf>,
            active: usize,
            tabs: Vec<TabSnap>,
            #[serde(default)]
            sync: crate::server::SyncScope,
        }
        #[derive(Serialize, Deserialize)]
        struct ReleasedSession {
            version: u32,
            #[serde(default)]
            revision: u32,
            workspaces: Vec<ReleasedWorkspace>,
            #[serde(default)]
            tunnels: Vec<crate::server::net::TunnelSpec>,
        }
        let source = serde_json::to_vec(&legacy_state()).unwrap();
        let original: ReleasedSession = serde_json::from_slice(&source).unwrap();
        let (snap, _) = migrate(&source).unwrap();
        let saved = serde_json::to_vec(&snap).unwrap();
        let read: ReleasedSession = serde_json::from_slice(&saved).unwrap();
        assert_eq!(read.version, 1);
        assert_eq!(read.workspaces.len(), 1);
        assert_eq!(serde_json::to_value(read.workspaces).unwrap(), serde_json::to_value(original.workspaces).unwrap());
    }

    #[test]
    fn migration_preserves_the_meaning_of_legacy_tab_names() {
        let mut source = legacy_state();
        source.as_object_mut().unwrap().remove("revision");
        let (snap, _) = migrate(&serde_json::to_vec(&source).unwrap()).unwrap();
        assert_eq!(snap.workspaces[0].tabs[0].name, "");
        source["revision"] = serde_json::json!(1);
        let (snap, _) = migrate(&serde_json::to_vec(&source).unwrap()).unwrap();
        assert_eq!(snap.workspaces[0].tabs[0].name, "tab 1");
    }

    #[test]
    fn the_migration_backup_is_private_complete_and_never_replaced() {
        use std::os::unix::fs::PermissionsExt;
        let dir = directory("backup");
        let path = dir.join("main.json");
        let scrollback = dir.join("scrollback");
        std::fs::create_dir(&scrollback).unwrap();
        std::fs::write(scrollback.join("1-2-3.ans"), "original text").unwrap();
        let source = serde_json::to_vec(&legacy_state()).unwrap();
        std::fs::write(&path, &source).unwrap();
        let loaded = load_from(&path, &scrollback).unwrap().unwrap();
        let backups = || {
            std::fs::read_dir(&dir)
                .unwrap()
                .map(|e| e.unwrap().path())
                .filter(|p| p.file_name().unwrap().to_string_lossy().starts_with("main.json.before-workspace-ids-"))
                .collect::<Vec<_>>()
        };
        let backup = backups().pop().unwrap();
        assert_eq!(std::fs::metadata(&backup).unwrap().permissions().mode() & 0o777, 0o700);
        assert_eq!(std::fs::metadata(backup.join("state.json")).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::read(backup.join("state.json")).unwrap(), source);
        std::fs::write(scrollback.join("1-2-3.ans"), "later text").unwrap();
        assert_eq!(std::fs::read_to_string(backup.join("scrollback/1-2-3.ans")).unwrap(), "original text");
        assert_eq!(load_from(&path, &scrollback).unwrap().unwrap().workspaces[0].id, loaded.workspaces[0].id);
        assert_eq!(backups().len(), 1, "reading migrated state does not migrate it again");
        // An older writer can save this session again; that conversion keeps a separate original.
        std::fs::write(&path, &source).unwrap();
        load_from(&path, &scrollback).unwrap();
        assert_eq!(backups().len(), 2);
        assert_eq!(std::fs::read(backup.join("state.json")).unwrap(), source);
        assert_eq!(std::fs::read_to_string(backup.join("scrollback/1-2-3.ans")).unwrap(), "original text");
        // Rollback needs no converter, just these saved bytes and their scrollback directory.
        std::fs::copy(backup.join("state.json"), &path).unwrap();
        std::fs::copy(backup.join("scrollback/1-2-3.ans"), scrollback.join("1-2-3.ans")).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), source);
        assert_eq!(std::fs::read_to_string(scrollback.join("1-2-3.ans")).unwrap(), "original text");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_failed_backup_leaves_the_state_unchanged_and_can_be_retried() {
        let dir = directory("failed-backup");
        let path = dir.join("main.json");
        let scrollback = dir.join("scrollback");
        std::fs::create_dir_all(scrollback.join("1-2-3.ans")).unwrap();
        let source = serde_json::to_vec(&legacy_state()).unwrap();
        std::fs::write(&path, &source).unwrap();
        assert!(load_from(&path, &scrollback).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), source);
        std::fs::remove_dir(scrollback.join("1-2-3.ans")).unwrap();
        assert!(
            load_from(&path, &scrollback).unwrap().is_some(),
            "missing scrollback does not prevent restoring the layout"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unreadable_future_and_invalid_states_are_never_replaced() {
        let dir = directory("invalid");
        let path = dir.join("main.json");
        let scrollback = dir.join("scrollback");
        let mut cases = vec![b"{".to_vec()];
        for (field, value) in [("version", 2), ("revision", 3), ("revision", 2)] {
            let mut source = legacy_state();
            source[field] = serde_json::json!(value);
            cases.push(serde_json::to_vec(&source).unwrap());
        }
        for id in ["bad/id", "duplicate"] {
            let mut source = legacy_state();
            source["workspaces"][0]["id"] = serde_json::json!(id);
            let ws = source["workspaces"][0].clone();
            source["workspaces"].as_array_mut().unwrap().push(ws);
            cases.push(serde_json::to_vec(&source).unwrap());
        }
        for source in cases {
            std::fs::write(&path, &source).unwrap();
            assert!(load_from(&path, &scrollback).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), source);
            assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        }
        std::fs::remove_file(&path).unwrap();
        assert!(load_from(&path, &scrollback).unwrap().is_none());
        std::os::unix::fs::symlink(dir.join("missing"), &path).unwrap();
        assert!(load_from(&path, &scrollback).is_err());
        assert!(std::fs::read_link(&path).is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn assert_state_refused(source: &[u8], reason: &str) {
        let session = format!("invalid-state-{}", crate::accounts::new_uuid());
        let path = paths::session_state_path(&session);
        let dir = path.parent().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(&path, source).unwrap();
        let error = format!("{:#}", load(&session).unwrap_err());
        assert!(error.contains("the saved session is unchanged; fix the file or its permissions and retry"), "{error}");
        assert!(error.contains(reason), "{error}");
        assert_eq!(std::fs::read(&path).unwrap(), source);
        assert_eq!(
            std::fs::read_dir(dir)
                .unwrap()
                .filter(|entry| entry.as_ref().unwrap().file_name().to_string_lossy().starts_with(&session))
                .count(),
            1,
            "refused state creates no migration backup"
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn duplicate_workspace_and_layout_fields_refuse_restore_without_changing_state() {
        for revision in 0..=REVISION {
            let mut source = legacy_state();
            source["revision"] = serde_json::json!(revision);
            source["workspaces"][0]["id"] = serde_json::json!("kept");
            let ws = &source["workspaces"][0];
            for (field, before, after) in [
                ("workspaces", &source["workspaces"], "[]"),
                ("tabs", &ws["tabs"], "[]"),
                ("root", &ws["tabs"][0]["root"], "null"),
            ] {
                let original = format!("\"{field}\":{before}");
                let duplicate = format!("{original},\"{field}\":{after}");
                let bytes = serde_json::to_string(&source).unwrap().replacen(&original, &duplicate, 1);
                assert_state_refused(bytes.as_bytes(), &format!("duplicate field `{field}`"));
            }
        }
    }

    #[test]
    fn positional_struct_arrays_refuse_restore_without_changing_state() {
        for revision in 0..=REVISION {
            let mut source = legacy_state();
            source["revision"] = serde_json::json!(revision);
            source["workspaces"][0]["id"] = serde_json::json!("kept");
            let mut workspace = source.clone();
            workspace["workspaces"][0] = serde_json::json!(["kept", "same", null, null, null, 0, [], "off", null]);
            let mut tab = source.clone();
            tab["workspaces"][0]["tabs"][0] = serde_json::json!(["tab 1", null, [], null, false, false, false, null]);
            for (value, reason) in [
                (serde_json::json!([1, revision, [], []]), "session must be an object"),
                (workspace, "workspace must be an object"),
                (tab, "tab must be an object"),
            ] {
                let bytes = serde_json::to_vec(&value).unwrap();
                assert!(serde_json::from_slice::<SessionSnapshot>(&bytes).is_ok(), "Serde accepts positional structs");
                assert_state_refused(&bytes, reason);
            }
        }
    }

    #[test]
    fn old_saved_accounts_gain_identity_without_losing_unknown_fields() {
        let pane = serde_json::json!({
            "name": "agent", "agent": "claude", "agent_session": null,
            "future": "kept", "spec": { "account": "claude:default", "future": [1, 2] }
        });
        let source = serde_json::json!({ "version": 1, "future": { "spec": { "account": "claude:default" } }, "workspaces": [{
            "name": "w", "definition": null, "cwd": null, "active": 0, "tabs": [{
                "name": "t", "root": { "Pane": pane }, "floating": [], "focus": 0,
                "show_floating": false, "zoomed": false, "sync": false
            }]
        }] });
        let accounts = crate::accounts::Accounts::empty();
        let bytes = bind_saved_accounts(&serde_json::to_vec(&source).unwrap(), &accounts).unwrap().unwrap();
        let mut migrated: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let spec = &mut migrated["workspaces"][0]["tabs"][0]["root"]["Pane"]["spec"];
        assert_eq!(spec["account_id"], "default-claude");
        assert_eq!(spec["account"], "claude:default");
        assert!(bind_saved_accounts(&bytes, &accounts).unwrap().is_none(), "an identity is never rebound");
        spec.as_object_mut().unwrap().remove("account_id");
        spec.as_object_mut().unwrap().remove("env");
        assert_eq!(migrated, source, "unknown fields and all other state survive");
        assert!(bind_saved_accounts(b"{broken", &accounts).unwrap().is_none());
        let mut future = source.clone();
        future["revision"] = serde_json::json!(REVISION + 1);
        assert!(bind_saved_accounts(&serde_json::to_vec(&future).unwrap(), &accounts).unwrap().is_none());
        let restored: SessionSnapshot = serde_json::from_slice(&bytes).unwrap();
        let NodeSnap::Pane(pane) = restored.workspaces[0].tabs[0].root.as_ref().unwrap() else { panic!("pane") };
        assert_eq!(pane.spec.account_id.as_deref(), Some("default-claude"));
    }

    fn stopping_pane(fields: serde_json::Value) -> crate::protocol::PaneInfo {
        let mut info = serde_json::json!({"id": 3, "name": "build", "title": "sh", "workspace": "w", "tab": "t",
                "kind": "terminal", "command": [], "cwd": null, "agent": null, "state": "unknown",
                "exit_code": null, "focused": false, "floating": false, "cols": 80, "rows": 24});
        for (k, v) in fields.as_object().unwrap() {
            info[k] = v.clone();
        }
        serde_json::from_value(info).unwrap()
    }

    #[test]
    fn a_restart_stops_programs_not_shells_ssh_shells_or_idle_agents() {
        let pane = stopping_pane;
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
    fn closing_a_workspace_lists_agents_ssh_jobs_and_unknown_activity_without_promising_a_resume() {
        let argv = |words: &[&str]| words.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        let shell = stopping_pane(serde_json::json!({}));
        let sh = argv(&["/bin/sh"]);
        let sleep = argv(&["sleep", "600"]);
        let jobs = [argv(&["tail", "-f", "log"])];
        assert!(stops_with_workspace(&shell, Some(&sh), &[], true).is_empty());
        assert_eq!(stops_with_workspace(&shell, Some(&sh), &jobs, true), ["tail -f log (in the background)"]);
        assert_eq!(
            stops_with_workspace(&shell, Some(&sleep), &jobs, false),
            ["sleep 600", "tail -f log (in the background)"]
        );
        assert_eq!(stops_with_workspace(&shell, Some(&sh), &[], false), ["/bin/sh"]);
        for command in [
            argv(&["sh", "-c", "sleep 600; echo done"]),
            argv(&["sh", "-ic", "sleep 600; echo done"]),
            argv(&["sh", "script.sh"]),
            argv(&["sh", "-i", "script.sh"]),
        ] {
            assert_eq!(stops_with_workspace(&shell, Some(&command), &[], true), [command_line(&command)]);
        }
        for command in [argv(&["-bash"]), argv(&["sh", "-i"]), argv(&["zsh", "-il"])] {
            assert!(stops_with_workspace(&shell, Some(&command), &[], true).is_empty());
        }
        for waiting in [false, true] {
            for program in [None, Some([].as_slice())] {
                assert_eq!(
                    stops_with_workspace(&shell, program, &[], waiting),
                    ["Unknown program (activity could not be checked)"]
                );
            }
        }
        for state in ["working", "idle", "unknown", "blocked", "done"] {
            let agent = stopping_pane(serde_json::json!({"agent": "claude", "state": state}));
            assert_eq!(
                stops_with_workspace(&agent, None, &jobs, true),
                [format!("claude: {state}"), "tail -f log (in the background)".into()]
            );
        }
        for command in [argv(&["ssh", "web-1"]), argv(&["make", "watch"])] {
            let pane = stopping_pane(serde_json::json!({"command": command}));
            assert_eq!(stops_with_workspace(&pane, None, &[], true), [command_line(&command)]);
            assert_eq!(stops_with_workspace(&pane, Some(&command), &[], false), [command_line(&command)]);
        }
        for fields in [
            serde_json::json!({"exit_code": 0}),
            serde_json::json!({"suspended": true}),
            serde_json::json!({"kind": "preview:markdown"}),
            serde_json::json!({"kind": "files"}),
        ] {
            assert!(stops_with_workspace(&stopping_pane(fields), Some(&sleep), &jobs, false).is_empty());
        }
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
