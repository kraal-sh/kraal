//! Running a step runbook: one pane per target, the schedule, timeouts, the log and the ledger.

use std::path::Path;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use serde_json::json;

use super::{Asker, Draft, ItemState, Run, RunItem, Target};
use crate::layout::PaneId;
use crate::pane::PaneSpec;
use crate::runbooks::{self, Source};
use crate::server::{ClientId, Server, Tab};
use crate::tools::{Open, ToolContext, ToolDef, Where};
use crate::{paths, shell};

impl Server {
    /// Start a prepared runbook: one pane per target in a new tab, `rolling` at a time. The run
    /// is first written in the ledger (by the log writer, which may be busy: the event loop does
    /// not wait for it); its targets start once it is on record, if they are still where the
    /// plan said. `asker` hears how it went; an `Err` here means nothing was started.
    pub(in crate::server) fn launch_runbook(&mut self, d: Draft, asker: Asker) -> Result<()> {
        self.check_launch(&d)?;
        if matches!(d.runbook.source, Source::Repo(_)) && !runbooks::trusted(&d.runbook) {
            runbooks::trust(&d.runbook)?;
        }
        let specs = self.launch_specs(&d)?;
        let labels: Vec<String> = specs.iter().map(|(l, _)| l.clone()).collect();
        let rolling = if d.steps.rolling == 0 { specs.len() } else { d.steps.rolling.min(specs.len()) };
        // The log exists before anything runs.
        let id = self.alloc_id();
        let name = d.runbook.name.clone();
        let log = runbooks::create_log(&name, id, &self.log_header(&d, &labels, rolling))?;
        // The run is on record before anything starts: a server that dies from now on leaves a
        // ledger the next one closes the log with.
        let entry = runbooks::LedgerRun {
            id,
            name,
            log: log.clone(),
            targets: labels
                .iter()
                .enumerate()
                .map(|(n, label)| runbooks::LedgerTarget {
                    label: label.clone(),
                    pending: true,
                    output: Some(log.with_extension(format!("{n}.out"))),
                })
                .collect(),
        };
        self.launching.push(entry);
        let ledger = self.ledger_entries();
        let job = self.alloc_id();
        self.jobs.insert(
            job,
            Box::new(move |server, result| {
                let recorded = match result.downcast::<Result<(), String>>() {
                    Ok(r) => *r,
                    Err(_) => Err("no answer from the log writer".into()),
                };
                let started = server.start_recorded(d, id, &log, recorded, asker.client);
                server.report_launch(asker, started);
            }),
        );
        let tx = self.tx.clone();
        let done = Box::new(move |r: Result<(), String>| {
            let _ = tx.send(crate::server::Event::Job(job, Ok(Box::new(r))));
        });
        if self.run_logs.send(runbooks::LogJob::LedgerSync(self.session.clone(), ledger, done)).is_err() {
            self.jobs.remove(&job);
            self.launching.retain(|r| r.id != id);
            bail!("the run log writer is gone: nothing was started");
        }
        Ok(())
    }

    /// What must hold for a run to start, when it is asked and again once it is on record.
    fn check_launch(&self, d: &Draft) -> Result<()> {
        // A pane that went away since the plan must not turn into "here".
        for t in d.targets.as_deref().unwrap_or_default() {
            if let Target::Pane(p) = t
                && !self.panes.contains_key(p)
            {
                bail!("the pane {} ran in is gone; review the runbook again", d.runbook.name);
            }
        }
        if d.targets.as_ref().is_none_or(|t| t.is_empty()) {
            bail!("nowhere to run {}", d.runbook.name);
        }
        if !d.trusted {
            bail!("{} from a repository has not been approved", d.runbook.name);
        }
        // The plan said where it runs: a pane that moved (another server, directory, production
        // status) since is not what was approved.
        if d.reviewed.as_ref().is_some_and(|r| *r != self.identities(d)) {
            bail!("where {} would run changed since its plan was shown: review it again", d.runbook.name);
        }
        Ok(())
    }

