//! Events: what happens in the session, streamed to `kraal events` and run through the
//! `[hooks]` of the config.
//!
//! Every event carries `seq`, increasing within a server's life, and `boot`, which changes when
//! the server restarts. The last events are kept, so a subscriber that was dropped (too slow)
//! or reconnects can ask for what it missed with `since`.

use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossbeam_channel::Sender;
use serde_json::{Map, Value, json};

use super::{ClientId, Server};
use crate::layout::PaneId;
use crate::protocol::{self, ServerMsg};

/// Every event type, for checking filters and hooks.
pub const TYPES: &[&str] = &[
    "workspace.opened",
    "workspace.closed",
    "pane.opened",
    "pane.exited",
    "pane.failed",
    "pane.closed",
    "agent.working",
    "agent.blocked",
    "agent.done",
    "agent.idle",
    "ssh.started",
    "ssh.ended",
    "production.entered",
    "production.left",
    "notification",
    "tunnel.up",
    "tunnel.down",
    "port.opened",
    "port.closed",
    "account.limit",
    "runbook.started",
    "runbook.finished",
    "command.finished",
];

/// Events kept for subscribers that ask for what they missed.
const KEPT: usize = 1024;

/// Whether `pattern` (`pane.exited`, `agent.*`, `*`) selects events of type `ty`.
pub fn matches(pattern: &str, ty: &str) -> bool {
    if pattern == "*" || pattern == ty {
        return true;
    }
    pattern.strip_suffix(".*").is_some_and(|p| ty.strip_prefix(p).is_some_and(|rest| rest.starts_with('.')))
}

/// Patterns that select no event: typos.
pub fn unknown_patterns<'a>(patterns: impl Iterator<Item = &'a String>) -> Vec<String> {
    patterns.filter(|p| !TYPES.iter().any(|t| matches(p, t))).cloned().collect()
}

