//! SSH-side features of the server: the known servers, SSH panes, whole groups in a grid,
//! auto-adding servers you connect to, and the SSH picker.

use anyhow::{Context, Result};

use super::ui::{PickAction, PickItem, Picker, PickerKind};
use super::{Server, Tab};
use crate::layout::{Axis, Node, PaneId};
use crate::pane::PaneSpec;
use crate::paths;
use crate::protocol::Placement;
use crate::servers::{self, Inventory, Source};
use crate::workspace::WorkspaceDef;

impl Server {
    pub(super) fn refresh_known(&mut self, defs: &[WorkspaceDef]) {
        let inv = match Inventory::load() {
            Ok(inv) => inv,
            Err(e) => {
                self.warnings.push(format!("{e:#}"));
                Inventory::empty()
            }
        };
        let hosts = if self.config.ssh_config_hosts { super::ssh_config_hosts() } else { Vec::new() };
        self.known_servers = servers::known(&inv, defs, &hosts);
    }

    /// Resolve a name or `user@host[:port]` against the known servers.
    pub fn resolve_server(&self, spec: &str) -> Option<servers::Server> {
        servers::resolve(&self.known_servers, spec)
    }

    /// `ssh` options to reach a server, connection sharing included, without the destination.
    pub fn ssh_options(&self, s: &servers::Server) -> Vec<String> {
        let mut out = Vec::new();
        if self.config.servers.multiplex
            && let Some(path) = paths::ssh_control_path()
        {
            out.extend([
                "-o".to_string(),
                "ControlMaster=auto".to_string(),
                "-o".to_string(),
                format!("ControlPath={}", path.display()),
                "-o".to_string(),
                "ControlPersist=10m".to_string(),
            ]);
        }
        let known = &self.known_servers;
        out.extend(s.ssh_options(&|j| servers::resolve(known, j).map(|js| js.address())));
        out
    }

    /// Full `ssh …` command line for a pane connected to `s`.
    pub fn ssh_command(&self, s: &servers::Server) -> Vec<String> {
        let mut cmd = vec!["ssh".to_string()];
        cmd.extend(self.ssh_options(s));
        cmd.push(s.destination());
        cmd
    }

    /// The server a pane spec connects to, if any.
    pub(super) fn ssh_target_of(&self, spec: &PaneSpec) -> Option<servers::Server> {
        if let Some(name) = &spec.server {
            return self.known_servers.iter().find(|k| &k.server.name == name).map(|k| k.server.clone());
        }
        let cmd = spec.command.first()?;
        if cmd.rsplit('/').next() != Some("ssh") {
            return None;
        }
        let mut s = servers::parse_ssh_args(&spec.command[1..])?;
        if let Some(k) = self.known_servers.iter().find(|k| servers::is_known(std::slice::from_ref(k), &s)) {
            s.name = k.server.name.clone();
        } else {
            s.name = servers::short_name(&s.host);
        }
        Some(s)
    }

    /// The server a pane is connected to right now.
    pub fn pane_server(&self, pane: PaneId) -> Option<servers::Server> {
        self.panes.get(&pane)?.ssh.clone()
    }

    /// Open an SSH pane to `s` in workspace `ws`.
    pub fn open_ssh(
        &mut self,
        ws: u32,
        target: Option<PaneId>,
        s: &servers::Server,
        placement: Placement,
    ) -> Result<PaneId> {
        let spec = PaneSpec {
            command: self.ssh_command(s),
            server: (!s.name.is_empty()).then(|| s.name.clone()),
            ..Default::default()
        };
        let name = if s.name.is_empty() { servers::short_name(&s.host) } else { s.name.clone() };
        self.spawn_pane_in(ws, target, spec, placement, Some(name))
    }

    /// Open every server of a group in a grid, `max_panes_per_tab` per tab, in a workspace named
    /// after the group (switching to it if it is already open).
    pub fn open_group(&mut self, group: &str) -> Result<u32> {
        if let Some(ws) = self.workspaces.iter().find(|w| w.name == group) {
            return Ok(ws.id);
        }
        let members: Vec<servers::Server> = self
            .known_servers
            .iter()
            .filter(|k| k.server.groups.iter().any(|g| g == group))
            .map(|k| k.server.clone())
            .collect();
        anyhow::ensure!(!members.is_empty(), "no server in group {group:?}");
        let per_tab = self.config.servers.max_panes_per_tab.max(1);
        let ws = self.new_workspace(group.to_string(), None);
        let chunks: Vec<&[servers::Server]> = members.chunks(per_tab).collect();
        let many = chunks.len() > 1;
        for (i, chunk) in chunks.into_iter().enumerate() {
            let mut ids = Vec::new();
            for s in chunk {
                let spec =
                    PaneSpec { command: self.ssh_command(s), server: Some(s.name.clone()), ..Default::default() };
                ids.push(self.spawn_pane(Some(s.name.clone()), spec)?);
            }
            let tab_id = self.alloc_id();
            let name = if many { format!("{group} {}", i + 1) } else { group.to_string() };
            let mut tab = Tab::new(tab_id, name);
            tab.root = Some(grid(&ids));
            tab.set_focus(ids[0]);
            let wi = self.ws_index(ws).context("workspace vanished")?;
            self.workspaces[wi].tabs.push(tab);
        }
        self.relayout();
        Ok(ws)
    }

