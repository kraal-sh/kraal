//! Workspace definitions (`.kdl` files): finding, opening and saving them.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use ratatui::layout::Rect;

use super::{FloatRect, Floating, Server, Tab};
use crate::layout::{Axis, Node, PaneId};
use crate::pane::PaneSpec;
use crate::paths;
use crate::workspace::{self, LayoutNode, PaneKind, SizeSpec, WorkspaceDef};
impl Server {
    pub(super) fn scan_definitions(&mut self) {
        self.last_def_scan = Instant::now();
        // Their changes are told, and those of accounts.toml and of the accounts' usage,
        // wherever they can be.
        let mut dirs = self.config.workspace_dirs();
        dirs.push(paths::config_dir());
        dirs.push(paths::state_dir().join("usage"));
        self.dirwatch.watch_config(&dirs);
        let defs: Vec<WorkspaceDef> = self.definitions().into_iter().filter_map(|(_, d)| d.ok()).collect();
        let names: Vec<String> = defs.iter().map(|d| d.name.clone()).collect();
        if names != self.def_names {
            self.def_names = names;
            self.mark_dirty();
        }
        self.refresh_known(&defs);
        self.reload_accounts();
    }

    pub fn definitions(&self) -> Vec<(PathBuf, Result<WorkspaceDef>)> {
        let mut out = Vec::new();
        for dir in self.config.workspace_dirs() {
            out.extend(workspace::load_dir(&dir));
        }
        out
    }

    pub(super) fn find_definition(&self, name: &str) -> Option<(PathBuf, WorkspaceDef)> {
        self.definitions().into_iter().find_map(|(p, d)| match d {
            Ok(d) if d.name == name => Some((p, d)),
            _ => None,
        })
    }

    /// Open a workspace by name: switch to it if it is running, else start it from its definition.
    pub fn open_workspace(&mut self, name: &str) -> Result<u32> {
        if let Some(ws) = self.workspaces.iter().find(|w| w.name == name) {
            return Ok(ws.id);
        }
        let (path, def) = self.find_definition(name).with_context(|| {
            let dirs: Vec<String> = self.config.workspace_dirs().iter().map(|d| paths::shorten_home(d)).collect();
            format!("no workspace named {name:?} in {}", dirs.join(", "))
        })?;
        self.start_definition(&def, Some(path))
    }

    /// Spawn every pane of a definition into a new workspace.
    pub fn start_definition(&mut self, def: &WorkspaceDef, path: Option<PathBuf>) -> Result<u32> {
        // Every pane is set up before the programs start.
        self.with_batch(|server| {
            server.starting_workspace = Some(def.name.clone());
            let before: std::collections::HashSet<PaneId> = server.panes.keys().copied().collect();
            let workspaces_before: Vec<u32> = server.workspaces.iter().map(|w| w.id).collect();
            let result = server.start_definition_inner(def, path);
            server.starting_workspace = None;
            if result.is_err() {
                server.rollback(&before, &workspaces_before);
            }
            result
        })
    }

    pub(super) fn start_definition_inner(&mut self, def: &WorkspaceDef, path: Option<PathBuf>) -> Result<u32> {
        let ws = self.new_workspace(def.name.clone(), path);
        let wi = self.ws_index(ws).unwrap();
        self.workspaces[wi].cwd = def.cwd.clone();
        let mut focus_tab = 0;
        for (ti, tdef) in def.tabs.iter().enumerate() {
            let tab_id = self.alloc_id();
            let mut tab = Tab::new(tab_id, tdef.name.clone().unwrap_or_default());
            let mut focus = None;
            let (root, area) = (&tdef.root, self.layout_area);
            let node = self.build_node(root, tdef.cwd.as_ref().or(def.cwd.as_ref()), area, &mut focus)?;
            tab.root = Some(node);
            for f in &tdef.floating {
                let id = self.spawn_from_def(f, tdef.cwd.as_ref().or(def.cwd.as_ref()))?;
                let n = tab.floating.len() as f32 * 0.03;
                tab.floating.push(Floating { pane: id, rect: FloatRect { x: 0.15 + n, y: 0.12 + n, w: 0.7, h: 0.7 } });
                if f.focus {
                    focus = Some(id);
                }
            }
            // Without a pane given the focus, the first place's: a stack keeps its expanded pane.
            let first = tab.root.as_ref().and_then(Node::first_shown);
            if let Some(f) = focus.or(first) {
                tab.set_focus(f);
            }
            if tdef.focus {
                focus_tab = ti;
            }
            let ws = &mut self.workspaces[wi];
            ws.tabs.push(tab);
        }
        if self.workspaces[wi].tabs.is_empty() {
            self.workspaces.remove(wi);
            anyhow::bail!("workspace {:?} has no panes", def.name);
        }
        self.workspaces[wi].active = focus_tab;
        self.relayout();
        Ok(ws)
    }