fn selected(filters: &[String], ty: &str) -> bool {
    filters.is_empty() || filters.iter().any(|f| matches(f, ty))
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

pub struct Subscriber {
    pub client: ClientId,
    pub out: Sender<Vec<u8>>,
    pub filters: Vec<String>,
}

/// Numbering of events and the last ones, for replay.
pub struct EventLog {
    boot: u64,
    seq: u64,
    kept: VecDeque<Map<String, Value>>,
}

impl Default for EventLog {
    fn default() -> Self {
        EventLog { boot: now_ms(), seq: 0, kept: VecDeque::new() }
    }
}

impl EventLog {
    /// Number an event and keep it.
    fn record(&mut self, ty: &str, fields: Value) -> Map<String, Value> {
        self.seq += 1;
        let mut event = Map::new();
        event.insert("v".into(), json!(1));
        event.insert("seq".into(), json!(self.seq));
        event.insert("boot".into(), json!(self.boot));
        event.insert("type".into(), json!(ty));
        event.insert("time".into(), json!(now_ms()));
        if let Value::Object(f) = fields {
            event.extend(f);
        }
        if self.kept.len() == KEPT {
            self.kept.pop_front();
        }
        self.kept.push_back(event.clone());
        event
    }

    /// Kept events after `since`, and whether some in between were already forgotten.
    fn after(&self, since: u64) -> (Vec<&Map<String, Value>>, bool) {
        let oldest = self.kept.front().and_then(|e| e["seq"].as_u64()).unwrap_or(self.seq + 1);
        let lost = since.saturating_add(1) < oldest && since < self.seq;
        let events = self.kept.iter().filter(|e| e["seq"].as_u64().is_some_and(|s| s > since)).collect();
        (events, lost)
    }
}

/// `[hooks]` of the config, flattened: `agent.blocked = "…"` and `[hooks.agent] blocked = "…"`
/// are the same. Returns warnings for values that are not commands.
pub fn read_hooks(table: &toml::Table) -> (Vec<(String, String)>, Vec<String>) {
    fn walk(prefix: &str, t: &toml::Table, out: &mut Vec<(String, String)>, warnings: &mut Vec<String>) {
        for (k, v) in t {
            let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
            match v {
                toml::Value::String(cmd) => out.push((key, cmd.clone())),
                toml::Value::Table(t) => walk(&key, t, out, warnings),
                _ => warnings.push(format!("hooks.{key}: expected a command line")),
            }
        }
    }
    let mut out = Vec::new();
    let mut warnings = Vec::new();
    walk("", table, &mut out, &mut warnings);
    for p in unknown_patterns(out.iter().map(|(p, _)| p)) {
        warnings.push(format!("hooks.{p}: no such event (see `kraal events --list`)"));
    }
    (out, warnings)
}

/// Hook processes, launched in the background and reaped by the tick.
#[derive(Default)]
pub struct Hooks {
    pub list: Vec<(String, String)>,
    running: Vec<Running>,
    recent: VecDeque<Instant>,
    warned: Option<Instant>,
}

/// A hook process, the readers of its output, and where that output goes.
struct Running {
    child: Child,
    started: Instant,
    output: Option<crate::exec::Drains>,
    log: PathBuf,
}

impl Running {
    /// Write what it printed to the log, off the event loop (a background descendant may hold
    /// its pipes open a little while).
    fn log_output(&mut self) {
        let (Some(drains), log) = (self.output.take(), self.log.clone()) else { return };
        std::thread::spawn(move || {
            let (out, err, _) = drains.finish(Duration::from_millis(500));
            if (!out.is_empty() || !err.is_empty())
                && let Ok(mut f) = open_log(&log)
            {
                let _ = f.write_all(&out);
                let _ = f.write_all(&err);
            }
        });
    }
}

/// More launches than this per second are dropped (a hook that triggers its own event)...
const MAX_PER_SECOND: usize = 20;
/// ...and so are launches beyond this many hooks running at once.
const MAX_RUNNING: usize = 32;
const MAX_RUNTIME: Duration = Duration::from_secs(600);
/// Output kept in hooks.log per hook and stream.
const OUTPUT_LIMIT: usize = 64 << 10;

impl Hooks {
    pub fn new(list: Vec<(String, String)>) -> Hooks {
        Hooks { list, ..Default::default() }
    }

    /// Forget finished hooks; stop (with everything they started) those running too long. A hook
    /// runs until its process group is empty: what it left in the background still counts.
    pub fn reap(&mut self) {
        self.running.retain_mut(|h| {
            let done = matches!(h.child.try_wait(), Ok(Some(_))) && !crate::exec::group_alive(&h.child);
            if !done && h.started.elapsed() <= MAX_RUNTIME {
                return true;
            }
            if !done {
                crate::exec::kill_group(&mut h.child);
            }
            h.log_output();
            false
        });
    }

    /// Whether hooks run (they are looked at every tick, to be stopped in time).
    pub fn busy(&self) -> bool {
        !self.running.is_empty()
    }

    /// kraal is stopping: so do the hooks.
    pub fn stop_all(&mut self) {
        for h in &mut self.running {
            crate::exec::kill_group(&mut h.child);
            h.log_output();
        }
        self.running.clear();
    }

    /// Whether another hook may start now; says why not otherwise.
    fn admit(&mut self) -> Result<(), &'static str> {
        let now = Instant::now();
        while self.recent.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(1)) {
            self.recent.pop_front();
        }
        if self.recent.len() >= MAX_PER_SECOND {
            return Err("too many events at once");
        }
        if self.running.len() >= MAX_RUNNING {
            return Err("too many hooks still running");
        }
        self.recent.push_back(now);
        Ok(())
    }
}

/// `KRAAL_<FIELD>` variables of an event's scalar fields (lists are joined with spaces).
pub fn hook_env(event: &Map<String, Value>) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for (k, v) in event {
        let value = match v {
            Value::String(s) => s.clone(),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => if *b { "1" } else { "" }.to_string(),
            Value::Null => String::new(),
            Value::Array(a) => {
                a.iter().map(|x| x.as_str().map(str::to_string).unwrap_or(x.to_string())).collect::<Vec<_>>().join(" ")
            }
            Value::Object(_) => continue,
        };
        let name = match k.as_str() {
            "type" => "EVENT".to_string(),
            other => other.to_ascii_uppercase(),
        };
        out.push((format!("KRAAL_{name}"), value));
    }
    out
}