    /// An `ssh` process appeared in a pane: remember its server, and add it to the inventory
    /// if kraal does not know it.
    pub(super) fn observe_ssh(&mut self, pane: PaneId, args: &[String]) {
        let Some(mut s) = servers::parse_ssh_args(args) else { return };
        let known = servers::is_known(&self.known_servers, &s);
        if let Some(k) = self.known_servers.iter().find(|k| servers::is_known(std::slice::from_ref(k), &s)) {
            s.name = k.server.name.clone();
        }
        if let Some(p) = self.panes.get_mut(&pane)
            && p.ssh.as_ref().is_none_or(|old| old.destination() != s.destination())
        {
            p.ssh = Some(s.clone());
        }
        if known || !self.config.servers.auto_add || !self.discovered.insert(s.address()) {
            return;
        }
        s.groups = vec!["discovered".into()];
        let result = Inventory::load().and_then(|mut inv| {
            let name = inv.add(s.clone())?;
            inv.save()?;
            Ok(name)
        });
        match result {
            Ok(name) => {
                let defs: Vec<WorkspaceDef> = self.definitions().into_iter().filter_map(|(_, d)| d.ok()).collect();
                self.refresh_known(&defs);
                if let Some(p) = self.panes.get_mut(&pane)
                    && let Some(ssh) = &mut p.ssh
                {
                    ssh.name = name.clone();
                }
                let ids: Vec<super::ClientId> = self.clients.keys().copied().collect();
                for c in ids {
                    self.toast(c, format!("new server saved as \"{name}\" (group discovered) · ctrl+b s to see it"));
                }
            }
            Err(e) => self.warnings.push(format!("could not save server {}: {e:#}", s.address())),
        }
    }

    pub(super) fn ssh_picker(&self) -> Picker {
        let mut items = Vec::new();
        for (g, n) in servers::groups(&self.known_servers) {
            items.push(PickItem {
                label: format!("▦ {g}"),
                detail: format!("group · {n} server{} · enter opens them all", if n == 1 { "" } else { "s" }),
                action: PickAction::SshGroup(g),
                state: None,
                dim: false,
            });
        }
        for k in &self.known_servers {
            let s = &k.server;
            let source = match &k.source {
                Source::Inventory => String::new(),
                Source::Workspace(w) => format!(" · from {w}"),
                Source::SshConfig => " · ~/.ssh/config".to_string(),
            };
            let groups = if s.groups.is_empty() { String::new() } else { format!(" · {}", s.groups.join(", ")) };
            let detail = if s.name == s.host {
                format!("{}{source}", groups.trim_start_matches(" · "))
            } else {
                format!("{}{groups}{source}", s.address())
            };
            items.push(PickItem {
                label: s.name.clone(),
                detail,
                action: PickAction::Ssh(s.name.clone()),
                state: None,
                dim: k.source == Source::SshConfig,
            });
        }
        Picker::new(
            PickerKind::Ssh,
            "Servers  (enter connect · ctrl+t new tab · ctrl+f files · ctrl+e edit list · ctrl+x forget · type user@host)",
            items,
        )
    }

    /// Remove a server from the inventory file.
    pub(super) fn forget_server(&mut self, name: &str) -> Result<bool> {
        let mut inv = Inventory::load()?;
        let removed = inv.remove(name);
        if removed {
            inv.save()?;
            let defs: Vec<WorkspaceDef> = self.definitions().into_iter().filter_map(|(_, d)| d.ok()).collect();
            self.refresh_known(&defs);
        }
        Ok(removed)
    }
}

/// Tile panes in a grid: rows of up to `cols` panes, columns wider than tall like terminal cells.
pub fn grid(ids: &[PaneId]) -> Node {
    let n = ids.len();
    if n == 1 {
        return Node::Leaf(ids[0]);
    }
    let cols = ((n as f64 * 1.5).sqrt().ceil() as usize).clamp(1, n);
    let rows: Vec<Node> = ids
        .chunks(cols)
        .map(|row| {
            if row.len() == 1 {
                Node::Leaf(row[0])
            } else {
                Node::Split { axis: Axis::Row, children: row.iter().map(|id| (Node::Leaf(*id), 1.0)).collect() }
            }
        })
        .collect();
    if rows.len() == 1 {
        return rows.into_iter().next().unwrap();
    }
    Node::Split { axis: Axis::Column, children: rows.into_iter().map(|r| (r, 1.0)).collect() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grids() {
        assert_eq!(grid(&[1]), Node::Leaf(1));
        let g = grid(&[1, 2, 3, 4]);
        let Node::Split { axis: Axis::Column, children } = &g else { panic!("{g:?}") };
        assert_eq!(children.len(), 2);
        let g = grid(&(1..=9).collect::<Vec<_>>());
        assert_eq!(g.leaves(), (1..=9).collect::<Vec<_>>());
        let Node::Split { children, .. } = &g else { panic!() };
        assert_eq!(children.len(), 3);
        let g = grid(&[1, 2]);
        assert!(matches!(g, Node::Split { axis: Axis::Row, .. }));
    }
}