    /// Undo a half-built workspace: kill the panes it spawned and drop workspaces it created, so
    /// no workspace is ever left without tabs.
    pub(crate) fn rollback(&mut self, panes_before: &std::collections::HashSet<PaneId>, workspaces_before: &[u32]) {
        let new: Vec<PaneId> = self.panes.keys().copied().filter(|p| !panes_before.contains(p)).collect();
        for id in new {
            if let Some(mut p) = self.panes.remove(&id) {
                p.kill();
            }
        }
        self.workspaces.retain(|w| workspaces_before.contains(&w.id));
        for ws in &mut self.workspaces {
            for tab in &mut ws.tabs {
                let missing_panes: Vec<PaneId> =
                    tab.panes().into_iter().filter(|p| !self.panes.contains_key(p)).collect();
                for p in missing_panes {
                    tab.remove(p);
                }
            }
            ws.tabs.retain(|t| !t.panes().is_empty());
        }
        self.workspaces.retain(|w| !w.tabs.is_empty());
        self.relayout();
    }

    pub(super) fn build_node(
        &mut self,
        node: &LayoutNode,
        cwd: Option<&PathBuf>,
        area: Rect,
        focus: &mut Option<PaneId>,
    ) -> Result<Node> {
        Ok(match node {
            LayoutNode::Pane(def) => {
                let id = self.spawn_from_def(def, cwd)?;
                if def.focus {
                    *focus = Some(id);
                }
                Node::Leaf(id)
            }
            LayoutNode::Stack { panes, expanded } => {
                let mut ids = Vec::new();
                for def in panes {
                    let id = self.spawn_from_def(def, cwd)?;
                    if def.focus {
                        *focus = Some(id);
                    }
                    ids.push(id);
                }
                let open = ids.get(*expanded).or(ids.first()).copied().context("an empty stack")?;
                let mut n = Node::Stack { panes: ids, open };
                n.normalize();
                n
            }
            LayoutNode::Split { axis, children } => {
                let len = match axis {
                    Axis::Row => area.width,
                    Axis::Column => area.height,
                }
                .max(1) as f32;
                let known: f32 = children
                    .iter()
                    .filter_map(|(_, s)| match s {
                        Some(SizeSpec::Percent(p)) => Some(*p as f32),
                        Some(SizeSpec::Fixed(n)) => Some(*n as f32 * 100.0 / len),
                        None => None,
                    })
                    .sum();
                let unknown = children.iter().filter(|(_, s)| s.is_none()).count().max(1) as f32;
                let share = ((100.0 - known) / unknown).max(5.0);
                let mut out = Vec::new();
                for (child, size) in children {
                    let w = match size {
                        Some(SizeSpec::Percent(p)) => *p as f32,
                        Some(SizeSpec::Fixed(n)) => *n as f32 * 100.0 / len,
                        None => share,
                    };
                    out.push((self.build_node(child, cwd, area, focus)?, w.max(1.0)));
                }
                let mut n = Node::Split { axis: *axis, children: out };
                n.normalize();
                n
            }
        })
    }

    pub(super) fn spawn_from_def(&mut self, def: &workspace::PaneDef, cwd: Option<&PathBuf>) -> Result<PaneId> {
        let mut spec = PaneSpec {
            cwd: def.cwd.clone().or_else(|| cwd.cloned()),
            close_on_exit: def.close_on_exit,
            ..Default::default()
        };
        match &def.kind {
            // Already resolved against the pane's directory when the definition was read.
            PaneKind::Preview(p) => spec.preview = Some(p.clone()),
            PaneKind::Terminal => {
                if let Some(cmd) = &def.command {
                    spec.command = std::iter::once(cmd.clone()).chain(def.args.iter().cloned()).collect();
                } else if let Some(agent) = &def.agent {
                    // `agent="claude"` picks an account by the rules; `agent="claude:pro"` names one.
                    let (kind, account) = match agent.split_once(':') {
                        Some((k, a)) => (k, Some(a)),
                        None => (agent.as_str(), None),
                    };
                    if crate::accounts::agent_kind(kind).is_some() {
                        let ws = self.starting_workspace.clone();
                        // Rules may restrict the account in this directory's repository: the pane
                        // waits until git has said which repository that is.
                        if account.is_none() && self.accounts.undecided(kind, spec.cwd.as_deref()) {
                            return self.spawn_pending_agent(def, kind, ws, spec);
                        }
                        let a = match account {
                            Some(name) => self.accounts.get(kind, name),
                            None => self.accounts.choose(
                                kind,
                                ws.as_deref(),
                                spec.cwd.as_deref(),
                                &self.usage,
                                crate::accounts::now(),
                            ),
                        };
                        // A misspelt account is an error, not a plain shell in its place.
                        let a = a.with_context(|| {
                            format!(
                                "{kind} has no account named {:?} (`kraal accounts` lists them)",
                                account.unwrap_or("")
                            )
                        })?;
                        let mut agent_spec = self.agent_spec(&a, spec.cwd.clone(), None, &def.args)?;
                        agent_spec.close_on_exit = spec.close_on_exit;
                        spec = agent_spec;
                    } else {
                        spec.startup_input = Some(format!("{agent}\r"));
                    }
                }
            }
        }
        if def.start_suspended && !spec.command.is_empty() {
            return self.spawn_pane_suspended(def.name.clone(), spec, "start_suspended");
        }
        self.spawn_pane(def.name.clone(), spec)
    }

