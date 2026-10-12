//! Panes and workspaces coming and going: spawning, exiting, closing.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use super::{FloatRect, Floating, Server, SyncScope, Tab, Workspace};
use crate::layout::{Axis, Node, PaneId};
use crate::pane::{Pane, PaneSpec, ProcessHold};
use crate::protocol;
impl Server {
    pub fn new_workspace(&mut self, name: String, definition: Option<PathBuf>) -> u32 {
        let id = self.alloc_id();
        let mut name = name;
        let mut n = 2;
        let base = name.clone();
        while self.workspaces.iter().any(|w| w.name == name) {
            name = format!("{base}-{n}");
            n += 1;
        }
        let mut ws = Workspace {
            id,
            name,
            tabs: Vec::new(),
            active: 0,
            definition: None,
            definition_inode: None,
            cwd: None,
            sync: SyncScope::Off,
            marked: Default::default(),
            sync_approved: Default::default(),
        };
        ws.set_definition(definition.as_deref());
        self.workspaces.push(ws);
        self.persist_dirty = true;
        let name = self.workspaces.last().map(|w| w.name.clone()).unwrap_or_default();
        self.emit("workspace.opened", serde_json::json!({ "workspace": name }));
        id
    }

    /// Spawn a pane process with a provisional size; call [`Server::relayout`] afterwards.
    pub fn spawn_pane(&mut self, name: Option<String>, spec: PaneSpec) -> Result<PaneId> {
        let mut spec = spec;
        self.add_worktree_env(&mut spec);
        let id = self.alloc_id();
        let inner = self.inner(self.layout_area);
        if let Some([left, right]) = spec.files.clone() {
            let fm = self.file_manager(&left, &right);
            self.panes.insert(id, Pane::files(id, name, spec, fm));
            self.persist_dirty = true;
            return Ok(id);
        }
        let ssh = self.ssh_target_of(&spec);
        let mut pane = match Pane::spawn(id, name.clone(), spec.clone(), inner.width, inner.height, &self.env, &self.tx)
        {
            Ok(p) => p,
            // A command (or a shell with input to type, an agent to resume) that cannot start
            // still gets its pane, so the layout stays whole and Enter can retry once the
            // problem is fixed.
            Err(e) if !spec.command.is_empty() || spec.startup_input.is_some() => {
                let reason = format!("{e:#}");
                let reason = reason.lines().next().unwrap_or("cannot start").to_string();
                return self.spawn_pane_suspended(name, spec, &format!("cannot start: {reason}"));
            }
            Err(e) => return Err(e),
        };
        pane.ssh = ssh;
        self.panes.insert(id, pane);
        self.persist_dirty = true;
        Ok(id)
    }

    /// A command pane that does not run its command yet: it shows `reason`, and Enter starts it.
    pub fn spawn_pane_suspended(&mut self, name: Option<String>, spec: PaneSpec, reason: &str) -> Result<PaneId> {
        let mut spec = spec;
        self.add_worktree_env(&mut spec);
        let id = self.alloc_id();
        let inner = self.inner(self.layout_area);
        let mut pane =
            Pane::spawn(id, name, placeholder(&spec, reason), inner.width, inner.height, &self.env, &self.tx)?;
        pane.ssh = self.ssh_target_of(&spec);
        pane.spec = spec;
        pane.suspended = true;
        self.panes.insert(id, pane);
        self.persist_dirty = true;
        Ok(id)
    }

    /// Run `f` with the programs of the panes it sets up held, then start them (see
    /// `crate::pane::batch`): the panes of a workspace are all set up first, instead of the
    /// server competing for the processors with the programs it started already.
    pub(super) fn with_batch<T>(&mut self, f: impl FnOnce(&mut Server) -> T) -> T {
        let batch = crate::pane::batch();
        let result = f(self);
        drop(batch);
        // The outermost batch starts them all.
        if !crate::pane::batching() {
            self.launch_pending();
        }
        result
    }

    /// Start the programs of the panes set up during a batch, in the order the panes were made,
    /// off this thread (see `crate::pane::launcher`).
    fn launch_pending(&mut self) {
        let mut ids: Vec<PaneId> = self.panes.keys().copied().collect();
        ids.sort_unstable();
        for id in ids {
            let Some(pending) = self.panes.get_mut(&id).and_then(Pane::take_pending) else { continue };
            // Without the launcher thread, here.
            if let Err(e) = self.launches.send(pending) {
                e.into_inner().start();
            }
        }
    }