    /// The pane of each target, with its label.
    fn launch_specs(&self, d: &Draft) -> Result<Vec<(String, PaneSpec)>> {
        let s = &d.steps;
        let secret = |k: &String| d.steps.params.iter().any(|p| &p.name == k && p.secret);
        let (secrets, mut extra): (Vec<_>, Vec<_>) = d.values.iter().cloned().partition(|(k, _)| secret(k));
        extra.push(("KRAAL_RUNBOOK".into(), d.runbook.name.clone()));
        let command = runbooks::command(&s.shell, &runbooks::script(&s.steps, s.stop_on_failure));
        let tool = |on: Where| ToolDef {
            command: command.clone(),
            description: String::new(),
            key: None,
            on,
            open: Open::Tab,
            hold: true,
            when: Vec::new(),
            enabled: true,
            read_only: false,
        };
        let base = d.pane.map(|p| self.tool_context(p)).unwrap_or_default();
        let local_cwd = self.draft_cwd(d);
        let mut specs = Vec::new();
        for t in d.targets.as_deref().unwrap_or_default() {
            let (ctx, on) = match t {
                Target::Local => {
                    let ctx = ToolContext {
                        server: None,
                        remote_cwd: None,
                        cwd: local_cwd.clone(),
                        extra: extra.clone(),
                        secrets: secrets.clone(),
                        ..base.clone()
                    };
                    (ctx, Where::Local)
                }
                Target::Pane(p) => (
                    ToolContext { extra: extra.clone(), secrets: secrets.clone(), ..self.tool_context(*p) },
                    Where::Pane,
                ),
                Target::Server(srv) => {
                    let ctx = ToolContext {
                        server: Some(srv.clone()),
                        production: self.target_production(t, d.pane).map(|p| p.why),
                        extra: extra.clone(),
                        secrets: secrets.clone(),
                        ..Default::default()
                    };
                    (ctx, Where::Pane)
                }
            };
            let spec = tool(on).spec(&ctx, &|srv, remote| self.ssh_command_running(srv, remote));
            ToolDef::check_cwd(&spec)?;
            specs.push((self.target_label(t), spec));
        }
        Ok(specs)
    }

    /// The run is on record (or could not be put there): start its targets, if they are still
    /// where the plan said. Its log says why nothing was started otherwise.
    fn start_recorded(
        &mut self,
        d: Draft,
        id: u32,
        log: &Path,
        recorded: Result<(), String>,
        client: Option<ClientId>,
    ) -> Result<u32> {
        self.launching.retain(|r| r.id != id);
        let started = match recorded {
            Err(e) => Err(anyhow::anyhow!("could not record the run ({e}): nothing was started")),
            Ok(()) => self.check_launch(&d).and_then(|()| self.launch_specs(&d)).and_then(|specs| {
                let rolling = if d.steps.rolling == 0 { specs.len() } else { d.steps.rolling.min(specs.len()) };
                self.start_run(&d, id, log, specs, rolling, client)
            }),
        };
        if let Err(e) = &started {
            self.append_log(log, &format!("\nnot started: {e:#}\n"));
            self.save_ledger();
        }
        started
    }

    /// Tell whoever asked for a run how its start went.
    fn report_launch(&mut self, asker: Asker, started: Result<u32>) {
        if let Some(r) = asker.reply {
            let result =
                started.map(|id| crate::protocol::Response::Text(id.to_string())).map_err(|e| format!("{e:#}"));
            self.send_msg(r, &crate::protocol::ServerMsg::Response { result });
            self.clients.remove(&r);
        } else if let (Err(e), Some(c)) = (started, asker.client) {
            self.toast(c, format!("{e:#}"));
        }
    }

