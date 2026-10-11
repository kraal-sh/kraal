//! SSH-side features of the server: the known servers, SSH panes, whole groups in a grid,
//! auto-adding servers you connect to, and the SSH picker.

use std::path::{Path, PathBuf};

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
        servers::ssh_args(s, &self.known_servers, self.config.servers.multiplex)
    }

    /// Full `ssh …` command line for a pane connected to `s`.
    pub fn ssh_command(&self, s: &servers::Server) -> Vec<String> {
        self.ssh_command_running(s, &[])
    }

    /// `ssh … host` running `remote` (with a terminal, for interactive or following commands).
    pub fn ssh_command_running(&self, s: &servers::Server, remote: &[String]) -> Vec<String> {
        let mut cmd = vec!["ssh".to_string()];
        cmd.extend(self.ssh_options(s));
        if !remote.is_empty() {
            cmd.push("-t".into());
        }
        cmd.push(s.destination());
        if !remote.is_empty() {
            cmd.push(remote.join(" "));
        }
        cmd
    }

    /// The server a pane spec connects to, if any: the connection kraal made, else the entry
    /// it is named after, else what its `ssh` command line says.
    pub(super) fn ssh_target_of(&self, spec: &PaneSpec) -> Option<servers::Server> {
        if let Some(c) = &spec.connection {
            return Some(c.clone());
        }
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
        self.open_ssh_running(ws, target, s, placement, &[])
    }

    /// Open an SSH pane to `s` running `remote` (a shell when empty).
    pub fn open_ssh_running(
        &mut self,
        ws: u32,
        target: Option<PaneId>,
        s: &servers::Server,
        placement: Placement,
        remote: &[String],
    ) -> Result<PaneId> {
        let spec = PaneSpec {
            command: self.ssh_command_running(s, remote),
            server: (!s.name.is_empty()).then(|| s.name.clone()),
            connection: Some(s.clone()),
            ..Default::default()
        };
        let name = if s.name.is_empty() { servers::short_name(&s.host) } else { s.name.clone() };
        self.spawn_pane_in(ws, target, spec, placement, Some(name))
    }

    /// Open every server of a group in a grid, `max_panes_per_tab` per tab, in a workspace named
    /// after the group (switching to it if it is already open).
    pub fn open_group(&mut self, group: &str, remote: &[String]) -> Result<u32> {
        let ws_name = group_workspace_name(group, remote);
        if let Some(ws) = self.workspaces.iter().find(|w| w.name == ws_name) {
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
        let ws = self.new_workspace(ws_name, None);
        let chunks: Vec<&[servers::Server]> = members.chunks(per_tab).collect();
        let many = chunks.len() > 1;
        for (i, chunk) in chunks.into_iter().enumerate() {
            let mut ids = Vec::new();
            for s in chunk {
                let spec = PaneSpec {
                    command: self.ssh_command_running(s, remote),
                    server: Some(s.name.clone()),
                    connection: Some(s.clone()),
                    ..Default::default()
                };
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
        // The ssh kraal started in this pane is the connection it made, with what its command
        // line only spells (a jump host with its own key is a ProxyCommand there).
        let own = self.panes.get(&pane).filter(|p| p.spec.command.get(1..) == Some(args)).map(|p| p.spec.clone());
        let Some(mut s) = own.and_then(|spec| self.ssh_target_of(&spec)).or_else(|| servers::parse_ssh_args(args))
        else {
            return;
        };
        let known = servers::is_known(&self.known_servers, &s);
        if let Some(k) = self.known_servers.iter().find(|k| servers::is_known(std::slice::from_ref(k), &s)) {
            s.name = k.server.name.clone();
        }
        if let Some(p) = self.panes.get_mut(&pane)
            && p.ssh.as_ref().is_none_or(|old| !old.same_connection(&s))
        {
            p.ssh = Some(s.clone());
        }
        if known || !self.config.servers.auto_add || !self.discovered.insert(s.address()) {
            return;
        }
        s.groups = vec!["discovered".into()];
        let address = s.address();
        // Off the event loop: another kraal may hold the inventory's lock, and the write syncs.
        let observed = s.clone();
        self.background(
            move || Inventory::update(|inv| inv.add(s)),
            move |server, result| match result {
                Ok(name) => {
                    let defs: Vec<WorkspaceDef> =
                        server.definitions().into_iter().filter_map(|(_, d)| d.ok()).collect();
                    server.refresh_known(&defs);
                    // The pane may have moved on to another server meanwhile.
                    if let Some(p) = server.panes.get_mut(&pane)
                        && let Some(ssh) = &mut p.ssh
                        && ssh.same_connection(&observed)
                    {
                        ssh.name = name.clone();
                    }
                    let ids: Vec<super::ClientId> = server.clients.keys().copied().collect();
                    let see = match server.keymap.hint(crate::keymap::Action::SshPicker) {
                        Some(k) => format!(" · {k} to see it"),
                        None => String::new(),
                    };
                    for c in ids {
                        server.toast(c, format!("new server saved as \"{name}\" (group discovered){see}"));
                    }
                }
                Err(e) => server.warnings.push(format!("could not save server {address}: {e:#}")),
            },
        );
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
            let mut detail = if s.name == s.host {
                format!("{}{source}", groups.trim_start_matches(" · "))
            } else {
                format!("{}{groups}{source}", s.address())
            };
            if crate::context::server_production(s, &self.config.production.words) {
                detail = format!("PROD · {detail}").trim_end_matches(" · ").to_string();
            }
            items.push(PickItem {
                label: s.name.clone(),
                detail,
                action: PickAction::Ssh(s.name.clone()),
                state: None,
                dim: k.source == Source::SshConfig,
            });
        }
        Picker::new(PickerKind::Ssh, "Connect to a server", items)
    }

    /// The production servers among `target` (a server name or `@group`).
    pub(super) fn production_servers(&self, target: &str) -> Vec<String> {
        let words = &self.config.production.words;
        self.known_servers
            .iter()
            .filter(|k| match target.strip_prefix('@') {
                Some(g) => k.server.groups.iter().any(|x| x == g),
                None => k.server.name == target,
            })
            .filter(|k| crate::context::server_production(&k.server, words))
            .map(|k| k.server.name.clone())
            .collect()
    }

    /// Remove a server from the inventory file.
    pub(super) fn forget_server(&mut self, name: &str) -> Result<bool> {
        let removed = Inventory::update(|inv| Ok(inv.remove(name)))?;
        if removed {
            let defs: Vec<WorkspaceDef> = self.definitions().into_iter().filter_map(|(_, d)| d.ok()).collect();
            self.refresh_known(&defs);
        }
        Ok(removed)
    }
}

/// The workspace of a group running `remote`: `journalctl -fu nginx` on @prod gets its own,
/// next to the plain shells. The whole command names it (where it is shown, it is cut to fit):
/// two commands that differ only at their end are two workspaces.
fn group_workspace_name(group: &str, remote: &[String]) -> String {
    if remote.is_empty() { group.to_string() } else { format!("{group}: {}", remote.join(" ")) }
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

/// Hosts from `~/.ssh/config` (no wildcards).
pub fn ssh_config_hosts() -> Vec<String> {
    let Some(home) = dirs::home_dir() else { return Vec::new() };
    let mut out = Vec::new();
    read_ssh_config(&home.join(".ssh/config"), &mut out, 0);
    out.sort();
    out.dedup();
    out
}

fn read_ssh_config(path: &Path, out: &mut Vec<String>, depth: u8) {
    let Ok(text) = std::fs::read_to_string(path) else { return };
    for line in text.lines() {
        let line = line.trim();
        let mut parts = line.split_whitespace();
        let Some(key) = parts.next() else { continue };
        if key.eq_ignore_ascii_case("host") {
            out.extend(parts.filter(|h| !h.contains(['*', '?', '!'])).map(String::from));
        } else if key.eq_ignore_ascii_case("include") && depth < 3 {
            for pat in parts {
                let p = paths::expand_tilde(pat);
                let p = if p.is_relative() { dirs::home_dir().unwrap_or_default().join(".ssh").join(p) } else { p };
                // Support a trailing `*` glob, the common case (`Include config.d/*`).
                if let Some(dir) = p.to_str().and_then(|s| s.strip_suffix("/*")) {
                    if let Ok(rd) = std::fs::read_dir(dir) {
                        let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
                        files.sort();
                        for f in files {
                            read_ssh_config(&f, out, depth + 1);
                        }
                    }
                } else {
                    read_ssh_config(&p, out, depth + 1);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_commands_get_a_workspace_each() {
        let name = |cmd: &str| group_workspace_name("prod", &[cmd.to_string()]);
        let march = name("journalctl -fu nginx --since=2020-03-01");
        assert_eq!(march, "prod: journalctl -fu nginx --since=2020-03-01");
        assert_ne!(march, name("journalctl -fu nginx --since=2020-04-01"));
        assert_eq!(group_workspace_name("prod", &[]), "prod");
    }

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