    /// The program of pane `id` (`process`), started after the pane was set up, could not start
    /// (`why`). Unless the pane is gone or runs another program since, it waits for Enter,
    /// saying why, as one whose program could not start when it was made does.
    pub(super) fn pane_failed(&mut self, id: PaneId, process: &ProcessHold, why: &str) {
        let Some(pane) = self.panes.get_mut(&id) else { return };
        if !pane.process_hold().is_some_and(|p| p.is(process)) {
            return;
        }
        let reason = format!("cannot start: {}", why.lines().next().unwrap_or("cannot start"));
        // A prelude (a worktree's setup) that could not start: Enter starts the command.
        pane.preparing = false;
        let spec = pane.spec.clone();
        if let Err(e) = self.suspend_pane(id, spec, &reason) {
            self.warnings.push(format!("{reason}; {e:#}"));
        }
        self.mark_dirty();
    }

    /// Make pane `id` wait for Enter to run `spec`, saying why (`reason`).
    pub fn suspend_pane(&mut self, id: PaneId, spec: PaneSpec, reason: &str) -> Result<()> {
        let ssh = self.ssh_target_of(&spec);
        let pane = self.panes.get_mut(&id).context("no such pane")?;
        pane.spec = placeholder(&spec, reason);
        pane.respawn(&self.env, &self.tx)?;
        pane.ssh = ssh;
        pane.spec = spec;
        pane.suspended = true;
        self.persist_dirty = true;
        Ok(())
    }

    /// A pane in workspace `ws` that runs `prelude` (a new worktree's setup) before its own
    /// command, `spec`: the command starts in its place once the prelude succeeds, and waits for
    /// Enter when it fails.
    pub fn spawn_pane_after(
        &mut self,
        ws: u32,
        prelude: PaneSpec,
        spec: PaneSpec,
        placement: crate::protocol::Placement,
        name: Option<String>,
    ) -> Result<PaneId> {
        let id = self.spawn_pane_in(ws, None, prelude, placement, name)?;
        let mut spec = spec;
        self.add_worktree_env(&mut spec);
        if let Some(p) = self.panes.get_mut(&id) {
            // A prelude that could not start waits for Enter: Enter starts the command instead.
            p.preparing = !p.suspended;
            p.spec = spec;
        }
        Ok(id)
    }

    /// Spawn a pane and place it in workspace `ws` relative to `target` (default: the focused pane).
    pub fn spawn_pane_in(
        &mut self,
        ws: u32,
        target: Option<PaneId>,
        spec: PaneSpec,
        placement: crate::protocol::Placement,
        name: Option<String>,
    ) -> Result<PaneId> {
        use crate::protocol::Placement;
        let wi = self.ws_index(ws).context("no such workspace")?;
        let mut spec = spec;
        if spec.cwd.is_none() {
            let from =
                target.or_else(|| self.workspaces[wi].tabs.get(self.workspaces[wi].active).and_then(|t| t.focus));
            spec.cwd = from
                .and_then(|p| self.panes.get(&p))
                .and_then(|p| p.current_cwd())
                .or_else(|| self.workspaces[wi].cwd.clone());
        }
        let id = self.spawn_pane(name, spec)?;
        let area = self.layout_area;
        let ws = &mut self.workspaces[wi];
        let new_tab = ws.tabs.is_empty() || placement == Placement::Tab;
        if new_tab {
            let tab_id = self.next_id;
            self.next_id += 1;
            ws.tabs.push(Tab::new(tab_id, String::new()));
            ws.active = ws.tabs.len() - 1;
        }
        // A new tab is where the pane goes; otherwise the target's tab, else the active one.
        let found = if new_tab { None } else { target.and_then(|t| ws.tabs.iter().position(|tab| tab.contains(t))) };
        let tab = match found {
            Some(ti) => {
                ws.active = ti;
                &mut ws.tabs[ti]
            }
            None => ws.tab_mut(),
        };
        let target = target.filter(|t| tab.contains(*t)).or(tab.focus);
        if placement == Placement::Floating {
            let n = tab.floating.len() as f32;
            let off = (n * 0.03).min(0.15);
            tab.floating.push(Floating { pane: id, rect: FloatRect { x: 0.15 + off, y: 0.12 + off, w: 0.7, h: 0.7 } });
            tab.show_floating = true;
        } else {
            match (&mut tab.root, target.filter(|t| !tab.floating.iter().any(|f| f.pane == *t))) {
                (None, _) => tab.root = Some(Node::Leaf(id)),
                (Some(root), Some(t)) => {
                    let axis = match placement {
                        Placement::Right => Axis::Row,
                        Placement::Down => Axis::Column,
                        _ => {
                            let rects = root.layout(area);
                            let r = rects.get(&root.holder(t)).copied().unwrap_or(area);
                            // Cells are about twice as tall as wide.
                            if r.width >= r.height * 2 { Axis::Row } else { Axis::Column }
                        }
                    };
                    // A new pane next to a stacked one goes in its stack; a split by hand
                    // splits the stack's place.
                    let split = matches!(placement, Placement::Right | Placement::Down);
                    if split || !root.join(t, id) {
                        root.split(t, axis, id);
                    }
                    if split {
                        tab.preset = None;
                    }
                }
                (Some(root), None) => {
                    let first = root.leaves()[0];
                    root.split(first, Axis::Row, id);
                }
            }
            // The arrangement the tab keeps takes the new pane in.
            if let Some(preset) = tab.preset {
                tab.focus = Some(id);
                tab.arrange(preset);
            }
            tab.zoom = None;
        }
        tab.set_focus(id);
        self.relayout();
        Ok(id)
    }

