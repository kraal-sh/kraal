//! Control requests from the CLI and from agents (`kraal run`, `kraal read`, hooks, …).

use std::time::Duration;

use super::{ClientId, Server};
use crate::agent::AgentState;
use crate::input::KeySpec;
use crate::layout::PaneId;
use crate::pane::PaneSpec;
use crate::protocol::{Request, Response, WorkspaceInfo};

type Reply = Option<Result<Response, String>>;

impl Server {
    /// Handle one request. `None` means the answer comes later (waits).
    pub(super) fn request(&mut self, client: ClientId, req: Request) -> Reply {
        Some(match req {
            Request::Ping => Ok(Response::Pong {
                pid: std::process::id(),
                session: self.session.clone(),
                version: env!("CARGO_PKG_VERSION").into(),
            }),
            Request::ListPanes => {
                let mut v: Vec<PaneId> = Vec::new();
                for ws in &self.workspaces {
                    v.extend(ws.panes());
                }
                Ok(Response::Panes(v.into_iter().map(|p| self.pane_info(p)).collect()))
            }
            Request::ListWorkspaces => Ok(Response::Workspaces(self.workspace_infos())),
            Request::OpenWorkspace { name } => {
                self.open_workspace(&name).map(|_| Response::Ok).map_err(|e| format!("{e:#}"))
            }
            Request::NewPane { target, command, cwd, name, placement, preview, focus, close_on_exit } => {
                let target = match target.as_deref().map(|t| self.resolve(t)).transpose() {
                    Ok(t) => t,
                    Err(e) => return Some(Err(e)),
                };
                let ws = match target.and_then(|t| self.workspace_of(t)) {
                    Some((wi, _)) => self.workspaces[wi].id,
                    None => match self
                        .primary
                        .and_then(|p| self.clients.get(&p))
                        .map(|c| c.workspace)
                        .or_else(|| self.workspaces.first().map(|w| w.id))
                    {
                        Some(ws) => ws,
                        None => self.new_workspace("main".into(), None),
                    },
                };
                let previous = match target.and_then(|t| self.workspace_of(t)) {
                    Some((wi, ti)) => self.workspaces[wi].tabs[ti].focus,
                    None => self
                        .ws_index(ws)
                        .and_then(|wi| self.workspaces[wi].tabs.get(self.workspaces[wi].active))
                        .and_then(|t| t.focus),
                };
                let spec = PaneSpec { command, cwd, preview, close_on_exit, ..Default::default() };
                match self.spawn_pane_in(ws, target, spec, placement, name) {
                    Ok(id) => {
                        if !focus
                            && let Some(prev) = previous
                            && let Some((wi, ti)) = self.workspace_of(prev)
                        {
                            self.workspaces[wi].tabs[ti].set_focus(prev);
                        }
                        self.mark_dirty();
                        Ok(Response::Pane(self.pane_info(id)))
                    }
                    Err(e) => Err(format!("{e:#}")),
                }
            }
            Request::ClosePane { pane } => self.resolve(&pane).map(|p| {
                self.close_pane(p);
                Response::Ok
            }),
            Request::FocusPane { pane } => self.resolve(&pane).map(|p| {
                match self.primary {
                    Some(c) => self.reveal(c, p),
                    None => {
                        if let Some((wi, ti)) = self.workspace_of(p) {
                            self.workspaces[wi].active = ti;
                            self.workspaces[wi].tabs[ti].set_focus(p);
                        }
                    }
                }
                Response::Ok
            }),
            Request::RenamePane { pane, name } => self.resolve(&pane).map(|p| {
                if let Some(pn) = self.panes.get_mut(&p) {
                    pn.name = (!name.is_empty()).then_some(name);
                }
                self.mark_persist();
                self.mark_dirty();
                Response::Ok
            }),
            Request::SendText { pane, text } => self.resolve(&pane).and_then(|p| {
                let pn = self.panes.get_mut(&p).ok_or("no such pane")?;
                if pn.exit_code.is_some() {
                    return Err("the pane's program has exited".into());
                }
                pn.write_input(text.into_bytes());
                Ok(Response::Ok)
            }),
            Request::SendKeys { pane, keys } => self.resolve(&pane).and_then(|p| {
                let mut specs = Vec::new();
                for k in &keys {
                    specs.push(KeySpec::parse(k).map_err(|e| format!("{k:?}: {e}"))?);
                }
                let pn = self.panes.get_mut(&p).ok_or("no such pane")?;
                let modes = pn.input_modes();
                let mut bytes = Vec::new();
                for s in specs {
                    bytes.extend(crate::input::encode_key_code(s.code, s.mods, modes));
                }
                pn.write_input(bytes);
                Ok(Response::Ok)
            }),
            Request::ReadPane { pane, lines } => {
                self.resolve(&pane).map(|p| Response::Text(self.panes[&p].text(lines)))
            }
            Request::Wait { pane, until, timeout_ms } => match self.resolve(&pane) {
                Ok(p) => {
                    let until = if until.is_empty() {
                        vec!["blocked".into(), "done".into(), "idle".into(), "exit".into()]
                    } else {
                        until
                    };
                    for u in &until {
                        if !matches!(u.as_str(), "exit" | "quiet") && AgentState::parse(u).is_none() {
                            return Some(Err(format!(
                                "unknown condition {u:?} (use working, blocked, done, idle, exit, quiet)"
                            )));
                        }
                    }
                    self.add_wait(client, p, until, timeout_ms.map(Duration::from_millis));
                    return None;
                }
                Err(e) => Err(e),
            },
            Request::AgentReport { pane, agent, state, session_id, message } => self.resolve(&pane).map(|p| {
                let before = self.panes[&p].agent.state;
                let pn = self.panes.get_mut(&p).unwrap();
                let a = &mut pn.agent;
                if state == Some(AgentState::Unknown) {
                    // The agent session ended.
                    a.hooked = false;
                    a.kind = None;
                    a.set(AgentState::Unknown);
                } else {
                    a.kind = Some(agent.clone());
                    // Only hooks that report every transition replace the screen heuristics;
                    // Codex's notify, for one, only says when a turn ends.
                    a.hooked = crate::agent::hooks_cover_lifecycle(&agent);
                    if let Some(id) = session_id {
                        a.session_id = Some(id);
                    }
                    if let Some(s) = state {
                        a.set(s);
                    }
                    if message.is_some() {
                        a.message = message;
                    }
                }
                self.on_agent_change(p, before);
                self.mark_persist();
                self.mark_dirty();
                Response::Ok
            }),
            Request::Notify { title, body } => {
                self.notify(&title, &body, None);
                Ok(Response::Ok)
            }
            Request::SaveWorkspace { workspace, name } => {
                let wi = match workspace {
                    Some(w) => self.workspaces.iter().position(|x| x.name == w),
                    None => self
                        .primary
                        .and_then(|p| self.clients.get(&p))
                        .and_then(|c| self.ws_index(c.workspace))
                        .or(Some(0)),
                };
                match wi.filter(|i| *i < self.workspaces.len()) {
                    None => Err("no such workspace".into()),
                    Some(wi) => {
                        let name = name.unwrap_or_else(|| self.workspaces[wi].name.clone());
                        self.save_definition(wi, &name).map(Response::Path).map_err(|e| format!("{e:#}"))
                    }
                }
            }
            Request::OpenSsh { target, group, placement, near } => {
                if group {
                    return Some(self.open_group(&target).map(|_| Response::Ok).map_err(|e| format!("{e:#}")));
                }
                let near = match near.as_deref().map(|n| self.resolve(n)).transpose() {
                    Ok(n) => n,
                    Err(e) => return Some(Err(e)),
                };
                let Some(server) = self.resolve_server(&target) else {
                    return Some(Err(format!("not a server: {target}")));
                };
                let ws = match near.and_then(|n| self.workspace_of(n)) {
                    Some((wi, _)) => self.workspaces[wi].id,
                    None => match self.primary.and_then(|p| self.clients.get(&p)).map(|c| c.workspace) {
                        Some(ws) => ws,
                        None => match self.workspaces.first() {
                            Some(w) => w.id,
                            None => self.new_workspace("main".into(), None),
                        },
                    },
                };
                self.open_ssh(ws, near, &server, placement)
                    .map(|p| Response::Pane(self.pane_info(p)))
                    .map_err(|e| format!("{e:#}"))
            }
            Request::OpenAgent { agent, account, placement, near, cwd } => {
                let near = match near.as_deref().map(|n| self.resolve(n)).transpose() {
                    Ok(n) => n,
                    Err(e) => return Some(Err(e)),
                };
                let ws = match near.and_then(|n| self.workspace_of(n)) {
                    Some((wi, _)) => self.workspaces[wi].id,
                    None => match self.primary.and_then(|p| self.clients.get(&p)).map(|c| c.workspace) {
                        Some(ws) => ws,
                        None => match self.workspaces.first() {
                            Some(w) => w.id,
                            None => self.new_workspace("main".into(), None),
                        },
                    },
                };
                self.launch_agent(ws, near, &agent, account.as_deref(), placement, cwd)
                    .map(|p| Response::Pane(self.pane_info(p)))
                    .map_err(|e| format!("{e:#}"))
            }
            Request::OpenFiles { left, right, near } => {
                let near = match near.as_deref().map(|n| self.resolve(n)).transpose() {
                    Ok(n) => n,
                    Err(e) => return Some(Err(e)),
                };
                let ws = match near.and_then(|n| self.workspace_of(n)) {
                    Some((wi, _)) => self.workspaces[wi].id,
                    None => match self.primary.and_then(|p| self.clients.get(&p)).map(|c| c.workspace) {
                        Some(ws) => ws,
                        None => match self.workspaces.first() {
                            Some(w) => w.id,
                            None => self.new_workspace("main".into(), None),
                        },
                    },
                };
                self.open_files(ws, left, right)
                    .map(|p| Response::Pane(self.pane_info(p)))
                    .map_err(|e| format!("{e:#}"))
            }
            Request::KillServer => {
                self.quitting = true;
                Ok(Response::Ok)
            }
        })
    }

