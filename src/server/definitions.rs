//! Workspace definitions (`.kdl` files): finding, opening and saving them.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use ratatui::layout::Rect;

use super::{FloatRect, Floating, Server, Tab};
use crate::layout::{Axis, Node, PaneId};
use crate::pane::PaneSpec;
use crate::paths;
use crate::workspace::catalog::{self, Entry, Kept, Source};
use crate::workspace::{self, LayoutNode, PaneKind, SizeSpec, WorkspaceDef};

pub struct DefinitionSave {
    pub path: PathBuf,
    pub revision: paths::FileRevision,
    pub question: Option<String>,
}

impl Server {
    pub(super) fn workspace_catalog<'a>(
        &'a self,
        sources: impl IntoIterator<Item = Source<'a>>,
    ) -> Vec<Entry<'a, &'a super::Workspace>> {
        catalog::entries(
            self.workspaces.iter().map(|w| Kept {
                workspace: w,
                id: Some(&w.stable_id),
                definition: w.definition.as_deref(),
            }),
            sources,
            |path| matches!(self.definition_workspace(path), Ok(None)),
        )
    }

    pub(super) fn cached_workspace_catalog(&self) -> Vec<Entry<'_, &super::Workspace>> {
        self.workspace_catalog(self.def_files.iter().filter(|(path, _)| !self.source_removed(path)).map(Source::cached))
    }

    pub(super) fn workspace_activity(&self, ws: &super::Workspace) -> (usize, usize, usize) {
        use crate::agent::AgentState;

        let (mut working, mut blocked, mut done) = (0, 0, 0);
        for pane in ws.panes() {
            match self.panes.get(&pane).map(|p| p.agent.state) {
                Some(AgentState::Working) => working += 1,
                Some(AgentState::Blocked) => blocked += 1,
                Some(AgentState::Done) => done += 1,
                _ => {}
            }
        }
        (working, blocked, done)
    }

    pub(super) fn workspace_info(&self, ws: &super::Workspace) -> crate::protocol::WorkspaceInfo {
        let (working, blocked, done) = self.workspace_activity(ws);
        let (tabs, pane_count) = ws.counts();
        crate::protocol::WorkspaceInfo {
            id: Some(ws.stable_id.clone()),
            name: ws.name.clone(),
            open: !ws.closed(),
            definition: ws.definition.clone(),
            tabs,
            panes: pane_count,
            working,
            blocked,
            done,
        }
    }

    pub(super) fn scan_definitions(&mut self) {
        self.last_def_scan = Instant::now();
        // Their changes are told, and those of accounts.toml and of the accounts' usage,
        // wherever they can be.
        let mut dirs = self.config.workspace_dirs();
        dirs.extend(self.workspaces.iter().filter_map(|w| w.definition.as_ref()?.parent().map(Path::to_path_buf)));
        dirs.push(paths::config_dir());
        dirs.push(paths::state_dir().join("usage"));
        dirs.push(paths::state_dir().join("usage/by-id"));
        self.dirwatch.watch_config(&dirs);
        let listed = self.definitions();
        self.follow_definitions(&listed);
        self.refresh_source_status();
        let files: Vec<(PathBuf, String)> =
            listed.iter().filter_map(|(p, d)| Some((p.clone(), d.as_ref().ok()?.name.clone()))).collect();
        if files != self.def_files {
            self.def_files = files;
            self.mark_dirty();
        }
        let defs: Vec<WorkspaceDef> = listed.into_iter().filter_map(|(_, d)| d.ok()).collect();
        self.refresh_known(&defs);
        self.reload_accounts();
    }

    /// Keep each saved workspace linked to its definition file: a file renamed on disk is found
    /// again by its inode. A missing file keeps its path, but not its inode, which a new file may take.
    fn follow_definitions(&mut self, listed: &[(PathBuf, Result<WorkspaceDef>)]) {
        for wi in 0..self.workspaces.len() {
            let ws = &mut self.workspaces[wi];
            let Some(path) = &ws.definition else { continue };
            if let Some(seen) = paths::file_id(path) {
                ws.definition_inode = Some(seen);
                continue;
            }
            let Some(was) = ws.definition_inode.take() else { continue };
            let moved = listed.iter().map(|(p, _)| p).find(|p| paths::file_id(p) == Some(was));
            if let Some(path) = moved.filter(|p| matches!(self.definition_workspace(p), Ok(None))) {
                self.workspaces[wi].set_definition(Some(path));
                self.mark_persist();
            }
        }
    }

    pub fn definitions(&self) -> Vec<(PathBuf, Result<WorkspaceDef>)> {
        let mut out = Vec::new();
        for dir in self.config.workspace_dirs() {
            out.extend(workspace::load_dir(&dir));
        }
        let mut seen = HashSet::new();
        out.retain(|(path, _)| !self.source_removed(path) && seen.insert(path.clone()));
        out
    }

    fn find_definition(&self, name: &str) -> Result<Option<PathBuf>> {
        let mut found = self.definitions().into_iter().filter_map(|(p, d)| (d.ok()?.name == name).then_some(p));
        let path = found.next();
        anyhow::ensure!(
            found.next().is_none(),
            "several workspace files are named {name:?}; choose a file in the workspace picker"
        );
        Ok(path)
    }

    pub(super) fn named_workspace(&self, name: &str) -> Result<Option<usize>> {
        let mut found = self.workspaces.iter().enumerate().filter(|(_, w)| w.name == name);
        let first = found.next().map(|(i, _)| i);
        let failed: Vec<_> = self.unrestored.iter().filter(|w| w.name == name).collect();
        anyhow::ensure!(
            found.next().is_none() && usize::from(first.is_some()) + failed.len() <= 1,
            "several saved workspaces are named {name:?}; select a workspace and rename it before retrying"
        );
        if let Some(ws) = failed.first() {
            anyhow::bail!(
                "workspace {name:?} could not be restored: {}; it stays saved; fix the cause and restart the session",
                ws.restore_error.as_deref().unwrap_or("its panes could not be started")
            );
        }
        Ok(first)
    }

    /// The saved workspace linked to the definition file at `path` (as [`Server::definitions`]
    /// lists it). An exact association wins over aliases; several alias owners need a name.
    pub(super) fn definition_workspace(&self, path: &Path) -> Result<Option<u32>> {
        let normalized = workspace::definition_path(path);
        if let Some(ws) = self.workspaces.iter().find(|w| w.definition.as_ref() == Some(&normalized)) {
            return Ok(Some(ws.id));
        }
        let mut aliases =
            self.workspaces.iter().filter(|w| w.definition.as_deref().is_some_and(|p| paths::same_file(p, path)));
        let Some(ws) = aliases.next() else { return Ok(None) };
        if let Some(other) = aliases.next() {
            anyhow::bail!(
                "{} belongs to several saved workspaces, including {:?} and {:?}; open a workspace by its name",
                path.display(),
                ws.name,
                other.name
            );
        }
        Ok(Some(ws.id))
    }

    /// Reopen a saved workspace by name or its source, else import the definition.
    pub fn open_workspace(&mut self, name: &str) -> Result<u32> {
        if let Some(wi) = self.named_workspace(name)? {
            let id = self.workspaces[wi].id;
            self.reopen_workspace(id)?;
            return Ok(id);
        }
        let path = self.find_definition(name)?.with_context(|| {
            let names_server = crate::servers::names_server(&self.known_servers, name);
            workspace::unknown(name, &self.config.workspace_dirs(), names_server)
        })?;
        self.open_definition(&path)
    }

    /// Reopen the workspace imported from `path`, else import it.
    pub(super) fn open_definition(&mut self, path: &Path) -> Result<u32> {
        if let Some(ws) =
            self.unrestored.iter().find(|w| w.definition.as_deref().is_some_and(|p| paths::same_file(p, path)))
        {
            anyhow::bail!(
                "workspace {:?} could not be restored: {}; it stays saved; fix the cause and restart the session",
                ws.name,
                ws.restore_error.as_deref().unwrap_or("its panes could not be started")
            );
        }
        if let Some(ws) = self.definition_workspace(path)? {
            self.reopen_workspace(ws)?;
            return Ok(ws);
        }
        let source = paths::read_regular_text(path, u64::MAX)?;
        let name = path.file_stem().unwrap_or_default().to_string_lossy();
        let def = workspace::parse_workspace(&name, &source).with_context(|| format!("parsing {}", path.display()))?;
        let order = self.updated_workspace_order();
        let id = self.start_definition(&def, Some(path.to_path_buf()))?;
        let wi = self.ws_index(id).unwrap();
        self.workspaces[wi].definition_source = Some(source);
        self.workspace_order = order;
        self.bind_workspace_order(id);
        self.sort_workspaces();
        self.removed_sources.retain(|p| !paths::same_file(p, path));
        self.mark_persist();
        Ok(id)
    }

    pub(super) fn prepare_layout_export(&self, wi: usize, path: &Path) -> Result<DefinitionSave> {
        self.check_definition_owner(wi, path)?;
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
        std::fs::create_dir_all(parent)?;
        let path = workspace::definition_path(path);
        let revision = paths::FileRevision::read(&path)?;
        if revision.bytes.is_some() {
            self.check_definition_owner(wi, &revision.target.with_extension("kdl.bak"))?;
        }
        let ws = &self.workspaces[wi];
        let source =
            ws.definition.as_deref().filter(|p| paths::same_file(p, &path)).and(ws.definition_source.as_deref());
        let question = revision.bytes.as_ref().map(|_| replacement_question(&path, source, &revision));
        Ok(DefinitionSave { path, revision, question })
    }

    pub(super) fn export_file_default(&self, wi: usize) -> PathBuf {
        let base = workspace::file_name_from(&self.workspaces[wi].name);
        (0..)
            .map(|n| {
                self.definition_dir().join(if n == 0 { format!("{base}.kdl") } else { format!("{base}-copy-{n}.kdl") })
            })
            .find(|p| {
                !p.exists()
                    && self.check_definition_owner(wi, p).is_ok()
                    && self.workspaces[wi].definition.as_deref().is_none_or(|own| !paths::same_file(own, p))
            })
            .unwrap()
    }

    fn check_definition_owner(&self, wi: usize, path: &Path) -> Result<()> {
        let other = self
            .workspaces
            .iter()
            .enumerate()
            .find_map(|(i, ws)| {
                (i != wi && ws.definition.as_deref().is_some_and(|p| paths::same_file(p, path))).then_some(&ws.name)
            })
            .or_else(|| {
                self.unrestored.iter().find_map(|ws| {
                    ws.definition.as_deref().is_some_and(|p| paths::same_file(p, path)).then_some(&ws.name)
                })
            });
        if let Some(name) = other {
            anyhow::bail!("{} belongs to workspace {name:?}; choose another name", path.display());
        }
        Ok(())
    }

    pub(super) fn write_definition(&mut self, wi: usize, name: &str, save: &DefinitionSave) -> Result<PathBuf> {
        let current = self.prepare_layout_export(wi, &save.path)?;
        anyhow::ensure!(
            current.path == save.path && current.revision == save.revision,
            "File changed while saving; retry to review {}",
            save.path.display()
        );
        let mut def = self.snapshot_definition(wi, name)?;
        def.portable_accounts();
        let removed = self.removed_sources.clone();
        // Publication replaces a directory entry. A hard link to the source becomes
        // a separate file, while a symlink still writes through to the source entry.
        if self.workspaces[wi].definition.as_deref().is_none_or(|p| {
            std::fs::canonicalize(p).unwrap_or_else(|_| workspace::definition_path(p)) != save.revision.target
        }) && !self.removed_sources.contains(&save.path)
        {
            // Keep a scanned export from appearing as a second workspace, including after a crash.
            self.removed_sources.push(save.path.clone());
        }
        if let Err(failure) = self.save_reversible(|server| {
            server.removed_sources = removed.clone();
            server.mark_persist();
        }) {
            let (error, rollback) = match failure {
                super::persistence::SaveFailure::Initial(error) => (error, String::new()),
                super::persistence::SaveFailure::Rollback { error, rollback } => {
                    (error, format!("\nCouldn't restore the saved listing; retry will repair it.\n{rollback}"))
                }
            };
            anyhow::bail!(
                "Couldn't prepare the export. The file is unchanged. Fix the save error and Retry.\n{error}{rollback}"
            );
        }
        let source = match workspace::save_file(&save.path, &def, &save.revision) {
            Ok(source) => source,
            Err(e) => {
                if self.removed_sources != removed {
                    self.removed_sources = removed;
                    self.mark_persist();
                    if let Err(rollback) = self.save_now() {
                        anyhow::bail!(
                            "{e:#}\nCouldn't restore the saved listing. Fix the save error and Retry.\n{rollback}"
                        );
                    }
                }
                return Err(e);
            }
        };
        if self.workspaces[wi].definition.as_deref().is_some_and(|p| paths::same_file(p, &save.path)) {
            self.workspace_order = self.updated_workspace_order();
            self.workspaces[wi].set_definition(Some(&save.path));
            self.workspaces[wi].definition_source = Some(source);
            self.workspaces[wi].source_reviewed = None;
            self.refresh_source_status();
            self.bind_workspace_order(self.workspaces[wi].id);
            self.mark_persist();
        }
        self.workspace_order.retain(|key| {
            !matches!(key, crate::persist::WorkspaceKey::Source(path) if path == &save.path && self.removed_sources.contains(path))
        });
        self.mark_persist();
        self.refresh_workspace_pickers();
        self.mark_dirty();
        Ok(save.path.clone())
    }

    /// Spawn every pane of a definition into a new workspace.
    pub(super) fn start_definition(&mut self, def: &WorkspaceDef, path: Option<PathBuf>) -> Result<u32> {
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
        self.populate_definition(wi, def)?;
        self.relayout();
        Ok(ws)
    }

    pub(super) fn populate_definition(&mut self, wi: usize, def: &WorkspaceDef) -> Result<()> {
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
            anyhow::bail!("workspace {:?} has no panes", def.name);
        }
        self.workspaces[wi].active = focus_tab;
        Ok(())
    }

    /// Undo a half-built workspace: kill the panes it spawned and drop workspaces it created, so
    /// existing empty and closed workspaces stay intact.
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
                        let a = match (def.account_id.as_deref(), account) {
                            (Some(id), _) => self.accounts.by_id(id).filter(|a| a.agent == kind),
                            (None, Some(name)) => self.accounts.get(kind, name),
                            (None, None) => self.accounts.choose(
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
                                "{} (`kraal accounts` lists them)",
                                if def.account_id.is_some() {
                                    "the account was removed; choose an account again".into()
                                } else {
                                    crate::accounts::missing_account(kind, account.unwrap_or(""))
                                }
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
        if def.start_suspended && (!spec.command.is_empty() || spec.startup_input.is_some()) {
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

    /// Export from retained data so a closed workspace needs no processes.
    fn snapshot_definition(&self, wi: usize, name: &str) -> Result<WorkspaceDef> {
        let snap = crate::server::persistence::snapshot(self)
            .workspaces
            .into_iter()
            .find(|w| w.id == self.workspaces[wi].stable_id)
            .unwrap();
        if snap.unopened {
            return workspace::parse_workspace(
                name,
                snap.pending_layout
                    .as_deref()
                    .or(snap.definition_source.as_deref())
                    .context("No retained source. Open the workspace before exporting.")?,
            );
        }
        Ok(workspace::from_snapshot(&snap, name))
    }

    pub fn definition_dir(&self) -> PathBuf {
        self.config.workspace_dirs().into_iter().next().unwrap_or_else(paths::workspaces_dir)
    }
}

fn replacement_question(path: &Path, source: Option<&str>, revision: &paths::FileRevision) -> String {
    let changed = source.is_some_and(|s| Some(s.as_bytes()) != revision.bytes.as_deref());
    format!(
        "{}\n{} {}\nComments and unsupported KDL will not be preserved.\nA backup will be kept. Cancel to export a copy.",
        if changed { "File changed" } else { "Replace file?" },
        path.display(),
        if changed { "changed since it was loaded." } else { "already exists." }
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> Server {
        let (tx, _rx) = super::super::queue::channel(32);
        let mut server = super::super::tests::server(tx);
        super::super::persistence::tests::writer(&mut server, vec![]);
        server
    }

    fn directory(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kraal-save-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(dir).unwrap()
    }

    fn linked(server: &mut Server, path: &Path, source: &str) -> usize {
        std::fs::write(path, source).unwrap();
        let id = server.new_workspace(path.file_stem().unwrap().to_string_lossy().into(), Some(path.into()));
        let wi = server.ws_index(id).unwrap();
        server.workspaces[wi].definition_source = Some(source.into());
        wi
    }

    #[test]
    fn catalog_keeps_live_counts_and_picker_actions_through_close_and_pending_operations() {
        use crate::agent::AgentState;
        use crate::server::ui::PickAction;

        let mut server = server();
        let def = workspace::parse_workspace(
            "personal",
            "layout { pane command=\"true\" start_suspended=true; pane command=\"true\" start_suspended=true; pane command=\"true\" start_suspended=true; }",
        ).unwrap();
        let ws = server.start_definition_inner(&def, None).unwrap();
        for (pane, state) in
            server.workspaces[0].panes().into_iter().zip([AgentState::Working, AgentState::Blocked, AgentState::Done])
        {
            server.panes.get_mut(&pane).unwrap().agent.state = state;
        }
        let info = server.workspace_info(&server.workspaces[0]);
        assert_eq!((info.tabs, info.panes, info.working, info.blocked, info.done), (1, 3, 1, 1, 1));
        assert!(info.open);
        let picker = server.workspace_picker(ws);
        assert_eq!(picker.items[0].detail, "Open · 1 tab · 3 panes · ▲1 ●1 ✓1 · Current");
        assert_eq!(picker.items[0].state, Some(AgentState::Blocked));
        assert!(matches!(picker.items[0].action, PickAction::SwitchWorkspace(id) if id == ws));
        server.keep_and_close_workspace(ws).unwrap();
        let info = server.workspace_info(&server.workspaces[0]);
        assert_eq!((info.tabs, info.panes, info.working, info.blocked, info.done), (1, 3, 0, 0, 0));
        assert!(!info.open);
        assert!(matches!(server.workspace_picker(ws).items[0].action, PickAction::ReopenWorkspace(id) if id == ws));
        server.workspaces[0].close_error = Some("disk full".into());
        assert!(matches!(server.workspace_picker(ws).items[0].action, PickAction::RetryWorkspaceClose(id) if id == ws));
        server.workspaces[0].delete_pending = true;
        assert!(
            matches!(server.workspace_picker(ws).items[0].action, PickAction::RetryWorkspaceDelete(id) if id == ws)
        );
    }

    #[test]
    fn failed_workspaces_keep_their_source_without_opening_a_second_copy() {
        let mut server = server();
        let dir = directory("failed-source");
        let path = dir.join("original.kdl");
        linked(&mut server, &path, "// unchanged\nlayout { pane; }");
        server.workspaces[0].name = "personal name".into();
        server.unrestored = crate::server::persistence::snapshot(&server).workspaces;
        server.unrestored[0].restore_error = Some("shell missing".into());
        server.workspaces.clear();
        for error in [server.open_definition(&path).unwrap_err(), server.open_workspace("personal name").unwrap_err()] {
            assert!(error.to_string().contains("shell missing"));
            assert!(error.to_string().contains("fix the cause and restart"));
        }
        assert!(server.workspaces.is_empty());
        assert_eq!(server.unrestored.len(), 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "// unchanged\nlayout { pane; }");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn ambiguous_workspace_names_never_pick_the_first_saved_layout_or_file() {
        let mut server = server();
        server.new_workspace("same".into(), None);
        server.new_workspace("other".into(), None);
        server.workspaces[1].name = "same".into();
        for ws in &mut server.workspaces {
            ws.tabs.push(crate::server::Tab::new(99, String::new()));
        }
        assert!(server.open_workspace("same").unwrap_err().to_string().contains("several saved workspaces"));
        let result = server.request(
            0,
            crate::protocol::Request::ExportWorkspace {
                workspace: Some("same".into()),
                identity: None,
                path: "/unused.kdl".into(),
                expected: None,
            },
        );
        assert!(result.immediate().unwrap_err().contains("several saved workspaces"));
        server.unrestored = crate::server::persistence::snapshot(&server).workspaces.into_iter().skip(1).collect();
        server.workspaces.pop();
        assert!(server.open_workspace("same").unwrap_err().to_string().contains("several saved workspaces"));
        server.workspaces.clear();
        server.unrestored.clear();
        let dir = directory("ambiguous-sources");
        for sub in ["one", "two"] {
            let path = dir.join(sub);
            std::fs::create_dir_all(&path).unwrap();
            std::fs::write(path.join("same.kdl"), "layout { pane; }").unwrap();
            server.config.workspace_dirs.push(path.display().to_string());
        }
        assert!(
            server.open_workspace("same").unwrap_err().to_string().contains("choose a file in the workspace picker")
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn directory_aliases_list_and_open_one_definition_by_name() {
        let mut server = server();
        let dir = directory("directory-aliases");
        let real = dir.join("real");
        let alias = dir.join("alias");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let path = real.join("directory-alias.kdl");
        let source = "layout { pane command=\"true\" start_suspended=true; }\n";
        std::fs::write(&path, source).unwrap();
        server.config.workspace_dirs = vec![real.display().to_string(), alias.display().to_string()];
        assert_eq!(server.definitions().iter().filter(|(p, _)| p == &path).count(), 1);
        let id = server.open_workspace("directory-alias").unwrap();
        assert_eq!(server.open_workspace("directory-alias").unwrap(), id);
        assert_eq!(server.workspaces.len(), 1);
        assert_eq!(server.workspaces[0].definition.as_ref(), Some(&path));
        assert_eq!(server.workspaces[0].panes().len(), 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_save_refuses_another_workspaces_path_and_its_aliases() {
        let dir = directory("owners");
        let path = dir.join("work.kdl");
        let mut server = server();
        let wi = linked(&mut server, &path, "layout { pane; }");
        let other = server.new_workspace("other".into(), Some(path.clone()));
        assert!(server.prepare_layout_export(wi, &path).err().unwrap().to_string().contains("belongs to workspace"));
        let oi = server.ws_index(other).unwrap();
        for alias in ["symlink.kdl", "hardlink.kdl"] {
            let alias = dir.join(alias);
            if alias.file_stem().unwrap() == "symlink" {
                std::os::unix::fs::symlink(&path, &alias).unwrap();
            } else {
                std::fs::hard_link(&path, &alias).unwrap();
            }
            server.workspaces[oi].set_definition(Some(&alias));
            assert!(server.prepare_layout_export(wi, &path).is_err());
            assert_eq!(server.definition_workspace(&alias).unwrap(), Some(other));
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "layout { pane; }");
        assert!(!path.with_extension("kdl.bak").exists());
        std::fs::remove_file(&path).unwrap();
        server.workspaces[oi].set_definition(Some(&dir.join("symlink.kdl")));
        assert!(server.prepare_layout_export(wi, &path).is_err(), "a missing file is still owned through its symlink");
        // A workspace whose panes could not be restored owns its file too.
        server.unrestored = crate::server::persistence::snapshot(&server).workspaces.into_iter().skip(1).collect();
        server.workspaces.pop();
        assert!(server.prepare_layout_export(wi, &path).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_save_refuses_a_backup_owned_through_another_symlink() {
        let dir = directory("backup-owner");
        let (target, backup) = (dir.join("work.kdl"), dir.join("work.kdl.bak"));
        let (a, b) = (dir.join("a.kdl"), dir.join("b.kdl"));
        let source = "layout { pane name=\"first\" command=\"true\" start_suspended=true; }\n";
        let other = "layout { pane name=\"second\" command=\"true\" start_suspended=true; }\n";
        std::fs::write(&target, source).unwrap();
        std::fs::write(&backup, other).unwrap();
        std::os::unix::fs::symlink(&target, &a).unwrap();
        std::os::unix::fs::symlink(&backup, &b).unwrap();
        let mut server = server();
        let id = server.open_definition(&a).unwrap();
        let wi = server.ws_index(id).unwrap();
        let save = server.prepare_layout_export(wi, &a).unwrap();
        assert!(save.question.is_some());
        server.open_definition(&b).unwrap();
        assert_eq!(server.workspaces.len(), 2);
        let error = server.write_definition(wi, "a", &save).unwrap_err().to_string();
        assert!(error.contains("belongs to workspace \"b\""), "{error}");
        assert!(error.contains(&backup.display().to_string()), "{error}");
        assert!(server.prepare_layout_export(wi, &a).err().unwrap().to_string().contains("belongs to workspace \"b\""));
        // A workspace that could not restore its panes also owns its backup destination.
        server.unrestored = crate::server::persistence::snapshot(&server).workspaces.into_iter().skip(1).collect();
        server.workspaces.pop();
        assert!(server.prepare_layout_export(wi, &a).err().unwrap().to_string().contains("belongs to workspace \"b\""));
        assert_eq!(std::fs::read_to_string(&a).unwrap(), source);
        assert_eq!(std::fs::read_to_string(&b).unwrap(), other);
        assert_eq!(std::fs::read_link(&a).unwrap(), target);
        assert_eq!(std::fs::read_link(&b).unwrap(), backup);
        assert!(!a.with_extension("kdl.bak").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn legacy_aliases_open_the_exact_association_and_ambiguous_aliases_need_a_name() {
        let dir = directory("legacy-aliases");
        let target = dir.join("shared.kdl");
        let source = "layout { pane command=\"true\" start_suspended=true; }\n";
        std::fs::write(&target, source).unwrap();
        for name in ["a", "b", "c"] {
            std::os::unix::fs::symlink(&target, dir.join(format!("{name}.kdl"))).unwrap();
        }
        let mut old = server();
        for (name, file, layout) in [
            ("alpha", "a.kdl", source),
            (
                "beta",
                "b.kdl",
                "layout { pane command=\"true\" start_suspended=true; pane command=\"false\" start_suspended=true; }\n",
            ),
        ] {
            let def = workspace::parse_workspace(name, layout).unwrap();
            old.start_definition_inner(&def, Some(dir.join(file))).unwrap();
        }
        // Older snapshots have different layouts at two aliases, and no recorded source.
        let json = serde_json::to_value(crate::server::persistence::snapshot(&old)).unwrap();
        assert!(json["workspaces"].as_array().unwrap().iter().all(|w| w.get("definition_source").is_none()));
        let mut restored = server();
        restored.config.workspace_dirs.push(dir.display().to_string());
        crate::server::persistence::apply(&mut restored, serde_json::from_value(json).unwrap()).unwrap();
        assert_eq!(restored.workspaces.len(), 2);
        let (alpha, beta) = (restored.workspaces[0].id, restored.workspaces[1].id);
        assert_eq!(restored.workspaces[0].panes().len(), 1);
        assert_eq!(restored.workspaces[1].panes().len(), 2);
        assert_eq!(restored.open_workspace("a").unwrap(), alpha);
        assert_eq!(restored.open_workspace("b").unwrap(), beta);
        let directory_alias = dir.join("directory");
        std::os::unix::fs::symlink(&dir, &directory_alias).unwrap();
        assert_eq!(restored.open_definition(&directory_alias.join("b.kdl")).unwrap(), beta);
        let error = restored.open_workspace("c").unwrap_err().to_string();
        assert!(
            error.contains("several saved workspaces") && error.contains("alpha") && error.contains("beta"),
            "{error}"
        );
        assert!(error.contains("open a workspace by its name"), "{error}");
        assert_eq!(restored.open_workspace("alpha").unwrap(), alpha);
        assert_eq!(restored.open_workspace("beta").unwrap(), beta);
        assert_eq!(restored.workspaces.len(), 2);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), source);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn opening_and_following_a_definition_preserves_what_was_loaded() {
        let dir = directory("loaded");
        let path = dir.join("work.kdl");
        let source = "// Kept exactly, including comments.\nlayout { pane command=\"true\" start_suspended=true; }\n";
        std::fs::write(&path, source).unwrap();
        let mut server = server();
        let id = server.open_definition(&path).unwrap();
        let wi = server.ws_index(id).unwrap();
        assert_eq!(server.workspaces[wi].definition_source.as_deref(), Some(source));
        let alias = dir.join("alias.kdl");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        assert_eq!(server.open_definition(&alias).unwrap(), id);
        assert_eq!(server.workspaces.len(), 1);
        let moved = dir.join("moved.kdl");
        std::fs::rename(&path, &moved).unwrap();
        server.follow_definitions(&[(moved.clone(), workspace::load_file(&moved))]);
        assert_eq!(server.workspaces[wi].definition.as_deref(), Some(moved.as_path()));
        assert_eq!(server.workspaces[wi].definition_source.as_deref(), Some(source));
        std::fs::write(&moved, "a later edit").unwrap();
        assert!(server.prepare_layout_export(wi, &moved).unwrap().question.is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn even_an_unchanged_source_needs_confirmation() {
        let dir = directory("revisions");
        let path = dir.join("work.kdl");
        let mut server = server();
        let wi = linked(&mut server, &path, "layout { pane; }");
        server.persist_dirty = false;
        assert!(server.prepare_layout_export(wi, &path).unwrap().question.is_some());
        let confirmed = server.prepare_layout_export(wi, &path).unwrap();
        assert!(confirmed.question.as_ref().unwrap().contains("Comments and unsupported KDL"));
        server.write_definition(wi, "work", &confirmed).unwrap();
        assert!(server.persist_dirty);
        let saved = std::fs::read_to_string(&path).unwrap();
        assert_eq!(server.workspaces[wi].definition_source.as_deref(), Some(saved.as_str()));
        assert!(server.prepare_layout_export(wi, &path).unwrap().question.is_some());
        std::fs::write(&path, "invalid external KDL").unwrap();
        server.follow_definitions(&[(path.clone(), workspace::load_file(&path))]);
        assert_eq!(server.workspaces[wi].definition_source.as_deref(), Some(saved.as_str()));
        assert!(server.prepare_layout_export(wi, &path).unwrap().question.unwrap().contains("File changed"));
        let confirmed = server.prepare_layout_export(wi, &path).unwrap();
        std::fs::write(&path, "another external edit").unwrap();
        assert!(server.write_definition(wi, "work", &confirmed).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "another external edit");
        let confirmed = server.prepare_layout_export(wi, &path).unwrap();
        server.write_definition(wi, "work", &confirmed).unwrap();
        assert_eq!(std::fs::read_to_string(path.with_extension("kdl.bak")).unwrap(), "another external edit");
        std::fs::remove_file(&path).unwrap();
        let save = server.prepare_layout_export(wi, &path).unwrap();
        server.write_definition(wi, "work", &save).unwrap();
        assert!(path.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_confirmed_destination_is_rechecked_for_new_owners() {
        let dir = directory("new-owner");
        let path = dir.join("work.kdl");
        let mut server = server();
        let wi = linked(&mut server, &path, "layout { pane; }");
        let save = server.prepare_layout_export(wi, &path).unwrap();
        server.new_workspace("other".into(), Some(path.clone()));
        assert!(server.write_definition(wi, "work", &save).unwrap_err().to_string().contains("belongs to workspace"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "layout { pane; }");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
