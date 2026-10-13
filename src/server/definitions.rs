//! Workspace definitions (`.kdl` files): finding, opening and saving them.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use ratatui::layout::Rect;

use super::{FloatRect, Floating, Server, Tab};
use crate::layout::{Axis, Node, PaneId};
use crate::pane::PaneSpec;
use crate::paths;
use crate::workspace::{self, LayoutNode, PaneKind, SizeSpec, WorkspaceDef};

pub struct DefinitionSave {
    pub path: PathBuf,
    pub revision: paths::FileRevision,
    pub question: Option<String>,
    link: bool,
}

impl Server {
    pub(super) fn scan_definitions(&mut self) {
        self.last_def_scan = Instant::now();
        // Their changes are told, and those of accounts.toml and of the accounts' usage,
        // wherever they can be.
        let mut dirs = self.config.workspace_dirs();
        dirs.push(paths::config_dir());
        dirs.push(paths::state_dir().join("usage"));
        dirs.push(paths::state_dir().join("usage/by-id"));
        self.dirwatch.watch_config(&dirs);
        let listed = self.definitions();
        self.follow_definitions(&listed);
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

    /// Keep each open workspace linked to its definition file: a file renamed on disk is found
    /// again by its inode. A file that is gone keeps its path, where saving the layout writes it
    /// again (an editor may be replacing it), but not its inode, which a new file may take.
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
        out.retain(|(path, _)| seen.insert(path.clone()));
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

    /// The open workspace linked to the definition file at `path` (as [`Server::definitions`]
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
                "{} belongs to several open workspaces, including {:?} and {:?}; open a workspace by its name",
                path.display(),
                ws.name,
                other.name
            );
        }
        Ok(Some(ws.id))
    }

    /// Open a workspace by name: switch to the open workspace of that name, or to the one its
    /// definition is open in, else start that definition.
    pub fn open_workspace(&mut self, name: &str) -> Result<u32> {
        if let Some(wi) = self.named_workspace(name)? {
            return Ok(self.workspaces[wi].id);
        }
        let path = self.find_definition(name)?.with_context(|| {
            let names_server = crate::servers::names_server(&self.known_servers, name);
            workspace::unknown(name, &self.config.workspace_dirs(), names_server)
        })?;
        self.open_definition(&path)
    }

    /// Switch to the workspace the definition at `path` is open in, else start it.
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
            return Ok(ws);
        }
        let source = paths::read_regular_text(path, u64::MAX)?;
        let name = path.file_stem().unwrap_or_default().to_string_lossy();
        let def = workspace::parse_workspace(&name, &source).with_context(|| format!("parsing {}", path.display()))?;
        let id = self.start_definition(&def, Some(path.to_path_buf()))?;
        let wi = self.ws_index(id).unwrap();
        self.workspaces[wi].definition_source = Some(source);
        Ok(id)
    }

    pub(super) fn prepare_definition_save(&self, wi: usize, name: &str) -> Result<DefinitionSave> {
        anyhow::ensure!(workspace::valid_file_name(name), "{}, not {name:?}", workspace::FILE_NAME_RULE);
        let ws = &self.workspaces[wi];
        let own = ws.definition.as_ref().filter(|p| p.file_stem() == Some(OsStr::new(name)));
        let path = own.cloned().unwrap_or_else(|| self.definition_dir().join(format!("{name}.kdl")));
        self.check_definition_owner(wi, &path)?;
        std::fs::create_dir_all(path.parent().context("no destination directory")?)?;
        let revision = paths::FileRevision::read(&path)
            .with_context(|| format!("reading {}; choose another name or fix the file", path.display()))?;
        if revision.bytes.is_some() {
            self.check_definition_owner(wi, &revision.target.with_extension("kdl.bak"))?;
        }
        let question = revision.bytes.as_deref().and_then(|bytes| {
            if own.is_some() && ws.definition_source.as_ref().is_some_and(|s| s.as_bytes() == bytes) {
                None
            } else if own.is_some() && ws.definition_source.is_some() {
                Some(format!("File changed\n{} changed since it was loaded.", path.display()))
            } else if own.is_some() {
                Some(format!("Replace file?\n{} has no recorded contents in this session.", path.display()))
            } else {
                Some(format!("Replace file?\n{} already exists.", path.display()))
            }
        });
        Ok(DefinitionSave { path, revision, question, link: own.is_some() || ws.definition.is_none() })
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
        let current = self.prepare_definition_save(wi, name)?;
        anyhow::ensure!(
            current.path == save.path && current.link == save.link && current.revision == save.revision,
            "File changed while saving; retry to review {}",
            save.path.display()
        );
        let def = self.snapshot_definition(wi, name);
        let source = workspace::save_file(&save.path, &def, &save.revision)?;
        if save.link {
            self.workspaces[wi].set_definition(Some(&save.path));
            self.workspaces[wi].definition_source = Some(source);
            self.mark_persist();
        }
        self.mark_dirty();
        Ok(save.path.clone())
    }

    pub(super) fn save_definition(&mut self, wi: usize, name: &str) -> Result<PathBuf> {
        let save = self.prepare_definition_save(wi, name)?;
        if let Some(question) = &save.question {
            anyhow::bail!(
                "{question} Choose another name with `kraal save <name>`, or use Save workspace layout in kraal to confirm replacement."
            );
        }
        self.write_definition(wi, name, &save)
    }

    /// Give the definition file of workspace `wi` its new name: `perso.kdl` becomes
    /// `<name>.kdl` beside it, in one step that never replaces a file already there. A workspace
    /// whose file is gone keeps none. Returns the file renamed, if one was.
    pub(super) fn rename_definition(&mut self, wi: usize, name: &str) -> Result<Option<PathBuf>> {
        let Some(from) = self.workspaces[wi].definition.clone() else { return Ok(None) };
        let to = from.with_file_name(format!("{name}.kdl"));
        if to == from {
            return Ok(None);
        }
        anyhow::ensure!(workspace::valid_file_name(name), "{}: nothing was renamed.", workspace::FILE_NAME_RULE);
        let renamed = match paths::rename_no_replace(&from, &to) {
            // A file system that ignores case finds the file itself under a name that differs
            // only in case.
            Err(e)
                if e.kind() == ErrorKind::AlreadyExists
                    && paths::file_id(&from).is_some_and(|i| paths::file_id(&to) == Some(i)) =>
            {
                std::fs::rename(&from, &to)
            }
            other => other,
        };
        match renamed {
            Ok(()) => {
                if let Some(dir) = to.parent() {
                    paths::sync_dir(dir);
                }
                self.workspaces[wi].set_definition(Some(&to));
                // Listed again at once: until the next scan, the sidebar would show the old name
                // as a definition not running.
                self.scan_definitions();
                Ok(Some(from))
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {
                self.workspaces[wi].set_definition(None);
                Ok(None)
            }
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                anyhow::bail!("{} exists: nothing was renamed.", file_name(&to))
            }
            Err(e) => anyhow::bail!("renaming {}: {e}", paths::shorten_home(&from)),
        }
    }

    /// The name workspace `wi` is saved under when none is given: its definition file's, else
    /// one made from its own name.
    pub(super) fn definition_file_name(&self, wi: usize) -> String {
        let ws = &self.workspaces[wi];
        match ws.definition.as_deref().and_then(Path::file_stem) {
            Some(stem) => stem.to_string_lossy().into_owned(),
            None => workspace::file_name_from(&ws.name),
        }
    }

    /// Spawn every pane of a definition into a new workspace.
    fn start_definition(&mut self, def: &WorkspaceDef, path: Option<PathBuf>) -> Result<u32> {
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
                    account_id: None,
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
                account_id: p.spec.account_id.clone(),
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
                    floating: t
                        .floating
                        .iter()
                        .filter(|f| self.panes.get(&f.pane).is_none_or(|p| p.spec.login.is_none()))
                        .map(|f| to_def(f.pane, t.focus == Some(f.pane)))
                        .collect(),
                }
            })
            .collect();
        WorkspaceDef { name: name.to_string(), cwd: ws.cwd.clone(), tabs, warnings: Vec::new() }
    }

    pub fn definition_dir(&self) -> PathBuf {
        self.config.workspace_dirs().into_iter().next().unwrap_or_else(paths::workspaces_dir)
    }
}