    /// An agent pane of a definition whose account depends on what git says about its
    /// directory: it waits (Enter does nothing useful yet) while git is asked off the loop, then
    /// starts on the account the rules pick, or waits for Enter if the definition says so.
    fn spawn_pending_agent(
        &mut self,
        def: &workspace::PaneDef,
        kind: &str,
        ws: Option<String>,
        spec: PaneSpec,
    ) -> Result<PaneId> {
        let pending = crate::pane::PendingAgent {
            agent: kind.to_string(),
            args: def.args.clone(),
            workspace: ws,
            suspended: def.start_suspended,
            close_on_exit: spec.close_on_exit,
        };
        let id = self.spawn_pane_suspended(def.name.clone(), pending_spec(&spec, pending), "choosing the account")?;
        self.resolve_pending_agent(id);
        Ok(id)
    }

    /// Ask git about the directory of pane `id`, an agent waiting for its account (see
    /// [`PaneSpec::pending_agent`]), then start it on the account the rules pick. Also for one
    /// restored from a session saved while it waited.
    pub(crate) fn resolve_pending_agent(&mut self, id: PaneId) {
        let Some(spec) = self.panes.get(&id).map(|p| p.spec.clone()) else { return };
        let Some(pending) = spec.pending_agent.clone() else { return };
        let dir = spec.cwd.clone().unwrap_or_default();
        self.background(
            move || {
                crate::git::project_path(&dir);
            },
            move |server, ()| {
                // Closed, or started by hand meanwhile: nothing to do.
                if server.panes.get(&id).is_none_or(|p| p.spec.pending_agent.as_ref() != Some(&pending)) {
                    return;
                }
                let cwd = spec.cwd.clone();
                if server.accounts.undecided(&pending.agent, cwd.as_deref()) {
                    server.warnings.push(format!(
                        "{}: git did not say which repository {} belongs to; the pane waits",
                        pending.agent,
                        cwd.as_deref().map(crate::paths::shorten_home).unwrap_or_default()
                    ));
                    return;
                }
                let a = server.accounts.choose(
                    &pending.agent,
                    pending.workspace.as_deref(),
                    cwd.as_deref(),
                    &server.usage,
                    crate::accounts::now(),
                );
                let Some(a) = a else {
                    server.warnings.push(format!(
                        "{}: no account allowed in {}: the matching rule in accounts.toml names none",
                        pending.agent,
                        cwd.as_deref().map(crate::paths::shorten_home).unwrap_or_default()
                    ));
                    return;
                };
                let started = server.agent_spec(&a, cwd, None, &pending.args).and_then(|mut agent| {
                    agent.close_on_exit = pending.close_on_exit;
                    crate::worktree_setup::carry_env(&spec.env, &mut agent.env);
                    if pending.suspended {
                        server.suspend_pane(id, agent, "start_suspended")
                    } else {
                        let pane = server.panes.get_mut(&id).context("no such pane")?;
                        pane.spec = agent;
                        pane.respawn(&server.env, &server.tx)
                    }
                });
                if let Err(e) = started {
                    server.warnings.push(format!("{}: {e:#}", pending.agent));
                }
                server.mark_persist();
                server.relayout();
                server.mark_dirty();
            },
        );
    }

