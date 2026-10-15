//! Control requests from the CLI and from agents (`kraal run`, `kraal read`, hooks, …).

use anyhow::Context;

use std::time::{Duration, Instant};

use super::{ClientId, PendingWait, Server};
use crate::agent::AgentState;
use crate::input::KeySpec;
use crate::layout::PaneId;
use crate::pane::PaneSpec;
use crate::protocol::{self, Request, Response, ServerMsg, WorkspaceInfo};
use crate::theme::NoticeKind;

// This lives only for dispatch; boxing would allocate for every immediate reply.
#[allow(clippy::large_enum_variant)]
pub(super) enum RequestOutcome {
    Immediate { reply: ReplyHandle, result: Result<Response, String> },
    Deferred,
    Streaming,
}

/// The right to answer one control request. Dropping it sends nothing.
pub(super) struct ReplyHandle {
    client: ClientId,
    out: Option<super::clients::Outbox>,
}

impl ReplyHandle {
    pub(super) fn new(client: ClientId) -> Self {
        Self { client, out: None }
    }

    pub(super) fn client(&self) -> ClientId {
        self.client
    }

    pub(super) fn complete(self, server: &mut Server, result: Result<Response, String>) {
        if let Some(out) = self.out {
            let mut buf = Vec::new();
            if protocol::write_json(&mut buf, &ServerMsg::Response { result }).is_ok() {
                let _ = out.send_timeout(buf, Duration::from_millis(200));
            }
        } else {
            server.send_msg(self.client, &ServerMsg::Response { result });
            server.clients.remove(&self.client);
        }
    }
}

impl RequestOutcome {
    #[cfg(test)]
    pub(super) fn immediate(self) -> Result<Response, String> {
        let Self::Immediate { result, .. } = self else { panic!("expected an immediate response") };
        result
    }
}

impl Server {
    pub(super) fn request(&mut self, client: ClientId, req: Request) -> RequestOutcome {
        let before: Vec<_> = self.workspaces.iter().map(|w| w.id).collect();
        let outcome = self.answer(ReplyHandle::new(client), req);
        if matches!(&outcome, RequestOutcome::Immediate { result: Err(_), .. }) {
            self.workspaces.retain(|w| before.contains(&w.id) || !w.tabs.is_empty() || w.kept.is_some());
        }
        outcome
    }

    /// Failed requests discard only the empty workspaces they just created.
    fn with_workspace_cleanup<T>(&mut self, f: impl FnOnce(&mut Self) -> Result<T, String>) -> Result<T, String> {
        let before: Vec<_> = self.workspaces.iter().map(|w| w.id).collect();
        let result = f(self);
        if result.is_err() {
            self.workspaces.retain(|w| before.contains(&w.id) || !w.tabs.is_empty() || w.kept.is_some());
        }
        result
    }

    fn answer(&mut self, reply: ReplyHandle, req: Request) -> RequestOutcome {
        match req {
            req @ (Request::RunbookPlan { .. } | Request::RunbookRun { .. }) => self.answer_runbook_request(reply, req),
            req @ (Request::NewPane { .. } | Request::OpenAgent { .. } | Request::EditRemote { .. }) => {
                self.answer_launch(reply, req)
            }
            Request::Wait { pane, until, timeout_ms } => self.answer_wait(reply, pane, until, timeout_ms),
            Request::KillServer { restart } => self.answer_shutdown(reply, restart),
            Request::Subscribe { events, since, boot } => self.subscribe(reply, events, since, boot),
            req => {
                let result = self.answer_immediate(reply.client(), req);
                RequestOutcome::Immediate { reply, result }
            }
        }
    }

    fn answer_runbook_request(&mut self, reply: ReplyHandle, req: Request) -> RequestOutcome {
        let mut reply = Some(reply);
        let result = match req {
            Request::RunbookPlan { path, target, params, pane } => {
                let pane = match pane.as_deref().map(|p| self.resolve(p)).transpose() {
                    Ok(p) => p,
                    Err(e) => return RequestOutcome::Immediate { reply: reply.take().unwrap(), result: Err(e) },
                };
                // Answered once the choices a parameter's command lists are known.
                match self.prepare_runbook(&path, target.as_deref(), params, pane, &mut reply) {
                    Ok(()) => return RequestOutcome::Deferred,
                    Err(e) => Err(format!("{e:#}")),
                }
            }
            // Answered once the run is on record and its targets have started.
            Request::RunbookRun { token } => match self.run_prepared(&token, &mut reply) {
                Ok(()) => return RequestOutcome::Deferred,
                Err(e) => Err(format!("{e:#}")),
            },
            _ => unreachable!("runbook request"),
        };
        RequestOutcome::Immediate { reply: reply.take().unwrap(), result }
    }