    /// Open the run's tab with one pane per target, the first `rolling` running.
    fn start_run(
        &mut self,
        d: &Draft,
        id: u32,
        log: &Path,
        specs: Vec<(String, PaneSpec)>,
        rolling: usize,
        client: Option<ClientId>,
    ) -> Result<u32> {
        let s = &d.steps;
        let name = d.runbook.name.clone();
        let labels: Vec<String> = specs.iter().map(|(l, _)| l.clone()).collect();
        let ws = client
            .and_then(|c| self.clients.get(&c))
            .map(|c| c.workspace)
            .or_else(|| d.pane.and_then(|p| self.workspace_of(p)).map(|(wi, _)| self.workspaces[wi].id))
            .or_else(|| self.primary.and_then(|c| self.clients.get(&c)).map(|c| c.workspace))
            .or_else(|| self.workspaces.first().map(|w| w.id))
            .unwrap_or_else(|| self.new_workspace("main".into(), None));
        let log = log.to_path_buf();
        let mut items: Vec<RunItem> = Vec::new();
        for (i, (label, spec)) in specs.into_iter().enumerate() {
            // A target that could not start stops the others of its batch from starting, as a
            // failure would (they wait, then are skipped).
            let failed_already = s.stop_on_failure && items.iter().any(|it| it.state.failed());
            let now = i < rolling && !failed_already;
            let spawned = if now {
                self.spawn_pane(Some(label.clone()), spec)
            } else {
                let reason = format!("waiting for its turn ({rolling} at a time)");
                self.spawn_pane_suspended(Some(label.clone()), spec, &reason)
            };
            match spawned {
                Ok(pane) => {
                    // A command that could not start gets a placeholder pane: it failed.
                    let state = match (now, self.panes.get(&pane).is_some_and(|p| p.suspended)) {
                        (true, true) => ItemState::Failed(None),
                        (true, false) => ItemState::Running,
                        (false, _) => ItemState::Waiting,
                    };
                    let process = if state == ItemState::Running {
                        self.panes.get(&pane).and_then(|p| p.process_hold())
                    } else {
                        None
                    };
                    items.push(RunItem { label, pane, state, started: now.then(Instant::now), process, killed: None });
                }
                Err(e) => {
                    for it in &items {
                        if let Some(mut p) = self.panes.remove(&it.pane) {
                            p.kill();
                        }
                    }
                    return Err(e.context("could not start"));
                }
            }
        }
        let ids: Vec<PaneId> = items.iter().map(|i| i.pane).collect();
        // Each target's output goes to the log as it comes, not only what its pane still shows.
        for (n, pane) in ids.iter().enumerate() {
            match crate::runbooks::OutputLog::create(log.with_extension(format!("{n}.out"))) {
                Ok(out) => {
                    self.run_output.insert(*pane, out);
                }
                Err(e) => self.warnings.push(format!("runbook log {}: {e}", log.display())),
            }
        }
        let tab_id = self.alloc_id();
        let mut tab = Tab::new(tab_id, format!("▶ {name}"));
        tab.root = Some(crate::layout::grid(&ids));
        tab.set_focus(ids[0]);
        let wi = self.ws_index(ws).context("workspace vanished")?;
        self.workspaces[wi].tabs.push(tab);
        self.workspaces[wi].active = self.workspaces[wi].tabs.len() - 1;
        if let Some(c) = client.and_then(|c| self.clients.get_mut(&c)) {
            c.workspace = ws;
        }
        self.relayout();
        self.mark_persist();
        let params: serde_json::Map<String, serde_json::Value> = d
            .values
            .iter()
            .map(|(k, v)| (k.clone(), json!(runbooks::shown_value(d.steps.params.iter().find(|p| &p.name == k), v))))
            .collect();
        self.runs.push(Run {
            id,
            name: name.clone(),
            tab: tab_id,
            items,
            rolling,
            stop_on_failure: s.stop_on_failure,
            timeout: s.timeout(),
            log: log.clone(),
            aborted: false,
        });
        self.emit(
            "runbook.started",
            json!({ "run": id, "name": name, "targets": labels, "params": params, "log": log.display().to_string() }),
        );
        // Targets that could not start count right away.
        let ri = self.runs.len() - 1;
        for pane in ids {
            if self.runs[ri].items.iter().any(|i| i.pane == pane && i.state.failed()) {
                self.log_item(ri, pane, "could not start");
            }
        }
        self.advance_run(ri);
        Ok(id)
    }