    /// Resolve a pane reference: an id, a pane name, or `workspace:name`.
    fn resolve(&self, r: &str) -> Result<PaneId, String> {
        if let Ok(id) = r.parse::<PaneId>() {
            return if self.panes.contains_key(&id) { Ok(id) } else { Err(format!("no pane with id {id}")) };
        }
        let (ws, name) = match r.split_once(':') {
            Some((w, n)) => (Some(w), n),
            None => (None, r),
        };
        let matches: Vec<PaneId> = self
            .workspaces
            .iter()
            .filter(|w| ws.is_none_or(|n| w.name == n))
            .flat_map(|w| w.panes())
            .filter(|p| {
                self.panes.get(p).is_some_and(|pn| pn.name.as_deref() == Some(name) || pn.display_name() == name)
            })
            .collect();
        match matches.as_slice() {
            [one] => Ok(*one),
            [] => Err(format!("no pane named {r:?}")),
            _ => Err(format!("{} panes are named {r:?}; use an id (kraal panes)", matches.len())),
        }
    }

    fn workspace_infos(&self) -> Vec<WorkspaceInfo> {
        let mut out = Vec::new();
        for ws in &self.workspaces {
            let panes = ws.panes();
            let count =
                |s: AgentState| panes.iter().filter(|p| self.panes.get(p).is_some_and(|x| x.agent.state == s)).count();
            out.push(WorkspaceInfo {
                name: ws.name.clone(),
                open: true,
                definition: ws.definition.clone(),
                tabs: ws.tabs.len(),
                panes: panes.len(),
                working: count(AgentState::Working),
                blocked: count(AgentState::Blocked),
                done: count(AgentState::Done),
            });
        }
        for (path, def) in self.definitions() {
            let Ok(def) = def else { continue };
            if self.workspaces.iter().any(|w| w.name == def.name) {
                continue;
            }
            out.push(WorkspaceInfo {
                name: def.name.clone(),
                open: false,
                definition: Some(path),
                tabs: def.tabs.len(),
                panes: def.panes().len(),
                working: 0,
                blocked: 0,
                done: 0,
            });
        }
        out
    }
}