    /// The process of pane `id` ended, and its last output was read (see `finish_exits`).
    pub(super) fn pane_exited(&mut self, id: PaneId, pid: Option<u32>, code: Option<i32>) {
        let Some(pane) = self.panes.get_mut(&id) else { return };
        if pid.is_some() && pane.pid() != pid {
            return; // an earlier process of a respawned pane
        }
        if self.switches.contains_key(&id) {
            // Ended first: if the switch cannot go on, Enter starts it again on its account.
            if let Some(pane) = self.panes.get_mut(&id) {
                pane.exit_code = Some(code);
                let session = pane.agent.session_id.take();
                pane.agent = crate::agent::AgentTracker { session_id: session, ..Default::default() };
            }
            self.finish_switch(id);
            self.mark_dirty();
            return;
        }
        let Some(pane) = self.panes.get_mut(&id) else { return };
        if pane.preparing {
            pane.preparing = false;
            let result = if code == Some(0) {
                pane.respawn(&self.env, &self.tx)
            } else {
                let spec = pane.spec.clone();
                self.suspend_pane(id, spec, "the worktree's setup did not succeed; Enter starts this anyway")
            };
            if let Err(e) = result {
                self.warnings.push(format!("{e:#}"));
            }
            self.mark_dirty();
            self.relayout();
            return;
        }
        let Some(pane) = self.panes.get_mut(&id) else { return };
        if let Some(spec) = pane.restart_with.take() {
            pane.spec = spec;
            if let Err(e) = pane.respawn(&self.env, &self.tx) {
                self.warnings.push(format!("{e:#}"));
            }
            self.limit_alerts.remove(&id);
            self.relayout();
            return;
        }
        // A suspended pane's placeholder ending is not its command ending.
        if !pane.suspended {
            self.announce_pane(id);
            let Some(pane) = self.panes.get(&id) else { return };
            let command = pane.spec.command.clone();
            let fields = serde_json::json!({ "code": code, "command": command });
            self.emit_pane("pane.exited", id, fields.clone());
            if code.is_some_and(|c| c != 0) {
                self.emit_pane("pane.failed", id, fields);
            }
        }
        let Some(pane) = self.panes.get_mut(&id) else { return };
        // Quitting with ctrl+c (130) is a normal end for a tool, not a failure to show.
        let failed = !matches!(code, Some(0) | Some(130) | None);
        let keep = pane.suspended
            || (!pane.spec.command.is_empty() && (!pane.spec.close_on_exit || (pane.spec.keep_on_failure && failed)));
        if keep {
            pane.exit_code = Some(code);
            // The conversation's id outlives the process: restarting resumes it.
            let session = pane.agent.session_id.take();
            pane.agent = crate::agent::AgentTracker { session_id: session, ..Default::default() };
            let job = pane.spec.close_on_success;
            if code == Some(0) && job {
                pane.close_at = Some(Instant::now() + Duration::from_secs(3));
            }
            if job {
                self.refresh_file_managers();
            }
            self.mark_dirty();
            self.check_waits();
            self.runbook_pane_exited(id, code);
        } else {
            self.close_pane(id);
        }
    }

    /// Close a pane and clean up empty tabs and workspaces.
    pub fn close_pane(&mut self, id: PaneId) {
        if !self.panes.contains_key(&id) {
            return;
        }
        self.announce_pane(id);
        let fields = self.pane_fields(id);
        self.end_pane_states(id, &fields);
        let Some(mut pane) = self.panes.remove(&id) else { return };
        pane.kill();
        self.panes_closed = true;
        if let Some((wi, ti)) = self.workspace_of(id) {
            let ws = &mut self.workspaces[wi];
            ws.marked.remove(&id);
            if ws.sync == SyncScope::Marked && ws.marked.is_empty() {
                ws.sync = SyncScope::Off;
            }
            if !ws.tabs[ti].remove(id) {
                ws.tabs.remove(ti);
                // The tab shown stays shown when an earlier one goes; when it is the one that
                // goes, the next one (or the last) is shown.
                if ti < ws.active || ws.active >= ws.tabs.len() {
                    ws.active = ws.active.saturating_sub(1);
                }
            }
            if ws.tabs.is_empty() {
                self.remove_workspace(wi);
            }
        }
        self.persist_dirty = true;
        self.emit("pane.closed", serde_json::Value::Object(fields));
        self.runbook_pane_closed(id);
        // The session ends with its last pane; the server tells its clients as it stops.
        if self.workspaces.is_empty() {
            // Nothing is left to restore, unless workspaces this server could not restore are.
            if self.unrestored.is_empty() {
                let _ = self.persister.send(super::persistence::Persist::Delete(self.session.clone()));
                self.persisted = None;
                self.persist_dirty = false;
            }
            self.quitting = true;
            return;
        }
        self.check_waits();
        self.relayout();
    }