    pub(super) fn log_header(&self, d: &Draft, labels: &[String], rolling: usize) -> String {
        let s = &d.steps;
        let mut h = format!(
            "runbook   {}  ({})\nstarted   {}  by {} on {}\ntargets   {}  ({rolling} at a time{})\n",
            d.runbook.name,
            paths::shorten_home(&d.runbook.path),
            runbooks::local_time(SystemTime::now()),
            std::env::var("USER").unwrap_or_default(),
            crate::context::hostname().unwrap_or_default(),
            labels.join(", "),
            if s.stop_on_failure { ", stop at the first failure" } else { "" },
        );
        for v in d.shown_values() {
            h.push_str(&format!("param     {v}\n"));
        }
        h.push_str("steps\n");
        for (i, st) in s.steps.iter().filter(|st| !st.trim().is_empty()).enumerate() {
            for (j, line) in st.lines().enumerate() {
                h.push_str(&if j == 0 { format!("  {}. {line}\n", i + 1) } else { format!("     {line}\n") });
            }
        }
        h
    }

    /// Add to a run's log (through the log writer: in order, off the event loop).
    pub(super) fn append_log(&mut self, log: &Path, text: &str) {
        let _ = self.run_logs.send(crate::runbooks::LogJob::Append(log.to_path_buf(), text.to_string()));
    }

    /// Record a finished target in the run's log, with all it printed.
    pub(super) fn log_item(&mut self, ri: usize, pane: PaneId, result: &str) {
        let Some(item) = self.runs[ri].items.iter().find(|i| i.pane == pane) else { return };
        let took = item.started.map(|s| format!(" · {:.1}s", s.elapsed().as_secs_f64())).unwrap_or_default();
        let header = format!("\n--- {} · {result}{took}\n", item.label);
        let log = self.runs[ri].log.clone();
        let Some(out) = self.run_output.remove(&pane) else {
            // No stream (it could not be created): what the pane still shows.
            let output = self.panes.get(&pane).map(|p| p.text(Some(5000))).unwrap_or_default();
            self.append_log(&log, &format!("{header}{}\n", output.trim_end()));
            return;
        };
        let (output, error) = out.finish();
        if let Some(e) = error {
            self.warnings.push(format!("the log of {} misses part of its output: {e}", self.runs[ri].name));
        }
        let _ = self.run_logs.send(crate::runbooks::LogJob::Fold { log, header, output });
    }

    /// Write the runs in progress to disk, for a server that starts after this one died.
    pub(super) fn save_ledger(&mut self) {
        let runs = self.ledger_entries();
        let _ = self.run_logs.send(crate::runbooks::LogJob::Ledger(self.session.clone(), runs));
    }

    /// The ledger's entries for the runs in progress, and those on their way to start.
    fn ledger_entries(&self) -> Vec<crate::runbooks::LedgerRun> {
        self.runs
            .iter()
            .map(|r| crate::runbooks::LedgerRun {
                id: r.id,
                name: r.name.clone(),
                log: r.log.clone(),
                targets: r
                    .items
                    .iter()
                    .map(|i| crate::runbooks::LedgerTarget {
                        label: i.label.clone(),
                        pending: i.state.pending(),
                        output: self.run_output.get(&i.pane).map(|o| o.path.clone()),
                    })
                    .collect(),
            })
            .chain(self.launching.iter().cloned())
            .collect()
    }

