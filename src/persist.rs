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

mod migration;

const VERSION: u32 = 2;

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionSnapshot {
    pub version: u32,
    pub workspaces: Vec<WorkspaceSnap>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspace_order: Vec<WorkspaceKey>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deleted_workspaces: Vec<DeletedWorkspace>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed_sources: Vec<PathBuf>,
    #[serde(default)]
    pub tunnels: Vec<crate::server::net::TunnelSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkspaceKey {
    Id(String),
    Source(PathBuf),
}

impl WorkspaceKey {
    pub fn position(order: &[Self], id: Option<&str>, source: Option<&Path>) -> usize {
        order
            .iter()
            .position(|key| match (key, id) {
                (Self::Id(saved), Some(id)) => saved == id,
                (Self::Source(path), None) => source.is_some_and(|p| p == path),
                _ => false,
            })
            .unwrap_or(usize::MAX)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeletedWorkspace {
    pub workspace: WorkspaceSnap,
    pub position: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order_position: Option<usize>,
    pub deleted_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkspaceSnap {
    #[serde(default)]
    pub id: String,
    pub name: String,
    pub definition: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_reviewed: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout_undo: Option<Box<WorkspaceSnap>>,
    pub cwd: Option<PathBuf>,
    pub active: usize,
    pub tabs: Vec<TabSnap>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub closed: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub empty: bool,
    /// Process identities retained before signalling, so interrupted closes can finish safely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_pending: Option<CloseIntent>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub delete_pending: bool,
    /// An unimported source stays recoverable even if the external file changes or disappears.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub unopened: bool,
    #[serde(default)]
    pub sync: crate::server::SyncScope,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restore_error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_layout: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloseIntent {
    machine: Option<String>,
    boot: String,
    members: Vec<(u32, u64)>,
    #[serde(default)]
    sessions: Vec<crate::process::SessionOwner>,
}

impl CloseIntent {
    pub fn new(members: Vec<(u32, u64)>, sessions: Vec<crate::process::SessionOwner>) -> Self {
        Self { machine: crate::process::machine_id(), boot: crate::process::boot_id(), members, sessions }
    }

    pub fn local_members(&self) -> Vec<(u32, u64)> {
        if self.is_local() { self.members.clone() } else { Vec::new() }
    }

    pub fn local_sessions(&self) -> Vec<crate::process::SessionOwner> {
        if self.is_local() { self.sessions.clone() } else { Vec::new() }
    }

    fn is_local(&self) -> bool {
        self.machine.is_some() && self.machine == crate::process::machine_id() && self.boot == crate::process::boot_id()
    }
}

impl WorkspaceSnap {
    pub fn duplicate(&self, name: &str) -> Result<Self> {
        let mut copy = self.clone();
        copy.id = crate::accounts::new_uuid();
        copy.name = name.into();
        copy.definition = None;
        copy.definition_source = None;
        copy.source_reviewed = None;
        copy.layout_undo = None;
        copy.pending_layout = None;
        copy.closed = true;
        copy.close_pending = None;
        copy.delete_pending = false;
        copy.restore_error = None;
        copy.sync = crate::server::SyncScope::Off;
        if self.unopened {
            let mut def = crate::workspace::parse_workspace(
                name,
                self.pending_layout
                    .as_deref()
                    .or(self.definition_source.as_deref())
                    .context("the saved source is missing")?,
            )?;
            fn pane(p: &mut crate::workspace::PaneDef) {
                p.start_suspended = true;
                let kind = match (&p.command, &p.agent) {
                    (Some(command), _) => command.rsplit('/').next(),
                    (None, Some(agent)) => agent.split(':').next(),
                    _ => None,
                }
                .and_then(crate::accounts::agent_kind);
                if let Some(kind) = kind {
                    p.args = crate::accounts::duplicate_args(kind, &p.args);
                }
            }
            fn node(n: &mut crate::workspace::LayoutNode) {
                use crate::workspace::LayoutNode;
                match n {
                    LayoutNode::Pane(p) => pane(p),
                    LayoutNode::Split { children, .. } => children.iter_mut().for_each(|(n, _)| node(n)),
                    LayoutNode::Stack { panes, .. } => panes.iter_mut().for_each(pane),
                }
            }
            for tab in &mut def.tabs {
                node(&mut tab.root);
                tab.floating.iter_mut().for_each(pane);
            }
            copy.pending_layout = Some(crate::workspace::to_kdl(&def));
        }
        fn pane(p: &mut PaneSnap, name: &str) {
            p.agent = None;
            p.agent_session = None;
            p.scrollback = None;
            p.stopped |= !p.spec.command.is_empty();
            p.spec.startup_input = None;
            p.spec.session = None;
            p.spec.account_undo = None;
            p.spec.close_on_success = false;
            // The folder is shared, but the copy must not own the source pane's worktree.
            p.spec.worktree = None;
            if let Some(program) = p.spec.command.first()
                && let Some(kind) = p
                    .spec
                    .account
                    .as_deref()
                    .and_then(|account| account.split(':').next())
                    .and_then(crate::accounts::agent_kind)
                    .or_else(|| program.rsplit('/').next().and_then(crate::accounts::agent_kind))
            {
                let args = crate::accounts::duplicate_args(kind, &p.spec.command[1..]);
                let mut command = vec![program.clone()];
                if (p.spec.account.is_some() || p.spec.account_id.is_some())
                    && let Some(flag) = kind.session_flag
                {
                    let id = crate::accounts::new_uuid();
                    command.extend([flag.to_string(), id.clone()]);
                    p.spec.session = Some(id);
                }
                command.extend(args);
                p.spec.command = command;
            }
            if let Some(pending) = &mut p.spec.pending_agent {
                pending.suspended = true;
                pending.workspace = Some(name.into());
                if let Some(kind) = crate::accounts::agent_kind(&pending.agent) {
                    pending.args = crate::accounts::duplicate_args(kind, &pending.args);
                }
            }
        }
        fn node(n: &mut NodeSnap, name: &str) {
            match n {
                NodeSnap::Pane(p) => pane(p, name),
                NodeSnap::Split { children, .. } => children.iter_mut().for_each(|(n, _)| node(n, name)),
            }
        }
        for tab in &mut copy.tabs {
            tab.sync = false;
            if let Some(root) = &mut tab.root {
                node(root, name);
            }
            for (p, _) in &mut tab.floating {
                pane(p, name);
            }
        }
        Ok(copy)
    }

    fn clear_scrollback(&mut self) {
        if let Some(undo) = &mut self.layout_undo {
            undo.clear_scrollback();
        }
        fn clear(node: &mut NodeSnap) {
            match node {
                NodeSnap::Pane(p) => p.scrollback = None,
                NodeSnap::Split { children, .. } => {
                    for (child, _) in children {
                        clear(child);
                    }
                }
            }
        }
        for tab in &mut self.tabs {
            if let Some(root) = &mut tab.root {
                clear(root);
            }
            for (pane, _) in &mut tab.floating {
                pane.scrollback = None;
            }
        }
    }

    pub fn counts(&self) -> (usize, usize) {
        if self.unopened {
            return self
                .pending_layout
                .as_deref()
                .or(self.definition_source.as_deref())
                .and_then(|s| crate::workspace::parse_workspace(&self.name, s).ok())
                .map_or((0, 0), |d| (d.tabs.len(), d.panes().len()));
        }
        fn panes(node: &NodeSnap) -> usize {
            match node {
                NodeSnap::Pane(_) => 1,
                NodeSnap::Split { children, .. } => children.iter().map(|(n, _)| panes(n)).sum(),
            }
        }
        if self.empty {
            return (0, 0);
        }
        (self.tabs.len(), self.tabs.iter().map(|t| t.root.as_ref().map_or(0, panes) + t.floating.len()).sum())
    }

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
        let mut changed = self.layout_undo.as_mut().is_some_and(|w| w.bind_accounts(accounts));
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

/// Write the state, in place once a crash of the machine cannot undo it (and so the scrollback
/// files it names before it, which are written first).
pub fn save(session: &str, snap: &SessionSnapshot) -> Result<()> {
    let path = paths::session_state_path(session);
    let dir = path.parent().context("no directory for the state")?;
    durable_dir(dir)?;
    paths::write_private(&path, &serde_json::to_vec_pretty(snap)?)?;
    sync_directory(dir)?;
    Ok(())
}

/// Closed sessions still carry old names until their next restore. Bind them while the
/// accounts file still tells what those names meant; a running server binds its own panes.
pub fn bind_accounts(accounts: &crate::accounts::Accounts) -> Result<()> {
    for session in paths::saved_sessions().context("reading saved sessions to migrate accounts")? {
        let Some(_lock) = paths::SessionLock::acquire(&session)? else { continue };
        // 0.2.0 does not hold a session lock. Its listening socket still owns this snapshot.
        if !paths::nobody_listens(&paths::socket_path(&session)) {
            continue;
        }
        match inspect(&session) {
            Ok(Some(_)) => {}
            Err(e) if e.is::<std::io::Error>() => return Err(e),
            _ => continue,
        }
        if load(&session)?.is_none() {
            continue;
        }
        let path = paths::session_state_path(&session);
        let source = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let Some(updated) = bind_saved_accounts(&source, accounts)? else { continue };
        paths::write_private(&path, &updated).with_context(|| format!("migrating accounts in {}", path.display()))?;
    }
    Ok(())
}

fn bind_saved_accounts(source: &[u8], accounts: &crate::accounts::Accounts) -> Result<Option<Vec<u8>>> {
    // An unreadable session is left for the restore path, which already explains how to recover it.
    let Ok(snapshot) = serde_json::from_slice::<SessionSnapshot>(source) else { return Ok(None) };
    if snapshot.version != VERSION {
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
        let Some(spec) = spec.as_object_mut() else { return false };
        spec.insert("account_id".into(), serde_json::json!(pane.account_id));
        spec.insert("account".into(), serde_json::json!(pane.account));
        spec.insert("env".into(), serde_json::json!(pane.env));
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
                if let Some(child) = child.get_mut(0) {
                    changed |= node(child, accounts);
                }
            }
        }
        changed
    }
    fn workspace(value: &mut serde_json::Value, accounts: &crate::accounts::Accounts) -> bool {
        let mut changed = value.get_mut("layout_undo").is_some_and(|undo| workspace(undo, accounts));
        for tab in value.get_mut("tabs").and_then(serde_json::Value::as_array_mut).into_iter().flatten() {
            if let Some(root) = tab.get_mut("root") {
                changed |= node(root, accounts);
            }
            for floating in tab.get_mut("floating").and_then(serde_json::Value::as_array_mut).into_iter().flatten() {
                if let Some(floating) = floating.get_mut(0) {
                    changed |= pane(floating, accounts);
                }
            }
        }
        changed
    }
    let mut changed = false;
    for saved in value.get_mut("workspaces").and_then(serde_json::Value::as_array_mut).into_iter().flatten() {
        changed |= workspace(saved, accounts);
    }
    for deleted in value.get_mut("deleted_workspaces").and_then(serde_json::Value::as_array_mut).into_iter().flatten() {
        if let Some(saved) = deleted.get_mut("workspace") {
            changed |= workspace(saved, accounts);
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
    sync_directory(&dir)?;
    Ok(())
}

fn sync_directory(dir: &Path) -> Result<()> {
    std::fs::File::open(dir).and_then(|f| f.sync_all()).with_context(|| format!("making {} durable", dir.display()))
}

/// [`paths::ensure_private_dir`], each directory it makes recorded in its parent for good.
fn durable_dir(dir: &Path) -> Result<()> {
    let missing: Vec<&Path> = dir.ancestors().take_while(|d| !d.as_os_str().is_empty() && !d.exists()).collect();
    paths::ensure_private_dir(dir)?;
    for made in missing.iter().rev() {
        if let Some(parent) = made.parent() {
            sync_directory(parent)?;
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
    for tab in snap
        .workspaces
        .iter()
        .chain(snap.deleted_workspaces.iter().map(|d| &d.workspace))
        .flat_map(|w| std::iter::successors(Some(w), |w| w.layout_undo.as_deref()))
        .flat_map(|w| &w.tabs)
    {
        if let Some(root) = &tab.root {
            walk(root, &mut out);
        }
        out.extend(tab.floating.iter().filter_map(|(p, _)| p.scrollback.as_deref()));
    }
    out
}

/// Delete the scrollback files of `session` that `snap`, just written, does not name: those of
/// panes closed since, and of writes cut short.
pub fn clean_scrollback(session: &str, snap: &SessionSnapshot, retained: &HashSet<String>) {
    clean_scrollback_in(&paths::scrollback_dir(session), snap, retained);
}

/// [`clean_scrollback`] in `dir`: only files kraal makes there, and never through a link that
/// stands in its place (with files of another directory).
pub(crate) fn clean_scrollback_in(dir: &Path, snap: &SessionSnapshot, retained: &HashSet<String>) {
    let mut keep = scrollback_files(snap);
    keep.extend(retained.iter().map(String::as_str));
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

/// Inspect a stopped session without converting its state or creating migration backups.
pub fn inspect(session: &str) -> Result<Option<SessionSnapshot>> {
    read_session(session, false)
}

pub fn load(session: &str) -> Result<Option<SessionSnapshot>> {
    read_session(session, true).with_context(|| {
        format!("could not restore {}; the saved session is unchanged", paths::session_state_path(session).display())
    })
}

fn read_session(session: &str, convert: bool) -> Result<Option<SessionSnapshot>> {
    read_from(
        &paths::session_state_path(session),
        &paths::legacy_session_state_path(session),
        &paths::scrollback_dir(session),
        convert,
    )
}

fn read_from(path: &Path, legacy: &Path, scrollback: &Path, convert: bool) -> Result<Option<SessionSnapshot>> {
    if let Some(data) = read_state(path)? {
        return parse(&data).map(Some).with_context(|| format!("parsing {}", path.display()));
    }
    let Some(data) = read_state(legacy)? else { return Ok(None) };
    let version = state_version(&data)?;
    anyhow::ensure!(version <= 1, "unsupported state version {version} in {}; use a newer kraal", legacy.display());
    if version == 1 {
        return migration::read(legacy, &data, path, scrollback, convert).map(Some);
    }
    anyhow::bail!("state predates kraal 0.4; run kraal 0.4 once first")
}

fn read_state(path: &Path) -> Result<Option<Vec<u8>>> {
    match paths::read_regular(path, u64::MAX) {
        Ok(data) => Ok(Some(data)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && std::fs::symlink_metadata(path).is_err() => Ok(None),
        Err(e) => {
            Err(e).with_context(|| format!("reading {}; fix the file or its permissions and retry", path.display()))
        }
    }
}

fn state_version(data: &[u8]) -> Result<u32> {
    #[derive(Deserialize)]
    struct Format {
        version: u32,
    }
    let value: serde_json::Value = serde_json::from_slice(data)?;
    anyhow::ensure!(value.is_object(), "saved session must be an object");
    Ok(serde_json::from_slice::<Format>(data)?.version)
}

fn parse(data: &[u8]) -> Result<SessionSnapshot> {
    let version = state_version(data)?;
    anyhow::ensure!(version >= VERSION, "state predates kraal 0.4; run kraal 0.4 once first");
    anyhow::ensure!(version == VERSION, "unsupported state version {}; use a newer kraal", version);
    let snap: SessionSnapshot = serde_json::from_slice(data)?;
    validate(&snap, data, false)?;
    Ok(snap)
}

fn validate(snap: &SessionSnapshot, data: &[u8], missing_ids: bool) -> Result<()> {
    let mut ids = HashSet::new();
    for ws in snap.workspaces.iter().chain(snap.deleted_workspaces.iter().map(|d| &d.workspace)) {
        if missing_ids && ws.id.is_empty() {
            continue;
        }
        anyhow::ensure!(
            !ws.id.is_empty()
                && ws.id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
                && ids.insert(&ws.id),
            "invalid or duplicate workspace identity"
        );
    }
    let value: serde_json::Value = serde_json::from_slice(data)?;
    let state = value.as_object().context("saved session must be an object")?;
    fn workspace(ws: &serde_json::Value) -> Result<()> {
        let ws = ws.as_object().context("saved workspace must be an object")?;
        if let Some(undo) = ws.get("layout_undo").filter(|v| !v.is_null()) {
            workspace(undo)?;
        }
        for tab in ws.get("tabs").and_then(serde_json::Value::as_array).context("saved tabs must be an array")? {
            tab.as_object().context("saved tab must be an object")?;
        }
        Ok(())
    }
    for ws in
        state.get("workspaces").and_then(serde_json::Value::as_array).context("saved workspaces must be an array")?
    {
        workspace(ws)?;
    }
    if let Some(deleted) = state.get("deleted_workspaces") {
        for deleted in deleted.as_array().context("saved deleted workspaces must be an array")? {
            let deleted = deleted.as_object().context("saved deleted workspace must be an object")?;
            workspace(deleted.get("workspace").context("saved deleted workspace must contain a workspace")?)?;
        }
    }
    Ok(())
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

/// Close keeps agents and SSH commands for later, but still stops their current programs.
pub fn stops_with_workspace(
    pane: &crate::protocol::PaneInfo,
    program: Option<&[String]>,
    jobs: &[Vec<String>],
    shell_waiting: bool,
) -> Vec<String> {
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

pub fn apply(server: &mut Server, mut snap: SessionSnapshot) -> Result<()> {
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
        if ws.id.is_empty() {
            ws.id = crate::accounts::new_uuid();
        }
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

pub(crate) fn apply_workspace(server: &mut Server, ws: &WorkspaceSnap) -> Result<()> {
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

pub(crate) fn restore_workspace_tabs(server: &mut Server, wi: usize, ws: &WorkspaceSnap) -> Result<()> {
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

    fn saved_state() -> serde_json::Value {
        serde_json::json!({"version": VERSION, "workspaces": [{
            "id": "kept", "name": "w", "definition": null, "cwd": null, "active": 0,
            "tabs": [{"name": "tab", "root": {"Pane": {"spec": {}, "name": null,
                "agent": null, "agent_session": null}}, "floating": [], "focus": 0,
                "show_floating": false, "zoomed": false, "sync": false}]
        }]})
    }

    #[test]
    fn duplicate_keeps_shell_stopped_state_and_stops_commands() {
        for (stopped, command) in [(false, false), (true, false), (false, true), (true, true)] {
            let mut snap: SessionSnapshot = serde_json::from_value(saved_state()).unwrap();
            let ws = &mut snap.workspaces[0];
            let Some(NodeSnap::Pane(p)) = &mut ws.tabs[0].root else { panic!() };
            p.stopped = stopped;
            if command {
                p.spec.command = vec!["sh".into()];
            }
            let copied = ws.duplicate("copy").unwrap();
            let Some(NodeSnap::Pane(p)) = &copied.tabs[0].root else { panic!() };
            assert_eq!(p.stopped, stopped || command);
        }
    }

    #[test]
    fn duplicate_removes_conversations_scrollback_and_ownership_but_keeps_layout_and_accounts() {
        let mut snap: SessionSnapshot = serde_json::from_value(saved_state()).unwrap();
        let ws = &mut snap.workspaces[0];
        ws.definition = Some("/source.kdl".into());
        ws.definition_source = Some("original source".into());
        ws.cwd = Some("/project".into());
        ws.sync = crate::server::SyncScope::Workspace;
        ws.tabs[0].sync = true;
        ws.tabs[0].zoomed = true;
        let Some(NodeSnap::Pane(p)) = &mut ws.tabs[0].root else { panic!() };
        p.agent = Some("claude".into());
        p.agent_session = Some("observed".into());
        p.scrollback = Some("1-2-3.ans".into());
        p.spec.command =
            ["/bin/managed-agent", "--session-id", "reserved", "--model", "opus"].map(str::to_string).to_vec();
        p.spec.account = Some("claude:work".into());
        p.spec.account_id = Some("account-id".into());
        p.spec.session = Some("reserved".into());
        p.spec.startup_input = Some("continue\r".into());
        p.spec.account_undo = Some(Box::new(p.spec.clone()));
        p.spec.cwd = Some("/project/tree".into());
        p.spec.worktree = Some(crate::git::Worktree {
            repo: "/project".into(),
            path: "/project/tree".into(),
            branch: "kraal/1".into(),
            base: "main".into(),
            index: 1,
        });
        let mut floating = *p.clone();
        floating.spec.pending_agent = Some(crate::pane::PendingAgent {
            agent: "codex".into(),
            args: vec!["resume".into(), "old".into()],
            workspace: Some("w".into()),
            ..Default::default()
        });
        floating.spec.close_on_success = true;
        ws.tabs[0].floating.push((floating, FloatRect { x: 0.1, y: 0.2, w: 0.3, h: 0.4 }));
        let original = serde_json::to_value(&*ws).unwrap();
        let copied = ws.duplicate("copy").unwrap();
        assert_eq!(serde_json::to_value(ws).unwrap(), original);
        assert_ne!(copied.id, "kept");
        assert!(copied.closed);
        assert_eq!(copied.counts(), (1, 2));
        assert!(copied.definition.is_none() && copied.definition_source.is_none());
        assert_eq!(copied.sync, crate::server::SyncScope::Off);
        let tab = &copied.tabs[0];
        assert!(!tab.sync);
        assert!(tab.zoomed);
        assert_eq!(tab.focus, Some(0));
        assert_eq!(tab.floating[0].1.x, 0.1);
        let Some(NodeSnap::Pane(p)) = &tab.root else { panic!() };
        for p in [p.as_ref(), &tab.floating[0].0] {
            assert!(p.stopped);
            assert!(p.agent.is_none() && p.agent_session.is_none() && p.scrollback.is_none());
            let session = p.spec.session.as_deref().unwrap();
            assert_ne!(session, "reserved");
            assert_ne!(session, "observed");
            assert!(p.spec.account_undo.is_none() && p.spec.startup_input.is_none());
            assert!(p.spec.worktree.is_none());
            assert!(!p.spec.close_on_success);
            assert_eq!(p.spec.command, ["/bin/managed-agent", "--session-id", session, "--model", "opus"]);
            assert_eq!(p.spec.cwd.as_deref(), Some(Path::new("/project/tree")));
            assert_eq!(p.spec.account_id.as_deref(), Some("account-id"));
            assert_eq!(p.spec.account.as_deref(), Some("claude:work"));
        }
        assert_ne!(p.spec.session, tab.floating[0].0.spec.session);
        let pending = tab.floating[0].0.spec.pending_agent.as_ref().unwrap();
        assert!(pending.suspended);
        assert!(pending.args.is_empty());
        assert_eq!(pending.workspace.as_deref(), Some("copy"));
        snap.workspaces.push(copied);
        let encoded = serde_json::to_vec(&snap).unwrap();
        let restored = parse(&encoded).unwrap();
        assert_eq!(restored.version, VERSION);
        assert!(restored.workspaces[1].closed);
        assert_eq!(scrollback_files(&restored), HashSet::from(["1-2-3.ans"]));
    }

    #[test]
    fn workspace_order_resolves_saved_workspaces_by_identity() {
        let path = Path::new("/layouts/x.kdl");
        let order = [WorkspaceKey::Source(path.into()), WorkspaceKey::Id("a".into()), WorkspaceKey::Id("b".into())];
        assert_eq!(WorkspaceKey::position(&order, Some("a"), Some(path)), 1);
        assert_eq!(WorkspaceKey::position(&order, Some("b"), Some(path)), 2);
        assert_eq!(WorkspaceKey::position(&order, Some("new"), Some(path)), usize::MAX);
        assert_eq!(WorkspaceKey::position(&order, None, Some(path)), 0);
    }

    #[test]
    fn workspace_order_is_optional_and_does_not_change_legacy_state_paths() {
        let mut state = saved_state();
        state["workspaces"][0]["empty"] = serde_json::json!(true);
        state["workspaces"][0]["tabs"] = serde_json::json!([]);
        let mut second = state["workspaces"][0].clone();
        second["id"] = serde_json::json!("second");
        second["name"] = serde_json::json!("second");
        state["workspaces"].as_array_mut().unwrap().push(second);
        let mut snap: SessionSnapshot = serde_json::from_value(state).unwrap();
        assert!(snap.workspace_order.is_empty());
        assert_eq!(snap.workspaces.iter().map(|w| w.name.as_str()).collect::<Vec<_>>(), ["w", "second"]);
        snap.workspace_order = vec![WorkspaceKey::Id("second".into()), WorkspaceKey::Id("kept".into())];
        snap.workspaces.swap(0, 1);
        let session = format!("workspace-order-compat-{}", std::process::id());
        let legacy = paths::legacy_session_state_path(&session);
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        let original = b"{\"version\":1,\"workspaces\":[]}";
        std::fs::write(&legacy, original).unwrap();
        save(&session, &snap).unwrap();
        let kept = load(&session).unwrap().unwrap();
        assert_eq!(kept.workspace_order, snap.workspace_order);
        assert_eq!(kept.workspaces[0].id, "second");
        assert_eq!(std::fs::read(&legacy).unwrap(), original);
        assert!(paths::session_state_path(&session).parent().unwrap().ends_with("sessions-v2"));
        std::fs::remove_file(paths::session_state_path(&session)).unwrap();
        std::fs::remove_file(legacy).unwrap();
    }

    #[test]
    fn close_recovery_never_signals_a_different_machine_boot_or_process() {
        let pid = std::process::id();
        let start = crate::process::start_time(pid).unwrap();
        let owner = crate::process::SessionOwner { leader: pid, started: start, token: "test".into() };
        let mut intent = CloseIntent::new(vec![(pid, start)], vec![owner]);
        if intent.machine.is_some() {
            assert_eq!(intent.local_members(), vec![(pid, start)]);
            assert_eq!(intent.local_sessions().len(), 1);
        }
        intent.boot.push_str("different");
        assert!(intent.local_members().is_empty());
        assert!(intent.local_sessions().is_empty());
        intent.boot = crate::process::boot_id();
        intent.machine = Some("another machine".into());
        assert!(intent.local_members().is_empty());
        assert!(intent.local_sessions().is_empty());
        assert!(!crate::process::still_running(pid, start.wrapping_add(1)));
    }

    fn assert_state_refused(source: &[u8], reason: &str) {
        let session = format!("invalid-state-{}", crate::accounts::new_uuid());
        let path = paths::session_state_path(&session);
        let dir = path.parent().unwrap();
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(&path, source).unwrap();
        let error = format!("{:#}", load(&session).unwrap_err());
        assert!(error.contains("the saved session is unchanged"), "{error}");
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
    fn unsupported_formats_are_refused_without_reading_them_as_an_empty_session() {
        assert_state_refused(br#"{"version":3,"new_layout":{}}"#, "use a newer kraal");
        assert_state_refused(br#"{"version":1,"workspaces":[]}"#, "run kraal 0.4 once first");
    }

    #[test]
    fn duplicate_workspace_and_layout_fields_refuse_restore_without_changing_state() {
        let source = saved_state();
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

    #[test]
    fn positional_struct_arrays_refuse_restore_without_changing_state() {
        let source = saved_state();
        let mut workspace = source.clone();
        workspace["workspaces"][0] = serde_json::json!([
            "kept",
            "same",
            null,
            null,
            null,
            null,
            null,
            0,
            [],
            false,
            false,
            null,
            false,
            false,
            "off",
            null,
            null
        ]);
        let mut tab = source.clone();
        tab["workspaces"][0]["tabs"][0] = serde_json::json!(["tab 1", null, [], null, false, false, false, null]);
        let deleted = |workspace: serde_json::Value| {
            let mut value = source.clone();
            value["workspaces"] = serde_json::json!([]);
            value["deleted_workspaces"] = serde_json::json!([{
                "workspace": workspace, "position": 0, "deleted_at": 1
            }]);
            value
        };
        let mut record = deleted(source["workspaces"][0].clone());
        record["deleted_workspaces"][0] = serde_json::json!([source["workspaces"][0], 0, null, 1]);
        let deleted_workspace = deleted(workspace["workspaces"][0].clone());
        let deleted_tab = deleted(tab["workspaces"][0].clone());
        for (value, reason) in [
            (serde_json::json!([VERSION, [], []]), "session must be an object"),
            (workspace, "workspace must be an object"),
            (tab, "tab must be an object"),
            (record, "deleted workspace must be an object"),
            (deleted_workspace, "workspace must be an object"),
            (deleted_tab, "tab must be an object"),
        ] {
            let bytes = serde_json::to_vec(&value).unwrap();
            assert!(serde_json::from_slice::<SessionSnapshot>(&bytes).is_ok(), "Serde accepts positional structs");
            assert_state_refused(&bytes, reason);
            assert!(bind_saved_accounts(&bytes, &crate::accounts::Accounts::empty()).unwrap().is_none());
        }
    }

    #[test]
    fn deletion_records_are_optional_and_share_the_workspace_identity_checks() {
        let original = saved_state();
        let old = parse(&serde_json::to_vec(&original).unwrap()).unwrap();
        assert!(old.deleted_workspaces.is_empty());
        assert!(old.removed_sources.is_empty());
        let mut deleted = original.clone();
        let mut workspace = deleted["workspaces"][0].clone();
        workspace["closed"] = true.into();
        deleted["deleted_workspaces"] = serde_json::json!([{"workspace": workspace, "position": 0, "deleted_at": 1}]);
        assert_state_refused(&serde_json::to_vec(&deleted).unwrap(), "duplicate workspace identity");
        deleted["workspaces"] = serde_json::json!([]);
        deleted["removed_sources"] = serde_json::json!(["/kept/api.kdl"]);
        let snap = parse(&serde_json::to_vec(&deleted).unwrap()).unwrap();
        assert!(snap.workspaces.is_empty());
        assert_eq!(snap.deleted_workspaces.len(), 1);
        assert_eq!(snap.deleted_workspaces[0].order_position, None);
        assert_eq!(snap.deleted_workspaces[0].workspace.name, old.workspaces[0].name);
        assert_eq!(snap.removed_sources, [PathBuf::from("/kept/api.kdl")]);
    }

    #[test]
    fn saved_account_names_gain_identity_without_losing_unknown_fields() {
        let pane = serde_json::json!({
            "name": "agent", "agent": "claude", "agent_session": null,
            "future": "kept", "spec": { "account": "claude:default", "future": [1, 2] }
        });
        let source = serde_json::json!({ "version": VERSION, "future": { "spec": { "account": "claude:default" } }, "workspaces": [{
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
        future["version"] = serde_json::json!(VERSION + 1);
        assert!(bind_saved_accounts(&serde_json::to_vec(&future).unwrap(), &accounts).unwrap().is_none());
        let restored: SessionSnapshot = serde_json::from_slice(&bytes).unwrap();
        let NodeSnap::Pane(pane) = restored.workspaces[0].tabs[0].root.as_ref().unwrap() else { panic!("pane") };
        assert_eq!(pane.spec.account_id.as_deref(), Some("default-claude"));
        let mut deleted = source.clone();
        deleted["deleted_workspaces"] = serde_json::json!([{
            "workspace": source["workspaces"][0], "position": 0, "deleted_at": 1
        }]);
        deleted["workspaces"] = serde_json::json!([]);
        let bytes = bind_saved_accounts(&serde_json::to_vec(&deleted).unwrap(), &accounts).unwrap().unwrap();
        let migrated: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            migrated["deleted_workspaces"][0]["workspace"]["tabs"][0]["root"]["Pane"]["spec"]["account_id"],
            "default-claude"
        );
        assert_eq!(migrated["future"], source["future"]);
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
        assert_eq!(stops_with_workspace(&shell, Some(&argv(&["sh"])), &[], true), none);
        assert_eq!(stops_with_workspace(&shell, Some(&argv(&["sh"])), &jobs, true), both);
        assert_eq!(stops_with_workspace(&shell, None, &[], false), ["Unknown program (activity could not be checked)"]);
        assert_eq!(stops_with_workspace(&shell, Some(&sleep), &[], false), ["sleep 1000"]);
        assert_eq!(stops_with_workspace(&idle, None, &[], false), ["claude: idle"]);
        assert_eq!(stops_with_workspace(&ssh, Some(&argv(&["ssh", "web-1"])), &[], false), ["ssh web-1"]);
        for pane in [&ended, &waiting, &preview] {
            assert_eq!(stops_with_workspace(pane, None, &[], false), none);
        }
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
    fn panes_name_their_scrollback_only_with_plain_filenames() {
        let old = r#"{"version":2,"workspaces":[{"id":"w","name":"w","definition":null,"cwd":null,"active":0,"tabs":[
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
                r#"{"version":2,"workspaces":[{"id":"w","name":"w","definition":null,"cwd":null,"active":0,"tabs":[
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
        clean_scrollback_in(&link, &names(&[]), &HashSet::new());
        assert!(elsewhere.join("notes.txt").exists() && elsewhere.join("1-2-3.ans").exists());
        assert!(link.exists());
        // In its own directory: the files of panes gone and of writes cut short go, the others stay.
        let dir = root.join("dir");
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["1-2-3.ans", "4-5-6.ans", ".4-5-6.ans.77.0.tmp", "notes.txt", "x.ans", ".x.ans.1.2.tmp"] {
            std::fs::write(dir.join(f), "text").unwrap();
        }
        std::os::unix::fs::symlink(elsewhere.join("1-2-3.ans"), dir.join("7-8-9.ans")).unwrap();
        clean_scrollback_in(&dir, &names(&["1-2-3.ans"]), &HashSet::new());
        let mut left: Vec<String> =
            std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        left.sort();
        assert_eq!(left, [".x.ans.1.2.tmp", "1-2-3.ans", "7-8-9.ans", "notes.txt", "x.ans"]);
        assert!(elsewhere.join("1-2-3.ans").exists(), "a link is not followed");
        // Nothing left: the directory goes.
        let empty = root.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::write(empty.join("1-2-3.ans"), "text").unwrap();
        clean_scrollback_in(&empty, &names(&[]), &HashSet::new());
        assert!(!empty.exists());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