    /// Current layout of a workspace as a definition (for "save workspace").
    pub fn snapshot_definition(&self, wi: usize, name: &str) -> WorkspaceDef {
        let ws = &self.workspaces[wi];
        let to_def = |id: PaneId, focused: bool| -> workspace::PaneDef {
            let p = &self.panes[&id];
            let mut agent = if p.spec.command.is_empty() { p.agent.kind.clone() } else { None };
            // A command that has not run yet still waits for Enter when the definition opens.
            let mut wait = p.suspended && !p.spec.command.is_empty();
            if let Some(pending) = &p.spec.pending_agent {
                return workspace::PaneDef {
                    name: p.name.clone(),
                    command: None,
                    args: pending.args.clone(),
                    cwd: p.spec.cwd.clone(),
                    focus: focused,
                    kind: PaneKind::Terminal,
                    agent: Some(pending.agent.clone()),
                    start_suspended: pending.suspended,
                    close_on_exit: pending.close_on_exit,
                };
            }
            let (kind, command, args) = match (&p.spec.preview, &p.spec.account) {
                (Some(path), _) => (PaneKind::Preview(path.clone()), None, Vec::new()),
                // An agent kraal started on an account is saved as that agent on that account,
                // with the arguments it was given: opening the definition starts a new
                // conversation, with the account's environment. A prompt or a one-shot run
                // (`-p`) given on its command line then waits for Enter: it would run again.
                (None, Some(account)) => {
                    agent = Some(account.clone());
                    let kind = account.split_once(':').and_then(|(k, _)| crate::accounts::agent_kind(k));
                    let args = kind.map(|k| crate::accounts::launch_args(k, &p.spec.command)).unwrap_or_default();
                    let options_only =
                        kind.and_then(|k| crate::accounts::resumable_args(k, &args)).is_some_and(|(_, exact)| exact);
                    wait |= !options_only;
                    (PaneKind::Terminal, None, args)
                }
                (None, None) => {
                    let mut it = p.spec.command.iter().cloned();
                    (PaneKind::Terminal, it.next(), it.collect())
                }
            };
            workspace::PaneDef {
                name: p.name.clone(),
                command,
                args,
                cwd: p.current_cwd().or_else(|| p.spec.cwd.clone()),
                focus: focused,
                kind,
                agent,
                start_suspended: wait,
                close_on_exit: p.spec.close_on_exit,
            }
        };
        fn conv(n: &Node, focus: Option<PaneId>, to_def: &dyn Fn(PaneId, bool) -> workspace::PaneDef) -> LayoutNode {
            match n {
                Node::Leaf(id) => LayoutNode::Pane(to_def(*id, focus == Some(*id))),
                Node::Stack { panes, open } => LayoutNode::Stack {
                    panes: panes.iter().map(|id| to_def(*id, focus == Some(*id))).collect(),
                    expanded: panes.iter().position(|id| id == open).unwrap_or(0),
                },
                Node::Split { axis, children } => {
                    let sum: f32 = children.iter().map(|(_, w)| *w).sum::<f32>().max(f32::EPSILON);
                    LayoutNode::Split {
                        axis: *axis,
                        children: children
                            .iter()
                            .map(|(c, w)| {
                                (
                                    conv(c, focus, to_def),
                                    Some(SizeSpec::Percent(((w / sum) * 100.0).round().max(1.0) as u16)),
                                )
                            })
                            .collect(),
                    }
                }
            }
        }
        let tabs = ws
            .tabs
            .iter()
            .enumerate()
            .map(|(i, t)| {
                // A definition always has tiled panes: a tab of floating panes alone gets a shell
                // under them (as Zellij gives it) rather than being left out.
                let root = match &t.root {
                    Some(root) => conv(root, t.focus, &to_def),
                    None => LayoutNode::Pane(workspace::PaneDef { cwd: ws.cwd.clone(), ..Default::default() }),
                };
                workspace::TabDef {
                    name: (!t.name.is_empty()).then(|| t.name.clone()),
                    focus: i == ws.active,
                    cwd: None,
                    root,
                    floating: t.floating.iter().map(|f| to_def(f.pane, t.focus == Some(f.pane))).collect(),
                }
            })
            .collect();
        WorkspaceDef { name: name.to_string(), cwd: ws.cwd.clone(), tabs, warnings: Vec::new() }
    }

    pub fn definition_dir(&self) -> PathBuf {
        self.config.workspace_dirs().into_iter().next().unwrap_or_else(paths::workspaces_dir)
    }
}

/// The waiting pane of an agent whose account is not chosen yet: it says why, and keeps what to
/// start.
fn pending_spec(spec: &PaneSpec, pending: crate::pane::PendingAgent) -> PaneSpec {
    PaneSpec {
        command: vec![
            "sh".into(),
            "-c".into(),
            "echo 'kraal: this agent waits for the account rules (asking git about this directory)'".into(),
        ],
        pending_agent: Some(pending),
        ..spec.clone()
    }
}