    /// A pane of a run finished: record it, then start the next targets or stop.
    pub(in crate::server) fn runbook_pane_exited(&mut self, pane: PaneId, code: Option<i32>) {
        // The placeholder of a waiting target ending is not the target running.
        if self.panes.get(&pane).is_none_or(|p| p.suspended) {
            return;
        }
        let Some(ri) = self.runs.iter().position(|r| r.items.iter().any(|i| i.pane == pane)) else { return };
        let item = self.runs[ri].items.iter_mut().find(|i| i.pane == pane).unwrap();
        if !item.state.pending() {
            return;
        }
        let stopping = matches!(item.state, ItemState::Stopping(_));
        // Stopped for its timeout, but what it started still runs: it keeps its slot until the
        // timeout check has stopped all of it.
        if stopping && item.process.as_ref().is_some_and(|p| p.group_running()) {
            return;
        }
        item.state = match code {
            _ if stopping => ItemState::TimedOut,
            Some(0) => ItemState::Ok,
            _ => ItemState::Failed(code),
        };
        let result = match code {
            _ if stopping => "timed out, stopped".to_string(),
            Some(0) => "ok".to_string(),
            Some(c) => format!("failed (exit {c})"),
            None => "killed".to_string(),
        };
        self.log_item(ri, pane, &result);
        self.advance_run(ri);
    }

    /// A pane of a run was closed while it ran or waited: nothing more starts. Targets already
    /// running finish (stopping a deploy halfway can be worse than letting it end).
    pub(in crate::server) fn runbook_pane_closed(&mut self, pane: PaneId) {
        let Some(ri) = self.runs.iter().position(|r| r.items.iter().any(|i| i.pane == pane)) else { return };
        let run = &mut self.runs[ri];
        let Some(item) = run.items.iter_mut().find(|i| i.pane == pane) else { return };
        let was_running = matches!(item.state, ItemState::Running | ItemState::Stopping(_));
        // Being stopped for its timeout, it goes on being stopped without its pane: what ignores
        // the hangup of the pane still gets SIGKILL after the grace period.
        if let (ItemState::Stopping(since), Some(process)) = (item.state, &item.process)
            && process.group_running()
        {
            self.stray_groups.push(super::StrayGroup { process: process.clone(), since, killed: item.killed });
        }
        item.state = match item.state {
            ItemState::Running | ItemState::Stopping(_) => ItemState::Failed(None),
            ItemState::Waiting => ItemState::Skipped,
            // Closing the pane of a target that finished changes nothing.
            _ => return,
        };
        run.aborted = true;
        let still = run.items.iter().filter(|i| i.state == ItemState::Running).count();
        let name = run.name.clone();
        let log = run.log.clone();
        if was_running {
            self.log_item(ri, pane, "its pane was closed while it ran");
        }
        self.append_log(&log, "\n--- the run was stopped: a pane was closed\n");
        if still > 0 {
            let ids: Vec<ClientId> = self.clients.keys().copied().collect();
            for c in ids {
                self.toast(c, format!("{name} stopped: {still} running target(s) will finish, nothing more starts"));
            }
        }
        self.advance_run(ri);
    }