    fn answer_launch(&mut self, reply: ReplyHandle, req: Request) -> RequestOutcome {
        let result = match req {
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
                            let result = server.with_workspace_cleanup(|server| server.new_pane(req));
                            reply.complete(server, result);
                        },
                    );
                    return RequestOutcome::Deferred;
                }
                self.new_pane(req)
            }

            Request::OpenAgent { agent, account, account_id, placement, near, cwd, args } => {
                let account = match account_id {
                    Some(id) => match self.accounts.by_id(&id).filter(|a| a.agent == agent) {
                        Some(a) => Some(format!("@{}", a.id)),
                        None => {
                            return RequestOutcome::Immediate {
                                reply,
                                result: Err("the account was removed; choose an account again".into()),
                            };
                        }
                    },
                    None => account,
                };
                let near = match near.as_deref().map(|n| self.resolve(n)).transpose() {
                    Ok(n) => n,
                    Err(e) => return RequestOutcome::Immediate { reply, result: Err(e) },
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
                            let result = server.with_workspace_cleanup(|server| {
                                server.open_agent(near, &agent, account.as_deref(), placement, cwd, &args)
                            });
                            reply.complete(server, result);
                        },
                    );
                    return RequestOutcome::Deferred;
                }
                self.open_agent(near, &agent, account.as_deref(), placement, cwd, &args)
            }
            Request::EditRemote { target, line, near, production, sudo } => {
                let near = match near.as_deref().map(|n| self.resolve(n)).transpose() {
                    Ok(n) => n,
                    Err(e) => return RequestOutcome::Immediate { reply, result: Err(e) },
                };
                let (server, path) = match crate::transfer::parse_endpoint(&target, &self.known_servers) {
                    Ok(crate::transfer::Endpoint::Remote { server, path }) => (server, path),
                    Ok(crate::transfer::Endpoint::Local(_)) => {
                        return RequestOutcome::Immediate {
                            reply,
                            result: Err(format!("{target} is not on a server: kraal edit takes SERVER:PATH")),
                        };
                    }
                    Err(e) => return RequestOutcome::Immediate { reply, result: Err(format!("{e:#}")) },
                };
                if path.is_empty() {
                    return RequestOutcome::Immediate {
                        reply,
                        result: Err("which file? kraal edit takes SERVER:PATH".into()),
                    };
                }
                if !production && let Some(why) = self.server_production(&server) {
                    return RequestOutcome::Immediate {
                        reply,
                        result: Err(format!("{} is production ({why}): pass --production to edit there", server.name)),
                    };
                }
                // Create a workspace, if needed, once the file is ready for its editor.
                let ws = match near.and_then(|n| self.workspace_of(n)) {
                    Some((wi, _)) => Some(self.workspaces[wi].id),
                    None => self.available_workspace(None),
                };
                // Told about saves and asked about changes: a client showing that workspace.
                let shows = |c: &&super::Client| c.attached && Some(c.workspace) == ws;
                let viewer = self.primary.filter(|p| self.clients.get(p).is_some_and(|c| shows(&c)));
                let viewer = viewer.or_else(|| self.clients.values().find(shows).map(|c| c.id)).or(self.primary);
                let mut ssh = self.ssh_options(&server);
                ssh.push(server.destination());
                let target = super::edits::EditTarget { server: server.name.clone(), ssh, path, line, ws, near, sudo };
                self.start_edit(viewer, target, Some(reply));
                return RequestOutcome::Deferred;
            }
            _ => unreachable!("launch request"),
        };
        RequestOutcome::Immediate { reply, result }
    }

    fn answer_wait(
        &mut self,
        reply: ReplyHandle,
        pane: String,
        until: Vec<String>,
        timeout_ms: Option<u64>,
    ) -> RequestOutcome {
        let result = match self.resolve(&pane) {
            Ok(p) => {
                let until = if until.is_empty() {
                    vec!["blocked".into(), "done".into(), "idle".into(), "exit".into()]
                } else {
                    until
                };
                for u in &until {
                    if !matches!(u.as_str(), "exit" | "quiet") && AgentState::parse(u).is_none() {
                        return RequestOutcome::Immediate {
                            reply,
                            result: Err(format!(
                                "unknown condition {u:?} (use working, blocked, done, idle, exit, quiet)"
                            )),
                        };
                    }
                }
                self.add_wait(reply, p, until, timeout_ms.map(Duration::from_millis));
                return RequestOutcome::Deferred;
            }
            Err(e) => Err(e),
        };
        RequestOutcome::Immediate { reply, result }
    }

    fn answer_shutdown(&mut self, reply: ReplyHandle, restart: bool) -> RequestOutcome {
        if restart && let Err(e) = self.save_now() {
            let why = format!("not restarting: {e}");
            self.warnings.push(why.clone());
            return RequestOutcome::Immediate { reply, result: Err(why) };
        }
        self.quitting = true;
        self.restarting |= restart;
        self.kill_waiters.push(reply);
        RequestOutcome::Deferred
    }

    fn answer_immediate(&mut self, client: ClientId, req: Request) -> Result<Response, String> {
        match req {
            Request::Ping => Ok(Response::Pong {
                pid: std::process::id(),
                session: self.session.clone(),
                version: protocol::KRAAL_VERSION.into(),
                protocol: protocol::PROTOCOL_VERSION,
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
            req @ (Request::ListDeletedWorkspaces
            | Request::CreateWorkspace { .. }
            | Request::RenameWorkspace { .. }
            | Request::CloseWorkspace { .. }
            | Request::DeleteWorkspace { .. }
            | Request::RestoreWorkspace { .. }) => self.workspace_request(req),
            Request::ListWorkspaces => Ok(Response::Workspaces(self.workspace_infos())),
            Request::OpenWorkspace { name } => {
                self.open_workspace(&name).map(|_| Response::Ok).map_err(|e| format!("{e:#}"))
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
                self.rename_pane(p, (!name.is_empty()).then_some(name));
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
                self.notify(NoticeKind::Notice, &title, &body, None);
                Ok(Response::Ok)
            }
            Request::ImportWorkspace { path, name, expected, accounts } => (|| {
                if let Some(expected) = expected {
                    let name = name.context("Choose a workspace name.")?;
                    self.import_layout(&path, &name, &expected, &accounts)?;
                    Ok(Response::Ok)
                } else {
                    self.preview_import(&path).map(Response::ImportPreview)
                }
            })()
            .map_err(|e: anyhow::Error| format!("{e:#}")),
            Request::PublishWorkspaceFile { path, source, expected } => {
                let sources =
                    self.workspaces.iter().filter_map(|w| Some((w.name.as_str(), w.definition.as_deref()?))).chain(
                        self.unrestored.iter().filter_map(|w| Some((w.name.as_str(), w.definition.as_deref()?))),
                    );
                crate::workspace::publish_import(&path, &source, &expected, sources)
                    .map(|()| Response::Ok)
                    .map_err(|e| format!("{e:#}"))
            }
            Request::ExportWorkspace { workspace, identity, path, expected } => (|| {
                let wi = if expected.is_some() {
                    let identity = identity.context("Preview the workspace before exporting.")?;
                    self.workspaces.iter().position(|w| w.stable_id == identity)
                } else {
                    match workspace {
                        Some(name) => self.named_workspace(&name)?,
                        None => self.available_workspace(None).and_then(|id| self.ws_index(id)),
                    }
                }
                .context("no such workspace")?;
                let save = self.prepare_layout_export(wi, &path)?;
                if let Some(expected) = expected {
                    anyhow::ensure!(save.revision == expected, "File changed. Retry to review the destination.");
                    self.write_definition(wi, &self.workspaces[wi].name.clone(), &save).map(Response::Path)
                } else {
                    Ok(Response::ExportPreview {
                        identity: self.workspaces[wi].stable_id.clone(),
                        path: save.path,
                        revision: save.revision,
                        question: save.question,
                    })
                }
            })()
            .map_err(|e: anyhow::Error| format!("{e:#}")),
            Request::OpenSsh { target, group, placement, near, command } => {
                if group {
                    let group = target.strip_prefix('@').unwrap_or(&target);
                    return self.open_group(group, &command).map(|_| Response::Ok).map_err(|e| format!("{e:#}"));
                }
                let near = near.as_deref().map(|n| self.resolve(n)).transpose()?;
                let Some(server) = self.resolve_server(&target) else {
                    return Err(format!("not a server: {target}"));
                };
                let ws = match near.and_then(|n| self.workspace_of(n)) {
                    Some((wi, _)) => self.workspaces[wi].id,
                    None => self.available_workspace(None).unwrap_or_else(|| self.new_workspace("main".into(), None)),
                };
                self.open_ssh_running(ws, near, &server, placement, &command)
                    .map(|p| Response::Pane(Box::new(self.pane_info(p))))
                    .map_err(|e| format!("{e:#}"))
            }
            Request::OpenFiles { left, right, near } => {
                let near = near.as_deref().map(|n| self.resolve(n)).transpose()?;
                let ws = match near.and_then(|n| self.workspace_of(n)) {
                    Some((wi, _)) => self.workspaces[wi].id,
                    None => self.available_workspace(None).unwrap_or_else(|| self.new_workspace("main".into(), None)),
                };
                self.open_files(ws, left, right)
                    .map(|p| Response::Pane(Box::new(self.pane_info(p))))
                    .map_err(|e| format!("{e:#}"))
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
            Request::RunbookPlan { .. }
            | Request::RunbookRun { .. }
            | Request::Subscribe { .. }
            | Request::NewPane { .. }
            | Request::OpenAgent { .. }
            | Request::EditRemote { .. }
            | Request::Wait { .. }
            | Request::KillServer { .. } => unreachable!("request family dispatched"),
        }
    }

    /// Open a pane as `kraal run` asks.
    fn new_pane(&mut self, req: Request) -> Result<Response, String> {
        let Request::NewPane { target, command, cwd, name, placement, preview, focus, close_on_exit } = req else {
            return Err("not a new pane".into());
        };
        let target = target.as_deref().map(|t| self.resolve(t)).transpose()?;
        let ws = match target.and_then(|t| self.workspace_of(t)) {
            Some((wi, _)) => self.workspaces[wi].id,
            None => match self.available_workspace(None) {
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

    /// The workspace an agent opened from the CLI goes to: that of the pane it starts next to,
    /// else an open workspace, preferring the one the last active client shows.
    fn agent_workspace(&self, near: Option<PaneId>) -> Option<u32> {
        near.and_then(|n| self.workspace_of(n))
            .map(|(wi, _)| self.workspaces[wi].id)
            .or_else(|| self.available_workspace(None))
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
        if self.workspace_of(pane).is_some_and(|(wi, _)| self.workspaces[wi].kept.is_some()) {
            return Err("The workspace has not finished closing. Retry Close to continue.".into());
        }
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
        use crate::workspace::catalog::{Entry, Source};

        let definitions = self.definitions();
        let mut catalog = self.workspace_catalog(definitions.iter().map(Source::loaded));
        catalog.sort_by_key(|entry| entry.position(&self.workspace_order));
        catalog
            .into_iter()
            .filter_map(|entry| match entry {
                Entry::Kept(w) => Some(self.workspace_info(w.workspace)),
                Entry::Source(s) => s.info(),
            })
            .collect()
    }
}

impl Server {
    pub(super) fn add_wait(
        &mut self,
        mut reply: ReplyHandle,
        pane: PaneId,
        until: Vec<String>,
        timeout: Option<Duration>,
    ) {
        let Some(c) = self.clients.remove(&reply.client()) else { return };
        reply.out = Some(c.out);
        self.waits.push(PendingWait { reply, pane, until, deadline: timeout.map(|t| Instant::now() + t) });
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
                    w.reply.complete(self, result);
                }
                None => i += 1,
            }
        }
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    use crate::server::{
        persistence::tests::writer,
        tests::{client, server},
    };
    use crate::testing::PATIENCE;

    pub(in crate::server) fn response(receive: &crossbeam_channel::Receiver<Vec<u8>>) -> Result<Response, String> {
        let frame = receive.recv_timeout(PATIENCE).unwrap();
        let (kind, payload) = protocol::read_frame(&mut frame.as_slice()).unwrap().unwrap();
        assert_eq!(kind, protocol::JSON);
        let ServerMsg::Response { result } = serde_json::from_slice(&payload).unwrap() else {
            panic!("expected a response");
        };
        result
    }

    pub(in crate::server) fn finish_job(server: &mut Server, rx: &crate::server::queue::EventReceiver) {
        loop {
            let event = rx.recv_timeout(PATIENCE).unwrap();
            if matches!(event, crate::server::Event::Job(..)) {
                server.handle(event);
                return;
            }
        }
    }

    fn files(server: &mut Server) -> PaneId {
        let Response::Pane(pane) = server
            .request(0, Request::OpenFiles { left: "/missing".into(), right: "/missing".into(), near: None })
            .immediate()
            .unwrap()
        else {
            panic!("files pane")
        };
        pane.id
    }

    #[test]
    fn immediate_replies_answer_once_and_release_the_connection() {
        let (tx, _rx) = crate::server::event_channel(32);
        let mut server = server(tx);
        for req in [Request::Ping, Request::Unknown(serde_json::json!({"cmd": "future_request"}))] {
            let success = matches!(req, Request::Ping);
            let (id, _peer) = client(&mut server);
            let receive = server.clients.get_mut(&id).unwrap().out.capture();
            let RequestOutcome::Immediate { reply, result } = server.request(id, req) else {
                panic!("immediate reply");
            };
            assert!(receive.is_empty());
            reply.complete(&mut server, result);
            if success {
                assert!(matches!(response(&receive), Ok(Response::Pong { protocol: protocol::PROTOCOL_VERSION, .. })));
            } else {
                assert_eq!(
                    response(&receive).unwrap_err(),
                    protocol::unsupported(&serde_json::json!({"cmd": "future_request"}))
                );
            }
            assert!(!server.clients.contains_key(&id));
            assert!(matches!(receive.try_recv(), Err(crossbeam_channel::TryRecvError::Disconnected)));
        }
    }

    #[test]
    fn waits_own_the_connection_until_a_condition_timeout_or_closed_pane() {
        for end in ["already done", "done", "timeout", "closed"] {
            let (tx, _rx) = crate::server::event_channel(32);
            let mut server = server(tx);
            let pane = files(&mut server);
            server.panes.get_mut(&pane).unwrap().agent.set(if end == "already done" {
                AgentState::Done
            } else {
                AgentState::Working
            });
            let (id, _peer) = client(&mut server);
            let receive = server.clients.get_mut(&id).unwrap().out.capture();
            let outcome = server.request(
                id,
                Request::Wait { pane: pane.to_string(), until: vec!["done".into()], timeout_ms: Some(60_000) },
            );
            assert!(matches!(outcome, RequestOutcome::Deferred));
            assert!(!server.clients.contains_key(&id));
            if end != "already done" {
                assert!(receive.is_empty());
                assert_eq!(server.waits.len(), 1);
                match end {
                    "done" => {
                        server.panes.get_mut(&pane).unwrap().agent.set(AgentState::Done);
                    }
                    "timeout" => server.waits[0].deadline = Some(Instant::now()),
                    "closed" => {
                        server.panes.remove(&pane);
                    }
                    _ => unreachable!(),
                }
                server.check_waits();
            }
            match end {
                "timeout" => assert_eq!(response(&receive).unwrap_err(), "timeout"),
                "closed" => assert_eq!(response(&receive).unwrap_err(), "pane closed"),
                _ => assert!(matches!(response(&receive), Ok(Response::Pane(p)) if p.id == pane)),
            }
            assert!(server.waits.is_empty());
            server.check_waits();
            assert!(matches!(receive.try_recv(), Err(crossbeam_channel::TryRecvError::Disconnected)));
        }
    }

    #[test]
    fn disconnected_waits_are_removed_without_closing_the_pane() {
        let (tx, _rx) = crate::server::event_channel(32);
        let mut server = server(tx);
        let pane = files(&mut server);
        let (id, _peer) = client(&mut server);
        let receive = server.clients.get_mut(&id).unwrap().out.capture();
        let invalid = server
            .request(id, Request::Wait { pane: pane.to_string(), until: vec!["invalid".into()], timeout_ms: None });
        assert_eq!(
            invalid.immediate().unwrap_err(),
            "unknown condition \"invalid\" (use working, blocked, done, idle, exit, quiet)"
        );
        assert!(server.waits.is_empty());
        assert!(server.clients.contains_key(&id));
        assert!(matches!(
            server.request(id, Request::Wait { pane: pane.to_string(), until: vec!["exit".into()], timeout_ms: None }),
            RequestOutcome::Deferred
        ));
        server.drop_client(id);
        assert!(server.waits.is_empty());
        assert!(server.panes.contains_key(&pane));
        assert!(matches!(receive.try_recv(), Err(crossbeam_channel::TryRecvError::Disconnected)));
    }

    #[test]
    fn dropping_or_completing_after_disconnect_does_not_send_an_extra_response() {
        let (tx, _rx) = crate::server::event_channel(32);
        let mut server = server(tx);
        let (id, _peer) = client(&mut server);
        let receive = server.clients.get_mut(&id).unwrap().out.capture();
        let reply = ReplyHandle::new(id);
        drop(reply);
        assert!(receive.is_empty());
        assert!(server.clients.contains_key(&id));
        let reply = ReplyHandle::new(id);
        server.drop_client(id);
        reply.complete(&mut server, Ok(Response::Ok));
        assert!(matches!(receive.try_recv(), Err(crossbeam_channel::TryRecvError::Disconnected)));
    }

    #[test]
    fn shutdown_replies_follow_the_final_save_attempt() {
        for failure in [false, true] {
            let (tx, _rx) = crate::server::event_channel(32);
            let mut server = server(tx);
            let saved = writer(&mut server, vec![if failure { Err("disk full".into()) } else { Ok(()) }]);
            let (id, _peer) = client(&mut server);
            let receive = server.clients.get_mut(&id).unwrap().out.capture();
            assert!(matches!(server.request(id, Request::KillServer { restart: false }), RequestOutcome::Deferred));
            assert!(receive.is_empty());
            assert!(saved.lock().unwrap().is_empty());
            assert_eq!(server.kill_waiters.len(), 1);
            server.shutdown();
            assert_eq!(saved.lock().unwrap().len(), 1);
            if failure {
                let Response::Text(text) = response(&receive).unwrap() else { panic!("save failure") };
                assert!(text.contains("disk full"));
            } else {
                assert!(matches!(response(&receive), Ok(Response::Ok)));
            }
            assert!(server.kill_waiters.is_empty());
            assert!(matches!(receive.try_recv(), Err(crossbeam_channel::TryRecvError::Disconnected)));
        }
    }

    #[test]
    fn restart_refusal_is_immediate_and_disconnect_does_not_cancel_shutdown() {
        let (tx, _rx) = crate::server::event_channel(32);
        let mut server = server(tx);
        let saved = writer(&mut server, vec![Err("disk full".into()), Ok(()), Ok(())]);
        let (id, _peer) = client(&mut server);
        let receive = server.clients.get_mut(&id).unwrap().out.capture();
        let error = server.request(id, Request::KillServer { restart: true }).immediate().unwrap_err();
        assert!(error.starts_with("not restarting: "));
        assert!(error.contains("disk full"));
        assert!(!server.quitting);
        assert!(!server.restarting);
        assert!(server.kill_waiters.is_empty());
        assert!(matches!(server.request(id, Request::KillServer { restart: true }), RequestOutcome::Deferred));
        assert!(receive.is_empty());
        assert_eq!(saved.lock().unwrap().len(), 2);
        server.drop_client(id);
        server.workspaces[0].name = "changed before shutdown".into();
        server.shutdown();
        assert_eq!(saved.lock().unwrap().len(), 3);
        assert!(server.quitting && server.restarting);
        assert!(server.kill_waiters.is_empty());
        assert!(matches!(receive.try_recv(), Err(crossbeam_channel::TryRecvError::Disconnected)));
    }

    #[test]
    fn git_lookup_replies_preserve_completion_disconnect_and_start_failure() {
        for mode in ["completed", "disconnected", "start failure"] {
            for agent in [false, true] {
                let (tx, rx) = crate::server::event_channel(32);
                let mut server = server(tx);
                let dir = crate::testing::TempDir::new("reply-launch");
                let cwd = dir.mkdir("app.worktrees/topic");
                let (id, _peer) = client(&mut server);
                let receive = server.clients.get_mut(&id).unwrap().out.capture();
                let req = if agent {
                    let Request::OpenAgent { agent, account, account_id, placement, near, args, .. } =
                        missing_account()
                    else {
                        unreachable!()
                    };
                    Request::OpenAgent { agent, account, account_id, placement, near, cwd: Some(cwd), args }
                } else {
                    Request::NewPane {
                        target: None,
                        command: vec!["true".into()],
                        cwd: Some(cwd),
                        name: None,
                        placement: protocol::Placement::Tab,
                        preview: None,
                        focus: false,
                        close_on_exit: false,
                    }
                };
                super::super::jobs::FAIL_START.set(mode == "start failure");
                assert!(matches!(server.request(id, req), RequestOutcome::Deferred));
                assert!(receive.is_empty());
                if mode == "start failure" {
                    assert!(server.jobs.is_empty());
                    assert!(server.clients.contains_key(&id));
                    assert!(server.panes.is_empty());
                    assert_eq!(server.warnings, ["cannot start a background job: thread start failed"]);
                    continue;
                }
                if mode == "disconnected" {
                    server.drop_client(id);
                }
                let _batch = crate::pane::batch();
                finish_job(&mut server, &rx);
                if mode != "disconnected" {
                    if agent {
                        assert!(response(&receive).unwrap_err().contains("missing"));
                    } else {
                        assert!(matches!(response(&receive), Ok(Response::Pane(_))));
                    }
                }
                assert_eq!(server.panes.len(), usize::from(!agent));
                assert!(!server.clients.contains_key(&id));
                assert!(server.jobs.is_empty());
                assert!(matches!(receive.try_recv(), Err(crossbeam_channel::TryRecvError::Disconnected)));
            }
        }
    }

    fn missing_account() -> Request {
        Request::OpenAgent {
            agent: "claude".into(),
            account: Some("missing".into()),
            account_id: None,
            placement: protocol::Placement::Tab,
            near: None,
            cwd: None,
            args: Vec::new(),
        }
    }

    #[test]
    fn failed_requests_remove_only_the_empty_workspaces_they_created() {
        let (tx, _rx) = crate::server::event_channel(4096);
        let mut server = super::super::tests::server(tx);
        assert!(server.request(0, missing_account()).immediate().is_err());
        assert!(server.workspaces.is_empty());
        let existing = server.new_workspace("empty".into(), None);
        assert!(server.request(0, missing_account()).immediate().is_err());
        assert_eq!(server.workspaces.len(), 1);
        assert_eq!(server.workspaces[0].id, existing);
        assert!(server.workspaces[0].tabs.is_empty());
        assert!(server.workspaces[0].kept.is_none());
        assert_eq!(crate::server::persistence::snapshot(&server).workspaces.len(), 1);
    }

    #[test]
    fn implicit_requests_use_an_open_workspace_without_an_attached_client() {
        let (tx, _rx) = crate::server::event_channel(4096);
        let mut server = super::super::tests::server(tx);
        let closed = server.new_workspace("a".into(), None);
        crate::server::persistence::tests::writer(&mut server, vec![]);
        server.keep_and_close_workspace(closed).unwrap();
        let open = server.new_workspace("b".into(), None);
        assert!(server.primary.is_none());
        assert_eq!(server.agent_workspace(None), Some(open));
        let request = Request::NewPane {
            target: None,
            command: vec!["sleep".into(), "60".into()],
            cwd: None,
            name: Some("new".into()),
            placement: protocol::Placement::Tab,
            preview: None,
            focus: false,
            close_on_exit: false,
        };
        let Response::Pane(pane) = server.request(0, request).immediate().unwrap() else { panic!("new pane") };
        assert_eq!(pane.workspace, "b");
        let request = Request::OpenFiles { left: "/missing".into(), right: "/missing".into(), near: None };
        let Response::Pane(files) = server.request(0, request).immediate().unwrap() else { panic!("files pane") };
        assert_eq!(files.workspace, "b");
        assert!(server.workspaces[0].closed());
        assert!(server.workspaces[0].panes().is_empty());
        server.close_pane(pane.id);
        server.close_pane(files.id);
    }
}
