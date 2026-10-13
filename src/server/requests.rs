//! Control requests from the CLI and from agents (`kraal run`, `kraal read`, hooks, …).

use std::time::{Duration, Instant};

use super::{ClientId, PendingWait, Server};
use crate::agent::AgentState;
use crate::input::KeySpec;
use crate::layout::PaneId;
use crate::pane::PaneSpec;
use crate::protocol::{self, Request, Response, ServerMsg, WorkspaceInfo};

type Reply = Option<Result<Response, String>>;

impl Server {
    /// Handle one request. `None` means the answer comes later, once what the request waits for
    /// is done: a `kraal wait` condition met, a remote file fetched and its editor open, a
    /// runbook's plan made or its run started, the server stopped, or the git details read of
    /// the directory a new pane or agent opens in. A subscription is answered by its stream of
    /// events.
    pub(super) fn request(&mut self, client: ClientId, req: Request) -> Reply {
        let reply = self.answer(client, req);
        // A request that created a workspace for a pane that then failed to start leaves it
        // empty.
        self.drop_empty_workspaces();
        reply
    }

    fn answer(&mut self, client: ClientId, req: Request) -> Reply {
        Some(match req {
            Request::RunbookPlan { path, target, params, pane } => {
                let pane = match pane.as_deref().map(|p| self.resolve(p)).transpose() {
                    Ok(p) => p,
                    Err(e) => return Some(Err(e)),
                };
                // Answered once the choices a parameter's command lists are known.
                match self.prepare_runbook(&path, target.as_deref(), params, pane, client) {
                    Ok(()) => return None,
                    Err(e) => Err(format!("{e:#}")),
                }
            }
            // Answered once the run is on record and its targets have started.
            Request::RunbookRun { token } => match self.run_prepared(&token, client) {
                Ok(()) => return None,
                Err(e) => Err(format!("{e:#}")),
            },
            Request::Subscribe { events, since, boot } => match self.subscribe(client, events, since, boot) {
                Ok(()) => return None,
                Err(e) => Err(e),
            },
            Request::Ping => Ok(Response::Pong {
                pid: std::process::id(),
                session: self.session.clone(),
                version: protocol::KRAAL_VERSION.into(),
                features: Some(protocol::feature_names()),
            }),
            Request::Unknown(request) => Err(protocol::unsupported(&request)),
            Request::ListPanes => {
                let mut v: Vec<PaneId> = Vec::new();
                for ws in &self.workspaces {
                    v.extend(ws.panes());
                }
                Ok(Response::Panes(v.into_iter().map(|p| self.pane_info(p)).collect()))
            }
            Request::ListAgents => {
                let mut v: Vec<PaneId> = Vec::new();
                for ws in &self.workspaces {
                    v.extend(ws.panes());
                }
                let local = v.into_iter().filter(|p| self.panes[p].agent.kind.is_some()).map(|p| self.pane_info(p));
                Ok(Response::Panes(local.chain(self.remote_agent_infos()).collect()))
            }
            Request::ListWorkspaces => Ok(Response::Workspaces(self.workspace_infos())),
            Request::OpenWorkspace { name } => {
                self.open_workspace(&name).map(|_| Response::Ok).map_err(|e| format!("{e:#}"))
            }
            req @ Request::NewPane { .. } => {
                // A kraal worktree git has not been asked about yet gives its number to the
                // pane's variables: git is asked first, off the loop, and the pane opens once it
                // has answered.
                let unknown = match &req {
                    Request::NewPane { cwd: Some(dir), .. } if self.worktree_unknown(dir) => Some(dir.clone()),
                    _ => None,
                };
                if let Some(dir) = unknown {
                    self.background(
                        move || {
                            let info = crate::git::status(&dir);
                            (dir, info)
                        },
                        move |server, (dir, info)| {
                            server.git_result(dir, info.map(Box::new));
                            let result = server.new_pane(req);
                            server.send_msg(client, &ServerMsg::Response { result });
                            server.clients.remove(&client);
                            server.drop_empty_workspaces();
                        },
                    );
                    return None;
                }
                self.new_pane(req)
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
                            self.relayout();
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
            Request::SendText { pane, text, production } => self.resolve(&pane).and_then(|p| {
                self.allow_input(p, production)?;
                let pn = self.panes.get_mut(&p).ok_or("no such pane")?;
                if pn.exit_code.is_some() {
                    return Err("the pane's program has exited".into());
                }
                pn.write_input(text.into_bytes());
                Ok(Response::Ok)
            }),
            Request::SendKeys { pane, keys, production } => self.resolve(&pane).and_then(|p| {
                self.allow_input(p, production)?;
                let mut specs = Vec::new();
                for k in &keys {
                    specs.push(KeySpec::parse(k).map_err(|e| format!("{k:?}: {e}"))?);
                }
                let pn = self.panes.get_mut(&p).ok_or("no such pane")?;
                let modes = pn.input_modes();
                let mut bytes = Vec::new();
                for s in specs {
                    // As if typed: in the kitty protocol when the program uses it.
                    let key = crate::input::Key { code: s.code, mods: s.mods, raw: Vec::new() };
                    bytes.extend(crate::input::encode_key(&key, modes));
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
                // Some agents run other agents' hooks (Cursor and Continue run Claude Code's):
                // a report that names another agent than the one running here is not about it.
                let running = self.panes[&p].foreground_argv().as_deref().and_then(crate::agent::detect_agent);
                if running.is_some_and(|r| r != agent) {
                    return Response::Ok;
                }
                // The hook's parent: the agent itself when it runs its hooks directly.
                let reporter = self
                    .clients
                    .get(&client)
                    .and_then(|c| c.peer)
                    .and_then(crate::process::ppid)
                    .and_then(|pid| Some((pid, crate::process::start_time(pid)?)));
                let session_process = self.panes[&p].foreground_pid().and_then(|fg| {
                    let (reporter, _) = reporter?;
                    crate::process::lineage(reporter)
                        .contains(&fg)
                        .then(|| Some((fg, crate::process::start_time(fg)?)))
                        .flatten()
                });
                let before = self.panes[&p].agent.state;
                let pn = self.panes.get_mut(&p).unwrap();
                let a = &mut pn.agent;
                if state == Some(AgentState::Unknown) {
                    // The agent session ended.
                    a.hooked = false;
                    a.kind = None;
                    a.reporter = None;
                    a.set(AgentState::Unknown);
                } else {
                    a.kind = Some(agent.clone());
                    // Only hooks that report every transition replace the screen heuristics;
                    // Codex's notify, for one, only says when a turn ends.
                    a.hooked = crate::agent::hooks_cover_lifecycle(&agent);
                    a.reporter = reporter;
                    if let Some(id) = session_id {
                        a.session_id = Some(id);
                        a.session_process = session_process;
                    }
                    // An agent waiting for a prompt says nothing of whether its last result
                    // was seen: a finished turn stays to be looked at.
                    if let Some(s) = state.filter(|s| !(*s == AgentState::Idle && a.state == AgentState::Done)) {
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
                    Some(w) => match self.named_workspace(&w) {
                        Ok(wi) => wi,
                        Err(e) => return Some(Err(format!("{e:#}"))),
                    },
                    None => self
                        .primary
                        .and_then(|p| self.clients.get(&p))
                        .and_then(|c| self.ws_index(c.workspace))
                        .or(Some(0)),
                };
                match wi.filter(|i| *i < self.workspaces.len()) {
                    None => Err("no such workspace".into()),
                    Some(wi) => {
                        let name = name.unwrap_or_else(|| self.definition_file_name(wi));
                        self.save_definition(wi, &name).map(Response::Path).map_err(|e| format!("{e:#}"))
                    }
                }
            }
            Request::OpenSsh { target, group, placement, near, command } => {
                if group {
                    let group = target.strip_prefix('@').unwrap_or(&target);
                    return Some(self.open_group(group, &command).map(|_| Response::Ok).map_err(|e| format!("{e:#}")));
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
                self.open_ssh_running(ws, near, &server, placement, &command)
                    .map(|p| Response::Pane(Box::new(self.pane_info(p))))
                    .map_err(|e| format!("{e:#}"))
            }
            Request::OpenAgent { agent, account, account_id, placement, near, cwd, args } => {
                let account = match account_id {
                    Some(id) => match self.accounts.by_id(&id).filter(|a| a.agent == agent) {
                        Some(a) => Some(format!("@{}", a.id)),
                        None => return Some(Err("the account was removed; choose an account again".into())),
                    },
                    None => account,
                };
                let near = match near.as_deref().map(|n| self.resolve(n)).transpose() {
                    Ok(n) => n,
                    Err(e) => return Some(Err(e)),
                };
                // The account rules may depend on which repository the directory belongs to (a
                // worktree anywhere counts as its repository), and a kraal worktree not seen yet
                // gives its number to the agent's variables: git is asked first, off the loop,
                // and the agent starts once it has answered.
                let ws = self.agent_workspace(near);
                let dir = ws.and_then(|ws| self.agent_cwd(ws, near, cwd.clone()));
                let undecided =
                    account.is_none() && dir.as_ref().is_some_and(|d| self.accounts.undecided(&agent, Some(d)));
                let new_worktree = dir
                    .as_ref()
                    .is_some_and(|d| crate::worktree_setup::in_worktrees_dir(d) && !self.git.contains_key(d));
                if let Some(dir) = dir.filter(|_| undecided || new_worktree) {
                    self.background(
                        move || {
                            crate::git::project_path(&dir);
                            let info = new_worktree.then(|| crate::git::status(&dir)).flatten();
                            (dir, info)
                        },
                        move |server, (dir, info)| {
                            if info.is_some() {
                                server.git_result(dir, info.map(Box::new));
                            }
                            let result = server.open_agent(near, &agent, account.as_deref(), placement, cwd, &args);
                            server.send_msg(client, &ServerMsg::Response { result });
                            server.clients.remove(&client);
                            server.drop_empty_workspaces();
                        },
                    );
                    return None;
                }
                self.open_agent(near, &agent, account.as_deref(), placement, cwd, &args)
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
                    .map(|p| Response::Pane(Box::new(self.pane_info(p))))
                    .map_err(|e| format!("{e:#}"))
            }
            Request::EditRemote { target, line, near, production, sudo } => {
                let near = match near.as_deref().map(|n| self.resolve(n)).transpose() {
                    Ok(n) => n,
                    Err(e) => return Some(Err(e)),
                };
                let (server, path) = match crate::transfer::parse_endpoint(&target, &self.known_servers) {
                    Ok(crate::transfer::Endpoint::Remote { server, path }) => (server, path),
                    Ok(crate::transfer::Endpoint::Local(_)) => {
                        return Some(Err(format!("{target} is not on a server: kraal edit takes SERVER:PATH")));
                    }
                    Err(e) => return Some(Err(format!("{e:#}"))),
                };
                if path.is_empty() {
                    return Some(Err("which file? kraal edit takes SERVER:PATH".into()));
                }
                if !production && let Some(why) = self.server_production(&server) {
                    return Some(Err(format!(
                        "{} is production ({why}): pass --production to edit there",
                        server.name
                    )));
                }
                // A workspace made now would be dropped as empty before the file is read: one is
                // made, if needed, when the editor opens.
                let ws = match near.and_then(|n| self.workspace_of(n)) {
                    Some((wi, _)) => Some(self.workspaces[wi].id),
                    None => self
                        .primary
                        .and_then(|p| self.clients.get(&p))
                        .map(|c| c.workspace)
                        .or_else(|| self.workspaces.first().map(|w| w.id)),
                };
                // Told about saves and asked about changes: a client showing that workspace.
                let shows = |c: &&super::Client| c.attached && Some(c.workspace) == ws;
                let viewer = self.primary.filter(|p| self.clients.get(p).is_some_and(|c| shows(&c)));
                let viewer = viewer.or_else(|| self.clients.values().find(shows).map(|c| c.id)).or(self.primary);
                let mut ssh = self.ssh_options(&server);
                ssh.push(server.destination());
                let target = super::edits::EditTarget { server: server.name.clone(), ssh, path, line, ws, near, sudo };
                self.start_edit(viewer, target, Some(client));
                return None;
            }
            Request::TunnelAdd { server, remote, local } => self
                .add_tunnel(&server, &remote, local)
                .map(|t| Response::Tunnels(vec![t]))
                .map_err(|e| format!("{e:#}")),
            Request::TunnelRemove { key } => {
                self.remove_tunnel(&key).map(|_| Response::Ok).map_err(|e| format!("{e:#}"))
            }
            Request::TunnelList => Ok(Response::Tunnels(self.tunnel_infos())),
            Request::BufferList => Ok(Response::Buffers(self.buffer_infos())),
            Request::BufferShow { buffer } => {
                self.buffers.get(buffer.as_deref()).map(|b| Response::Text(b.text.clone()))
            }
            Request::BufferSet { name, text } => self.set_buffer(name.as_deref(), text),
            Request::BufferPaste { buffer, pane, production } => self.resolve(&pane).and_then(|p| {
                self.allow_input(p, production)?;
                self.paste_buffer_to(p, buffer.as_deref())
            }),
            Request::BufferDelete { buffer, all } => self.delete_buffer(buffer.as_deref(), all),
            Request::ReloadConfig => self.reload_config().map(|warnings| Response::Text(warnings.join("\n"))),
            // Answered when the server stops, once its last state is written or has failed to
            // be: `kraal kill` then says whether the layout a restart brings back is saved. A
            // restart stops every program only once that layout is on disk.
            Request::KillServer { restart } => {
                if restart && let Err(e) = self.save_now() {
                    let why = format!("not restarting: {e}");
                    self.warnings.push(why.clone());
                    return Some(Err(why));
                }
                self.quitting = true;
                self.restarting |= restart;
                self.kill_waiters.push(client);
                return None;
            }
        })
    }

    /// Open a pane as `kraal run` asks.
    fn new_pane(&mut self, req: Request) -> Result<Response, String> {
        let Request::NewPane { target, command, cwd, name, placement, preview, focus, close_on_exit } = req else {
            return Err("not a new pane".into());
        };
        let target = target.as_deref().map(|t| self.resolve(t)).transpose()?;
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
        // What is on screen, to put back when the new pane must not take the focus.
        let shown = self.ws_index(ws).map(|wi| {
            let w = &self.workspaces[wi];
            (w.active, w.tabs.get(w.active).map(|t| t.show_floating))
        });
        let spec = PaneSpec { command, cwd, preview, close_on_exit, ..Default::default() };
        match self.spawn_pane_in(ws, target, spec, placement, name) {
            Ok(id) => {
                if !focus {
                    if let Some(prev) = previous
                        && let Some((wi, ti)) = self.workspace_of(prev)
                    {
                        self.workspaces[wi].tabs[ti].set_focus(prev);
                    }
                    if let (Some(wi), Some((active, show_floating))) = (self.ws_index(ws), shown)
                        && active < self.workspaces[wi].tabs.len()
                    {
                        let w = &mut self.workspaces[wi];
                        w.active = active;
                        if let Some(show) = show_floating {
                            w.tabs[active].show_floating = show && !w.tabs[active].floating.is_empty();
                        }
                    }
                    self.relayout();
                }
                self.mark_dirty();
                Ok(Response::Pane(Box::new(self.pane_info(id))))
            }
            Err(e) => Err(format!("{e:#}")),
        }
    }

    /// Resolve a pane reference: an id, a pane name, or `workspace:name`.
    /// The workspace an agent opened from the CLI goes to: that of the pane it starts next to,
    /// else the one the last active client shows, else the first.
    fn agent_workspace(&self, near: Option<PaneId>) -> Option<u32> {
        near.and_then(|n| self.workspace_of(n))
            .map(|(wi, _)| self.workspaces[wi].id)
            .or_else(|| self.primary.and_then(|p| self.clients.get(&p)).map(|c| c.workspace))
            .or_else(|| self.workspaces.first().map(|w| w.id))
    }

    fn open_agent(
        &mut self,
        near: Option<PaneId>,
        agent: &str,
        account: Option<&str>,
        placement: crate::protocol::Placement,
        cwd: Option<std::path::PathBuf>,
        args: &[String],
    ) -> Result<Response, String> {
        let ws = match self.agent_workspace(near) {
            Some(ws) => ws,
            None => self.new_workspace("main".into(), None),
        };
        self.launch_agent_with(ws, near, agent, account, placement, cwd, args)
            .map(|p| Response::Pane(Box::new(self.pane_info(p))))
            .map_err(|e| format!("{e:#}"))
    }

    /// Input from the CLI reaches a production pane only when the caller says it means to.
    fn allow_input(&self, pane: PaneId, production: bool) -> Result<(), String> {
        match self.pane_production(pane) {
            Some(why) if !production => {
                Err(format!("{} is production ({why}); add --production to send to it", self.pane_label(pane)))
            }
            _ => Ok(()),
        }
    }

    pub(super) fn resolve(&self, r: &str) -> Result<PaneId, String> {
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
                id: Some(ws.stable_id.clone()),
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
            if !matches!(self.definition_workspace(&path), Ok(None)) {
                continue;
            }
            out.push(WorkspaceInfo {
                id: None,
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

impl Server {
    pub(super) fn add_wait(&mut self, client: ClientId, pane: PaneId, until: Vec<String>, timeout: Option<Duration>) {
        let Some(c) = self.clients.remove(&client) else { return };
        self.waits.push(PendingWait { client, out: c.out, pane, until, deadline: timeout.map(|t| Instant::now() + t) });
        self.check_waits();
    }

    pub(super) fn check_waits(&mut self) {
        let now = Instant::now();
        let mut i = 0;
        while i < self.waits.len() {
            let w = &self.waits[i];
            let result = match self.panes.get(&w.pane) {
                None => Some(Err("pane closed".to_string())),
                Some(p) => {
                    let state = p.agent.state;
                    let hit = w.until.iter().any(|u| match u.as_str() {
                        "exit" => p.exit_code.is_some(),
                        "quiet" => p.last_output.elapsed() > Duration::from_secs(2),
                        other => AgentState::parse(other) == Some(state),
                    });
                    if hit {
                        Some(Ok(protocol::Response::Pane(Box::new(self.pane_info(w.pane)))))
                    } else if w.deadline.is_some_and(|d| d <= now) {
                        Some(Err("timeout".to_string()))
                    } else {
                        None
                    }
                }
            };
            match result {
                Some(result) => {
                    let w = self.waits.remove(i);
                    let mut buf = Vec::new();
                    if protocol::write_json(&mut buf, &ServerMsg::Response { result }).is_ok() {
                        let _ = w.out.send_timeout(buf, Duration::from_millis(200));
                    }
                }
                None => i += 1,
            }
        }
    }
}