    /// Stop targets that ran longer than the runbook's `timeout`: SIGTERM to what runs in their
    /// pane, then SIGKILL if it is still there after a grace period. A target counts as timed
    /// out once its process, and everything it started in its process group, has really ended.
    ///
    /// Only the target's own process is signalled through its pane: one restarted there by hand
    /// is another run. And a group still there a while after SIGKILL holds only processes that
    /// cannot end (stuck in the kernel): the target is then counted as stopped rather than
    /// keeping its slot for ever. The waits count from the signals themselves, so a server held
    /// up for a while (suspended, swapping) still sends SIGKILL before giving up.
    pub(in crate::server) fn check_run_timeouts(&mut self) {
        let mut term = Vec::new();
        let mut kill = Vec::new();
        let mut stopped = Vec::new();
        let mut changed = Vec::new();
        for run in &mut self.runs {
            let Some(limit) = run.timeout else { continue };
            for item in &mut run.items {
                // The process the target started, if its pane still runs that one.
                let own = self.panes.get(&item.pane).is_some_and(|p| {
                    p.exit_code.is_none()
                        && p.process_hold().zip(item.process.as_ref()).is_some_and(|(now, its)| now.is(its))
                });
                match item.state {
                    ItemState::Running if item.started.is_some_and(|s| s.elapsed() > limit) => {
                        item.state = ItemState::Stopping(Instant::now());
                        item.killed = None;
                        term.push((run.log.clone(), item.label.clone(), item.pane, item.process.clone(), own));
                        changed.push(run.id);
                    }
                    ItemState::Stopping(since) => {
                        if !own && !item.process.as_ref().is_some_and(|p| p.group_running()) {
                            stopped.push((run.id, item.pane, "timed out, stopped"));
                            continue;
                        }
                        match item.killed {
                            None if since.elapsed() > STOP_GRACE => {
                                item.killed = Some(Instant::now());
                                kill.push((item.pane, item.process.clone(), own));
                            }
                            Some(at) if at.elapsed() > KILL_WAIT => {
                                stopped.push((run.id, item.pane, "timed out; some of its processes do not end"));
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
        }
        for (log, label, pane, process, own) in term {
            self.append_log(&log, &format!("\n--- {label} · over its timeout: stopping it\n"));
            self.signal_target(pane, process.as_ref(), own, libc::SIGTERM);
        }
        for (pane, process, own) in kill {
            self.signal_target(pane, process.as_ref(), own, libc::SIGKILL);
        }
        // The targets whose panes were closed while they were being stopped.
        self.stray_groups.retain_mut(|g| {
            if !g.process.group_running() {
                return false;
            }
            match g.killed {
                None if g.since.elapsed() > STOP_GRACE => {
                    g.process.signal(libc::SIGKILL);
                    g.killed = Some(Instant::now());
                    true
                }
                Some(at) => at.elapsed() <= KILL_WAIT,
                None => true,
            }
        });
        // Its command ended earlier, and now the rest of it: it timed out.
        for (id, pane, how) in stopped {
            let Some(ri) = self.runs.iter().position(|r| r.id == id) else { continue };
            if let Some(item) = self.runs[ri].items.iter_mut().find(|i| i.pane == pane) {
                item.state = ItemState::TimedOut;
            }
            self.log_item(ri, pane, how);
            changed.push(id);
        }
        // A timeout is a failure now: with stop_on_failure, what waits is skipped at once.
        changed.dedup();
        for id in changed {
            if let Some(ri) = self.runs.iter().position(|r| r.id == id) {
                self.advance_run(ri);
            }
        }
    }

    /// Move a run forward: skip what must not run, start what may, until nothing changes; then
    /// finish it if no target is left. Each pass changes the state of at least one waiting
    /// target, so this ends.
    pub(super) fn advance_run(&mut self, ri: usize) {
        // A target that is done lets its process go (to be reaped when its pane does too).
        if let Some(run) = self.runs.get_mut(ri) {
            for item in run.items.iter_mut().filter(|i| !i.state.pending()) {
                item.process = None;
            }
        }
        loop {
            let Some(run) = self.runs.get(ri) else { return };
            // A target over its timeout has failed already, while it is being stopped.
            let failing = |s: ItemState| s.failed() || matches!(s, ItemState::Stopping(_));
            let stop = run.aborted || (run.stop_on_failure && run.items.iter().any(|i| failing(i.state)));
            let waiting: Vec<PaneId> =
                run.items.iter().filter(|i| i.state == ItemState::Waiting).map(|i| i.pane).collect();
            if waiting.is_empty() {
                break;
            }
            if stop {
                let why = if run.aborted { "the run was stopped" } else { "an earlier target failed" };
                for p in waiting {
                    self.skip_pane(ri, p, why);
                }
                break;
            }
            // One being stopped still runs: it keeps its slot until it is gone.
            let running =
                run.items.iter().filter(|i| matches!(i.state, ItemState::Running | ItemState::Stopping(_))).count();
            let room = run.rolling.saturating_sub(running);
            if room == 0 {
                break;
            }
            for p in waiting.into_iter().take(room) {
                self.start_waiting(ri, p);
                // One that could not start is a failure: the loop sees it before starting more.
                let run = &self.runs[ri];
                if run.stop_on_failure && run.items.iter().any(|i| i.pane == p && i.state.failed()) {
                    break;
                }
            }
        }
        if self.runs[ri].items.iter().any(|i| i.state.pending()) {
            self.save_ledger();
        } else {
            self.finish_run(ri);
        }
        self.mark_dirty();
    }

    /// Start a waiting target; a target that cannot start has failed.
    pub(super) fn start_waiting(&mut self, ri: usize, pane: PaneId) {
        // The user may have started a waiting pane by hand: then it already runs.
        let started = match self.panes.get_mut(&pane) {
            Some(p) if p.suspended => p.respawn(&self.env, &self.tx).map_err(|e| format!("{e:#}")),
            Some(_) => Ok(()),
            None => Err("its pane is gone".into()),
        };
        let process = self.panes.get(&pane).and_then(|p| p.process_hold());
        let Some(item) = self.runs[ri].items.iter_mut().find(|i| i.pane == pane) else { return };
        item.started = Some(Instant::now());
        match started {
            Ok(()) => {
                item.state = ItemState::Running;
                item.process = process;
            }
            Err(e) => {
                item.state = ItemState::Failed(None);
                self.log_item(ri, pane, &format!("could not start: {e}"));
            }
        }
    }

    /// A waiting target that will not run: its pane says so instead of offering to start it.
    pub(super) fn skip_pane(&mut self, ri: usize, pane: PaneId, why: &str) {
        if let Some(item) = self.runs[ri].items.iter_mut().find(|i| i.pane == pane) {
            item.state = ItemState::Skipped;
        }
        let Some(p) = self.panes.get_mut(&pane) else { return };
        let msg = shell::quote(&format!("skipped: {why}"));
        p.spec = PaneSpec {
            command: vec!["sh".into(), "-c".into(), format!("printf '\\033[2m%s\\033[0m\\n' {msg}")],
            ..Default::default()
        };
        p.name = p.name.take().map(|n| format!("{n} (skipped)"));
        let _ = p.respawn(&self.env, &self.tx);
    }

    pub(super) fn finish_run(&mut self, ri: usize) {
        let run = self.runs.remove(ri);
        // Targets that never ran (skipped) have nothing worth keeping.
        for item in &run.items {
            if let Some(out) = self.run_output.remove(&item.pane) {
                let _ = std::fs::remove_file(out.finish().0);
            }
        }
        self.save_ledger();
        let count = |f: &dyn Fn(ItemState) -> bool| run.items.iter().filter(|i| f(i.state)).count();
        let ok = count(&|s| s == ItemState::Ok);
        let failed = count(&|s| s.failed());
        let skipped = count(&|s| s == ItemState::Skipped);
        let mut summary = format!("{ok} ok");
        if failed > 0 {
            summary.push_str(&format!(", {failed} failed"));
        }
        if skipped > 0 {
            summary.push_str(&format!(", {skipped} skipped"));
        }
        self.append_log(&run.log, &format!("\n\nfinished  {}  {summary}\n", runbooks::local_time(SystemTime::now())));
        let mark = if failed == 0 && skipped == 0 { "✓" } else { "✗" };
        for ws in &mut self.workspaces {
            if let Some(t) = ws.tabs.iter_mut().find(|t| t.id == run.tab) {
                t.name = format!("{mark} {}", run.name);
            }
        }
        // Announced once its log is complete: whoever reads it then reads all of it.
        let fields = json!({
            "run": run.id,
            "name": run.name,
            "ok": ok,
            "failed": failed,
            "skipped": skipped,
            "log": run.log.display().to_string(),
        });
        let tx = self.tx.clone();
        let _ = self.run_logs.send(crate::runbooks::LogJob::Then(Box::new(move || {
            let _ = tx.send(super::super::Event::Emit("runbook.finished", fields));
        })));
        self.notify(&format!("runbook {}", run.name), &summary, None);
        self.mark_dirty();
    }

    /// kraal is stopping: say so in the logs of runs in progress.
    pub(in crate::server) fn interrupt_runs(&mut self) {
        self.finish_stopping();
        for ri in 0..self.runs.len() {
            let pending: Vec<PaneId> =
                self.runs[ri].items.iter().filter(|i| i.state.pending()).map(|i| i.pane).collect();
            for pane in pending {
                self.log_item(ri, pane, "interrupted");
            }
            let when = runbooks::local_time(SystemTime::now());
            let log = self.runs[ri].log.clone();
            self.append_log(&log, &format!("\n\ninterrupted  {when}  kraal stopped while the run was in progress\n"));
        }
        for out in std::mem::take(&mut self.run_output).into_values() {
            let _ = std::fs::remove_file(out.finish().0);
        }
        self.runs.clear();
        self.save_ledger();
    }
}

/// How long a target over its timeout has, after SIGTERM, before SIGKILL.
const STOP_GRACE: Duration = Duration::from_secs(5);
/// How long after SIGKILL a target still not gone is given up on.
const KILL_WAIT: Duration = Duration::from_secs(10);

impl Server {
    /// Send `signal` to a target: through its pane when the pane still runs the target's own
    /// process (its foreground job too), and to the process group it started in.
    fn signal_target(&self, pane: PaneId, process: Option<&crate::pane::ProcessHold>, own: bool, signal: i32) {
        if let Some(p) = self.panes.get(&pane).filter(|_| own) {
            p.signal(signal);
        }
        if let Some(process) = process {
            process.signal(signal);
        }
    }

    /// Kraal is stopping: targets being stopped for their timeout are stopped for good first.
    /// What ignored SIGTERM and the hangup gets the rest of its grace period, then SIGKILL.
    fn finish_stopping(&mut self) {
        let mut stopping: Vec<(crate::pane::ProcessHold, Instant)> = self
            .runs
            .iter()
            .flat_map(|r| &r.items)
            .filter_map(|i| match (i.state, &i.process) {
                (ItemState::Stopping(since), Some(p)) => Some((p.clone(), since)),
                _ => None,
            })
            .collect();
        stopping.extend(self.stray_groups.drain(..).map(|g| (g.process, g.since)));
        let start = Instant::now();
        loop {
            stopping.retain(|(p, _)| p.group_running());
            let due = |since: &Instant| since.elapsed() > STOP_GRACE || start.elapsed() > STOP_GRACE;
            if stopping.iter().all(|(_, since)| due(since)) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        for (process, _) in &stopping {
            process.signal(libc::SIGKILL);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_group_is_alive_while_anything_in_it_runs() {
        use std::os::unix::process::CommandExt;
        // A leader that ends at once, leaving a process that ignores SIGTERM in its group.
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("trap '' TERM; sleep 30 & exit 0")
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = child.id() as libc::pid_t;
        child.wait().unwrap();
        let running = || crate::process::group_running(pgid);
        assert!(running());
        // SAFETY: plain syscalls on the group this test made.
        unsafe { libc::kill(-pgid, libc::SIGTERM) };
        std::thread::sleep(Duration::from_millis(200));
        assert!(running(), "SIGTERM is ignored");
        unsafe { libc::kill(-pgid, libc::SIGKILL) };
        let start = Instant::now();
        while running() && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!running());
    }
}