/// The name of the file at `path`, to show.
pub(super) fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
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
        super::super::tests::server(tx)
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
    fn failed_workspaces_keep_their_source_without_opening_a_second_copy() {
        let mut server = server();
        let dir = directory("failed-source");
        let path = dir.join("original.kdl");
        linked(&mut server, &path, "// unchanged\nlayout { pane; }");
        server.workspaces[0].name = "personal name".into();
        server.unrestored = crate::persist::snapshot(&server).workspaces;
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
        let result =
            server.request(0, crate::protocol::Request::SaveWorkspace { workspace: Some("same".into()), name: None });
        assert!(result.unwrap().unwrap_err().contains("several saved workspaces"));
        server.unrestored = crate::persist::snapshot(&server).workspaces.into_iter().skip(1).collect();
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
        assert!(server.save_definition(wi, "work").unwrap_err().to_string().contains("belongs to workspace"));
        let oi = server.ws_index(other).unwrap();
        for alias in ["symlink.kdl", "hardlink.kdl"] {
            let alias = dir.join(alias);
            if alias.file_stem().unwrap() == "symlink" {
                std::os::unix::fs::symlink(&path, &alias).unwrap();
            } else {
                std::fs::hard_link(&path, &alias).unwrap();
            }
            server.workspaces[oi].set_definition(Some(&alias));
            assert!(server.save_definition(wi, "work").is_err());
            assert_eq!(server.definition_workspace(&alias).unwrap(), Some(other));
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "layout { pane; }");
        assert!(!path.with_extension("kdl.bak").exists());
        std::fs::remove_file(&path).unwrap();
        server.workspaces[oi].set_definition(Some(&dir.join("symlink.kdl")));
        assert!(server.save_definition(wi, "work").is_err(), "a missing file is still owned through its symlink");
        // A workspace whose panes could not be restored owns its file too.
        server.unrestored = crate::persist::snapshot(&server).workspaces.into_iter().skip(1).collect();
        server.workspaces.pop();
        assert!(server.save_definition(wi, "work").is_err());
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
        let save = server.prepare_definition_save(wi, "a").unwrap();
        assert!(save.question.is_none());
        server.open_definition(&b).unwrap();
        assert_eq!(server.workspaces.len(), 2);
        let error = server.write_definition(wi, "a", &save).unwrap_err().to_string();
        assert!(error.contains("belongs to workspace \"b\""), "{error}");
        assert!(error.contains(&backup.display().to_string()), "{error}");
        assert!(server.save_definition(wi, "a").unwrap_err().to_string().contains("belongs to workspace \"b\""));
        // A workspace that could not restore its panes also owns its backup destination.
        server.unrestored = crate::persist::snapshot(&server).workspaces.into_iter().skip(1).collect();
        server.workspaces.pop();
        assert!(server.save_definition(wi, "a").unwrap_err().to_string().contains("belongs to workspace \"b\""));
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
        let json = serde_json::to_value(crate::persist::snapshot(&old)).unwrap();
        assert!(json["workspaces"].as_array().unwrap().iter().all(|w| w.get("definition_source").is_none()));
        let mut restored = server();
        restored.config.workspace_dirs.push(dir.display().to_string());
        crate::persist::apply(&mut restored, serde_json::from_value(json).unwrap()).unwrap();
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
            error.contains("several open workspaces") && error.contains("alpha") && error.contains("beta"),
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
        assert!(server.prepare_definition_save(wi, "moved").unwrap().question.is_some());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn only_an_unchanged_source_saves_without_confirmation() {
        let dir = directory("revisions");
        let path = dir.join("work.kdl");
        let mut server = server();
        let wi = linked(&mut server, &path, "layout { pane; }");
        server.persist_dirty = false;
        server.save_definition(wi, "work").unwrap();
        assert!(server.persist_dirty);
        let saved = std::fs::read_to_string(&path).unwrap();
        assert_eq!(server.workspaces[wi].definition_source.as_deref(), Some(saved.as_str()));
        assert!(server.prepare_definition_save(wi, "work").unwrap().question.is_none());
        std::fs::write(&path, "invalid external KDL").unwrap();
        server.follow_definitions(&[(path.clone(), workspace::load_file(&path))]);
        assert_eq!(server.workspaces[wi].definition_source.as_deref(), Some(saved.as_str()));
        assert!(server.save_definition(wi, "work").unwrap_err().to_string().contains("File changed"));
        let confirmed = server.prepare_definition_save(wi, "work").unwrap();
        std::fs::write(&path, "another external edit").unwrap();
        assert!(server.write_definition(wi, "work", &confirmed).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "another external edit");
        let confirmed = server.prepare_definition_save(wi, "work").unwrap();
        server.write_definition(wi, "work", &confirmed).unwrap();
        assert_eq!(std::fs::read_to_string(path.with_extension("kdl.bak")).unwrap(), "another external edit");
        std::fs::remove_file(&path).unwrap();
        server.save_definition(wi, "work").unwrap();
        assert!(path.exists());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_confirmed_destination_is_rechecked_for_new_owners() {
        let dir = directory("new-owner");
        let path = dir.join("work.kdl");
        let mut server = server();
        let wi = linked(&mut server, &path, "layout { pane; }");
        let save = server.prepare_definition_save(wi, "work").unwrap();
        server.new_workspace("other".into(), Some(path.clone()));
        assert!(server.write_definition(wi, "work", &save).unwrap_err().to_string().contains("belongs to workspace"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "layout { pane; }");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cli_links_are_marked_for_persistence_and_sources_survive_a_restart() {
        use crate::protocol::{Request, Response};
        let mut server = server();
        let name = format!("w2-cli-link-{}", std::process::id());
        let id = server.new_workspace(name.clone(), None);
        let dir = directory("persistence");
        let preview = dir.join("preview.txt");
        std::fs::write(&preview, "kept").unwrap();
        server
            .spawn_pane_in(
                id,
                None,
                PaneSpec { preview: Some(preview), ..Default::default() },
                crate::protocol::Placement::Tab,
                None,
            )
            .unwrap();
        server.persist_dirty = false;
        let response = server.request(0, Request::SaveWorkspace { workspace: Some(name), name: None });
        let Some(Ok(Response::Path(path))) = response else { panic!("save failed") };
        assert!(server.persist_dirty);
        let snap = crate::persist::snapshot(&server);
        assert_eq!(snap.workspaces[0].definition, Some(workspace::definition_path(&path)));
        let source = snap.workspaces[0].definition_source.clone().unwrap();
        std::fs::write(&path, "layout { pane name=\"edited while stopped\"; }").unwrap();
        let mut json = serde_json::to_value(&snap).unwrap();
        let mut restored = self::server();
        crate::persist::apply(&mut restored, serde_json::from_value(json.clone()).unwrap()).unwrap();
        assert_eq!(restored.workspaces[0].definition_source.as_deref(), Some(source.as_str()));
        let name = restored.definition_file_name(0);
        assert!(restored.save_definition(0, &name).is_err());
        // Old snapshots have no baseline. They load, but cannot silently authorize replacement.
        json["workspaces"][0].as_object_mut().unwrap().remove("definition_source");
        let mut legacy = self::server();
        crate::persist::apply(&mut legacy, serde_json::from_value(json).unwrap()).unwrap();
        assert_eq!(legacy.workspaces.len(), 1);
        assert!(legacy.workspaces[0].definition_source.is_none());
        assert!(legacy.save_definition(0, &name).is_err());
        assert_eq!(server.workspaces[0].id, id);
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_copy_keeps_the_original_link_and_first_save_collision_needs_confirmation() {
        let dir = directory("copy");
        let path = dir.join("work.kdl");
        let mut server = server();
        let wi = linked(&mut server, &path, "layout { pane; }");
        let name = format!("w2-copy-{}", std::process::id());
        let copy = server.save_definition(wi, &name).unwrap();
        assert_eq!(server.workspaces[wi].definition.as_deref(), Some(path.as_path()));
        assert!(server.save_definition(wi, &name).unwrap_err().to_string().contains("Replace file?"));
        let scratch = server.new_workspace("scratch".into(), None);
        let si = server.ws_index(scratch).unwrap();
        assert!(server.save_definition(si, &name).is_err());
        assert!(server.workspaces[si].definition.is_none());
        let save = server.prepare_definition_save(si, &name).unwrap();
        server.write_definition(si, &name, &save).unwrap();
        assert_eq!(server.workspaces[si].definition, Some(workspace::definition_path(&copy)));
        assert!(server.save_definition(wi, &name).unwrap_err().to_string().contains("belongs to workspace"));
        std::fs::remove_file(copy.with_extension("kdl.bak")).unwrap();
        std::fs::remove_file(copy).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