    /// Remove workspace `wi`, moving the clients that showed it to a neighbour.
    fn remove_workspace(&mut self, wi: usize) {
        let removed = self.workspaces.remove(wi);
        self.emit("workspace.closed", serde_json::json!({ "workspace": removed.name }));
        let fallback = self.workspaces.get(wi.saturating_sub(1)).or(self.workspaces.first()).map(|w| w.id);
        for c in self.clients.values_mut() {
            if c.workspace == removed.id {
                c.workspace = fallback.unwrap_or(0);
            }
        }
        self.persist_dirty = true;
    }

    /// Remove the workspaces left without tabs because what was to fill them could not start:
    /// a workspace always has a tab to show.
    pub fn drop_empty_workspaces(&mut self) {
        while let Some(wi) = self.workspaces.iter().position(|w| w.tabs.is_empty()) {
            self.remove_workspace(wi);
        }
    }

    pub fn close_workspace(&mut self, ws: u32) {
        let Some(wi) = self.ws_index(ws) else { return };
        for id in self.workspaces[wi].panes() {
            self.close_pane(id);
        }
    }

    pub fn pane_label(&self, pane: PaneId) -> String {
        let name = self.panes.get(&pane).map(|p| p.display_name()).unwrap_or_default();
        match self.workspace_of(pane) {
            Some((wi, _)) => format!("{} › {name}", self.workspaces[wi].name),
            None => name,
        }
    }

    pub fn pane_info(&self, id: PaneId) -> protocol::PaneInfo {
        let p = &self.panes[&id];
        let (ws, tab, floating, focused) = match self.workspace_of(id) {
            Some((wi, ti)) => {
                let w = &self.workspaces[wi];
                let t = &w.tabs[ti];
                (w.name.clone(), self.tab_label(t), t.is_floating(id), t.focus == Some(id) && w.active == ti)
            }
            None => (String::new(), String::new(), false, false),
        };
        protocol::PaneInfo {
            id,
            name: p.name.clone(),
            title: p.title.clone(),
            workspace: ws,
            tab,
            kind: match &p.content {
                crate::pane::Content::Preview(pv) => format!("preview:{:?}", pv.kind()).to_lowercase(),
                crate::pane::Content::Files(_) => "files".into(),
                crate::pane::Content::Term(_) => "terminal".into(),
            },
            command: p.spec.command.clone(),
            cwd: p.current_cwd(),
            agent: p.agent.kind.clone(),
            state: p.agent.state,
            exit_code: if p.suspended { None } else { p.exit_code.map(|c| c.unwrap_or(-1)) },
            suspended: p.suspended,
            keyboard: p.input_modes().kitty_flags,
            server: p.ssh.as_ref().map(|s| s.name.clone()),
            session: None,
            production: self.pane_production_shown(id),
            contexts: self.context_labels(id).into_iter().map(|(l, _)| l).collect(),
            focused,
            floating,
            cols: p.cols,
            rows: p.rows,
        }
    }

    /// A tab's name: the one it was given, else what its focused pane shows.
    pub fn tab_label(&self, tab: &Tab) -> String {
        if !tab.name.is_empty() {
            return tab.name.clone();
        }
        tab.focus
            .or_else(|| tab.panes().first().copied())
            .and_then(|p| self.panes.get(&p))
            .map(|p| p.display_name())
            .unwrap_or_else(|| "shell".into())
    }
}

/// What a suspended pane shows until Enter runs `spec`: why it waits, and the command.
fn placeholder(spec: &PaneSpec, reason: &str) -> PaneSpec {
    let line = crate::shell::quote(&spec.command.join(" "));
    PaneSpec {
        command: vec![
            "sh".into(),
            "-c".into(),
            format!(
                "printf '\\033[2m%s\\n\\033[0m$ %s\\n' {} {line}",
                crate::shell::quote(&format!("kraal: {reason}"))
            ),
        ],
        cwd: spec.cwd.clone().filter(|c| c.is_dir()),
        ..Default::default()
    }
}