fn hooks_log() -> PathBuf {
    crate::paths::state_dir().join("hooks.log")
}

/// The hooks log, for appending. Commands and what they print may hold secrets (a webhook
/// token): the file is only ours, in a directory only ours, and never a link placed there.
fn open_log(log: &Path) -> std::io::Result<std::fs::File> {
    if let Some(dir) = log.parent() {
        crate::paths::ensure_private_dir(dir)?;
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.append(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        let f = opts.open(log)?;
        // A log an older kraal created readable by others.
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        Ok(f)
    }
    #[cfg(not(unix))]
    opts.open(log)
}

fn rotate(log: &Path) {
    if std::fs::metadata(log).is_ok_and(|m| m.len() > 1 << 20) {
        let _ = std::fs::rename(log, log.with_extension("log.1"));
    }
}

/// A frame of replayed events.
fn events_frame(events: Vec<Value>) -> Result<Vec<u8>, String> {
    let mut frame = Vec::new();
    protocol::write_json(&mut frame, &ServerMsg::Events { events })
        .map_err(|e| format!("could not replay the events: {e}"))?;
    Ok(frame)
}

impl Server {
    /// Something happened: number it, tell subscribers and run the matching hooks.
    pub fn emit(&mut self, ty: &str, fields: Value) {
        let event = self.events.record(ty, fields);
        if !self.subscribers.is_empty() {
            let mut frame = Vec::new();
            if protocol::write_json(&mut frame, &ServerMsg::Event { event: Value::Object(event.clone()) }).is_ok() {
                // A subscriber too slow to keep up is dropped rather than slowing the session;
                // it can come back with `since` and replay what it missed.
                self.subscribers.retain(|s| !selected(&s.filters, ty) || s.out.try_send(frame.clone()).is_ok());
            }
        }
        let commands: Vec<String> =
            self.hooks.list.iter().filter(|(p, _)| matches(p, ty)).map(|(_, c)| c.clone()).collect();
        for cmd in commands {
            self.run_hook(&cmd, &event);
        }
    }

    fn run_hook(&mut self, cmd: &str, event: &Map<String, Value>) {
        if let Err(why) = self.hooks.admit() {
            if self.hooks.warned.is_none_or(|t| t.elapsed() > Duration::from_secs(60)) {
                self.hooks.warned = Some(Instant::now());
                self.warnings.push(format!("hooks: {why}, some were skipped"));
            }
            return;
        }
        let log = hooks_log();
        rotate(&log);
        if let Ok(mut f) = open_log(&log) {
            let ty = event.get("type").and_then(|t| t.as_str()).unwrap_or("");
            let _ = writeln!(f, "--- {} {ty}: {cmd}", now_ms());
        }
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(cmd)
            .envs(hook_env(event))
            .env("KRAAL_EVENT_JSON", Value::Object(event.clone()).to_string())
            .env("KRAAL_SOCKET", &self.env.socket)
            .env("KRAAL_SESSION", &self.env.session)
            .env("KRAAL_HOOK", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        match crate::exec::own_group(&mut command).spawn() {
            Ok(mut child) => {
                let output = Some(crate::exec::Drains::start(&mut child, OUTPUT_LIMIT));
                self.hooks.running.push(Running { child, started: Instant::now(), output, log });
            }
            Err(e) => self.warnings.push(format!("hook {cmd:?}: {e}")),
        }
    }

    /// Fields that describe a pane in events.
    pub fn pane_fields(&self, id: PaneId) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("pane".into(), json!(id));
        if let Some(p) = self.panes.get(&id) {
            m.insert("name".into(), json!(p.display_name()));
        }
        if let Some((wi, _)) = self.workspace_of(id) {
            m.insert("workspace".into(), json!(self.workspaces[wi].name));
        }
        m
    }

    pub fn emit_pane(&mut self, ty: &str, id: PaneId, extra: Value) {
        let mut m = self.pane_fields(id);
        if let Value::Object(e) = extra {
            m.extend(e);
        }
        self.emit(ty, Value::Object(m));
    }

    /// Say `pane.opened` once for a pane: at the first tick after it has its place in a
    /// workspace, or before anything else is said about it.
    pub(super) fn announce_pane(&mut self, id: PaneId) {
        if !self.panes.contains_key(&id) || !self.announced.insert(id) {
            return;
        }
        let Some(p) = self.panes.get(&id) else { return };
        let command = p.spec.command.clone();
        let cwd = p.spec.cwd.as_ref().map(|c| c.display().to_string());
        let kind = match &p.content {
            crate::pane::Content::Term(_) => "terminal",
            crate::pane::Content::Preview(_) => "preview",
            crate::pane::Content::Files(_) => "files",
        };
        self.emit_pane("pane.opened", id, json!({ "command": command, "cwd": cwd, "kind": kind }));
    }

    /// Start streaming events to a `kraal events` client, after replaying those after `since`.
    pub(super) fn subscribe(
        &mut self,
        client: ClientId,
        filters: Vec<String>,
        since: Option<u64>,
        boot: Option<u64>,
    ) -> Result<(), String> {
        let unknown = unknown_patterns(filters.iter());
        if !unknown.is_empty() {
            return Err(format!("no such event: {} (see `kraal events --list`)", unknown.join(", ")));
        }
        if !self.clients.contains_key(&client) {
            return Err("no client".into());
        }
        let mut frames = Vec::new();
        let mut ok = Vec::new();
        if protocol::write_json(&mut ok, &ServerMsg::Response { result: Ok(protocol::Response::Ok) }).is_ok() {
            frames.push(ok);
        }
        // A cursor from before a restart means nothing now: everything kept is new to it.
        let restarted = boot.is_some_and(|b| b != self.events.boot);
        let since = if restarted { Some(0) } else { since };
        if let Some(since) = since {
            let (missed, lost) = self.events.after(since);
            let mut replay = Vec::new();
            if lost || restarted {
                let reason = if restarted { "kraal restarted" } else { "too old" };
                replay.push(json!({ "v": 1, "type": "events.lost", "boot": self.events.boot, "since": since, "reason": reason }));
            }
            replay.extend(
                missed
                    .into_iter()
                    .filter(|e| selected(&filters, e["type"].as_str().unwrap_or("")))
                    .map(|e| Value::Object(e.clone())),
            );
            // A few frames of at most a few MiB each for all of it (the replay buffer holds 1024
            // events), so it fits the client's queue whatever its length.
            const BATCH: usize = 4 << 20;
            let mut batch: Vec<Value> = Vec::new();
            let mut size = 0;
            for e in replay {
                let len = serde_json::to_vec(&e).map(|v| v.len()).unwrap_or(0);
                if size + len > BATCH && !batch.is_empty() {
                    frames.push(events_frame(std::mem::take(&mut batch))?);
                    size = 0;
                }
                size += len;
                batch.push(e);
            }
            if !batch.is_empty() {
                frames.push(events_frame(batch)?);
            }
        }
        // From here on the connection is a subscriber, no longer a client.
        let Some(c) = self.clients.remove(&client) else { return Err("no client".into()) };
        // Never wait on a client; a fresh connection has room for these few frames.
        for f in frames {
            if c.out.try_send(f).is_err() {
                return Err("the event stream could not start: try again".into());
            }
        }
        self.subscribers.push(Subscriber { client, out: c.out, filters });
        Ok(())
    }

    /// Changes noticed by polling: new panes, SSH sessions and production status.
    pub(super) fn diff_pane_states(&mut self) {
        let ids: Vec<PaneId> = self.panes.keys().copied().collect();
        for id in ids {
            self.announce_pane(id);
            let ssh = self.panes.get(&id).and_then(|p| p.ssh.as_ref().map(|s| s.name.clone()));
            let prod = self.pane_production_shown(id);
            let (old_ssh, old_prod) = self.pane_states.get(&id).cloned().unwrap_or_default();
            if ssh != old_ssh {
                if let Some(s) = &old_ssh {
                    self.emit_pane("ssh.ended", id, json!({ "server": s }));
                }
                if let Some(s) = &ssh {
                    let host = self.pane_server_entry(id).map(|e| e.destination()).unwrap_or_default();
                    let fields = json!({ "server": s, "host": host, "production": prod.is_some() });
                    self.emit_pane("ssh.started", id, fields);
                }
            }
            if prod != old_prod {
                match &prod {
                    Some(reason) => {
                        self.emit_pane("production.entered", id, json!({ "reason": reason, "previous": old_prod }))
                    }
                    None => self.emit_pane("production.left", id, json!({ "previous": old_prod })),
                }
            }
            self.pane_states.insert(id, (ssh, prod));
        }
        let panes = &self.panes;
        self.pane_states.retain(|id, _| panes.contains_key(id));
        self.announced.retain(|id| panes.contains_key(id));
    }
}

/// Remember what changed between two port lists.
pub fn port_changes(
    before: &std::collections::BTreeMap<u16, PaneId>,
    after: &std::collections::BTreeMap<u16, PaneId>,
) -> (Vec<(u16, PaneId)>, Vec<u16>) {
    let opened = after.iter().filter(|(p, _)| !before.contains_key(p)).map(|(p, id)| (*p, *id)).collect();
    let closed = before.keys().filter(|p| !after.contains_key(p)).copied().collect();
    (opened, closed)
}

pub type PaneStates = HashMap<PaneId, (Option<String>, Option<String>)>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns() {
        assert!(matches("agent.*", "agent.blocked"));
        assert!(!matches("agent.*", "agents.x"));
        assert!(matches("*", "pane.exited"));
        assert!(matches("pane.exited", "pane.exited"));
        assert!(!matches("pane", "pane.exited"));
        let bad = unknown_patterns(["agent.*".to_string(), "agent.blokced".to_string()].iter());
        assert_eq!(bad, vec!["agent.blokced"]);
    }

    #[test]
    fn hooks_flatten_and_env() {
        let t: toml::Table =
            toml::from_str("\"pane.failed\" = \"a\"\n[agent]\nblocked = \"b\"\n[ssh]\nstarted = 3\n").unwrap();
        let (hooks, warnings) = read_hooks(&t);
        assert_eq!(hooks, vec![("agent.blocked".into(), "b".into()), ("pane.failed".into(), "a".into())]);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let ev = json!({"type": "pane.exited", "pane": 3, "command": ["make", "test"], "production": false});
        let env = hook_env(ev.as_object().unwrap());
        assert!(env.contains(&("KRAAL_EVENT".into(), "pane.exited".into())));
        assert!(env.contains(&("KRAAL_PANE".into(), "3".into())));
        assert!(env.contains(&("KRAAL_COMMAND".into(), "make test".into())));
        assert!(env.contains(&("KRAAL_PRODUCTION".into(), String::new())));
    }

    #[test]
    fn numbering_and_replay() {
        let mut log = EventLog::default();
        for i in 0..(KEPT + 10) {
            log.record("pane.opened", json!({ "pane": i }));
        }
        let (events, lost) = log.after(5);
        assert!(lost, "events 6..10 were forgotten");
        assert_eq!(events.len(), KEPT);
        assert_eq!(events[0]["seq"], 11);
        let (events, lost) = log.after(log.seq - 2);
        assert!(!lost);
        assert_eq!(events.len(), 2);
        let (events, lost) = log.after(log.seq);
        assert!(events.is_empty() && !lost);
    }

    #[test]
    fn hook_logs_are_private() {
        let dir = std::env::temp_dir().join(format!("kraal-hooklog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("hooks.log");
        std::fs::write(&log, "old\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o644)).unwrap();
            let _ = open_log(&log).unwrap();
            assert_eq!(std::fs::metadata(&log).unwrap().permissions().mode() & 0o777, 0o600);
            assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
            std::fs::remove_file(&log).unwrap();
            std::os::unix::fs::symlink(dir.join("elsewhere"), &log).unwrap();
            assert!(open_log(&log).is_err(), "a link is not followed");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn hook_limits() {
        let mut h = Hooks::new(Vec::new());
        for _ in 0..MAX_PER_SECOND {
            assert!(h.admit().is_ok());
        }
        assert_eq!(h.admit(), Err("too many events at once"));
    }
}
