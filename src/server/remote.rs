//! Agents of kraal sessions on servers. A server whose entry in servers.toml names a session
//! (`agents = "main"`, which `kraal attach --host` sets) is watched over one ssh that runs
//! `kraal agents --follow` there and stays open: each line it prints is that session's whole
//! list of agents. When it drops, it is tried again later, sooner at first, and never with a
//! question (BatchMode). The sidebar, the pane picker and the attention queue show these agents
//! with the server's name; choosing one attaches to that session in a tab, its pane shown.
//! What a server says is bounded and cleaned before it is drawn.

use std::collections::HashMap;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;

use super::{ClientId, Event, Server};
use crate::agent::AgentState;
use crate::layout::PaneId;
use crate::pane::PaneSpec;
use crate::protocol::Placement;
use crate::servers;

/// A longer line is not a list kraal printed: it is dropped.
const MAX_LINE: usize = 4 << 20;
/// Agents kept of one server.
const MAX_AGENTS: usize = 64;
/// Columns kept of a name, a workspace or an agent's kind.
const MAX_NAME: usize = 48;
/// At most ten lists a second are taken from a server (the next one waits): `kraal agents
/// --follow` says less, and a server saying more never keeps the event loop busy.
const LIST_GAP: Duration = Duration::from_millis(100);

/// An agent of a kraal session on a server.
#[derive(Debug, Clone)]
pub struct RemoteAgent {
    /// Its pane's id there.
    pub pane: u32,
    pub name: String,
    pub workspace: String,
    pub agent: String,
    pub state: AgentState,
    /// The pane is the one its workspace shows there.
    pub focused: bool,
    /// Since when it is in this state, as seen here.
    pub since: Instant,
}

impl RemoteAgent {
    fn same(&self, other: &RemoteAgent) -> bool {
        (self.pane, &self.name, &self.workspace, &self.agent, self.state, self.focused)
            == (other.pane, &other.name, &other.workspace, &other.agent, other.state, other.focused)
    }
}

/// A pane as `kraal agents --json` describes it, as far as it is used here.
#[derive(Deserialize)]
struct Listed {
    id: u32,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    title: String,
    #[serde(default)]
    workspace: String,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    state: String,
    #[serde(default)]
    focused: bool,
}

/// Text from a server, safe to draw: control characters made visible, formatting ones (bidi
/// overrides, zero-width characters) dropped, cut to `max` columns.
pub fn clean(text: &str, max: usize) -> String {
    let invisible = |c: &char| matches!(c, '\u{200b}'..='\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2060}'..='\u{206f}' | '\u{feff}');
    let text: String = text.chars().filter(|c| !invisible(c)).take(max * 4).map(crate::text::visible_char).collect();
    crate::text::truncate(text.trim(), max)
}

/// One line of `kraal agents --follow`: its agents, cleaned, at most [`MAX_AGENTS`].
pub fn parse_list(line: &str, now: Instant) -> Option<Vec<RemoteAgent>> {
    let listed: Vec<Listed> = serde_json::from_str(line).ok()?;
    let agents = listed
        .into_iter()
        .filter_map(|p| {
            let agent = p.agent.filter(|a| !a.trim().is_empty())?;
            let name = p.name.filter(|n| !n.trim().is_empty()).unwrap_or(p.title);
            let name = if name.trim().is_empty() { agent.clone() } else { name };
            Some(RemoteAgent {
                pane: p.id,
                name: clean(&name, MAX_NAME),
                workspace: clean(&p.workspace, MAX_NAME),
                agent: clean(&agent, MAX_NAME),
                state: AgentState::parse(&p.state).unwrap_or_default(),
                focused: p.focused,
                since: now,
            })
        })
        .take(MAX_AGENTS)
        .collect();
    Some(agents)
}

/// A server whose agents are listed, and the connection that lists them.
pub struct Watch {
    /// Its entry: the name, how to reach it, and the session.
    pub server: servers::Server,
    pub session: String,
    pub agents: Vec<RemoteAgent>,
    /// Which connection this is: the lines of one already replaced are dropped.
    conn: u32,
    child: Option<Child>,
    stop_reading: Arc<AtomicBool>,
    /// The last line ssh (or the command there) wrote on stderr.
    said: Arc<Mutex<String>>,
    /// The states last seen, kept while it reconnects: what was seen is not announced again.
    seen: HashMap<u32, AgentState>,
    /// A list came since kraal started watching: until then, nothing is announced.
    heard: bool,
    retry_at: Option<Instant>,
    failures: u32,
    /// Why its agents cannot be listed was said already (once is enough).
    warned: bool,
}

impl Watch {
    fn new(server: servers::Server, session: String) -> Watch {
        Watch {
            server,
            session,
            agents: Vec::new(),
            conn: 0,
            child: None,
            stop_reading: Default::default(),
            said: Default::default(),
            seen: HashMap::new(),
            heard: false,
            retry_at: None,
            failures: 0,
            warned: false,
        }
    }

    /// Stop its ssh with what that started, and the readers of its output; how ssh ended, if
    /// it ran.
    fn stop(&mut self) -> Option<std::process::ExitStatus> {
        self.stop_reading.store(true, Ordering::SeqCst);
        let mut c = self.child.take()?;
        crate::exec::kill_group(&mut c);
        c.try_wait().ok().flatten()
    }

    /// Take the list a connection just gave: each agent keeps the time it got into its state.
    /// Says whether what is shown changed, and which agents are in another state than when last
    /// seen, with that state (none from the first list: they were not seen change).
    fn take(&mut self, mut list: Vec<RemoteAgent>) -> (bool, Vec<(RemoteAgent, Option<AgentState>)>) {
        for a in &mut list {
            if let Some(old) = self.agents.iter().find(|o| o.pane == a.pane && o.state == a.state) {
                a.since = old.since;
            }
        }
        let moved = if self.heard {
            list.iter()
                .map(|a| (a.clone(), self.seen.get(&a.pane).copied()))
                .filter(|(a, before)| *before != Some(a.state))
                .collect()
        } else {
            Vec::new()
        };
        self.seen = list.iter().map(|a| (a.pane, a.state)).collect();
        self.heard = true;
        self.failures = 0;
        let changed = self.agents.len() != list.len() || self.agents.iter().zip(&list).any(|(a, b)| !a.same(b));
        self.agents = list;
        (changed, moved)
    }

    /// The connection ended: its agents go until it is back (what was seen of them stays).
    fn lost(&mut self) -> bool {
        !std::mem::take(&mut self.agents).is_empty()
    }

    /// Wait before the next try: longer after each failure, at most a minute.
    fn back_off(&mut self) {
        self.failures += 1;
        self.retry_at = Some(Instant::now() + Duration::from_secs((2u64 << self.failures.min(5)).min(60)));
    }
}

/// ssh's arguments to list the agents of `session` on `s`: never a question (nobody is there to
/// answer one), a connection of its own (one shared with panes could hide that the link is
/// dead), a server that does not answer given up after 15 s, and a dead link seen within a
/// minute.
pub fn watch_args(s: &servers::Server, known: &[servers::Known], session: &str) -> Vec<String> {
    let mut args: Vec<String> = [
        "-T",
        "-o",
        "BatchMode=yes",
        "-o",
        "ConnectTimeout=15",
        "-o",
        "ServerAliveInterval=15",
        "-o",
        "ServerAliveCountMax=3",
        "-o",
        "ControlMaster=no",
        "-o",
        "ControlPath=none",
    ]
    .iter()
    .map(|a| a.to_string())
    .collect();
    args.extend(servers::ssh_args(s, known, false));
    args.push(s.destination());
    args.push(crate::client::remote_kraal(&format!("-s {} agents --follow", crate::shell::quote(session))));
    args
}

/// What to tell the user when a server's agents cannot be listed, from what ssh or the command
/// there said last.
fn why_not(name: &str, said: &str, status: Option<std::process::ExitStatus>) -> String {
    let said = clean(said, 200);
    let fix = format!("`kraal remote-install {name}` puts this kraal there");
    if said.contains("--follow") {
        format!("the kraal on {name} is too old to list its agents: {fix}")
    } else if status.and_then(|s| s.code()) == Some(127) || (said.contains("kraal") && said.contains("not found")) {
        format!("no kraal on {name} to list its agents: {fix}")
    } else if said.is_empty() {
        let status = status.map(|s| s.to_string()).unwrap_or_else(|| "ended".into());
        format!("the agents of {name} cannot be listed: ssh exited ({status})")
    } else {
        format!("the agents of {name} cannot be listed: {said}")
    }
}

impl Server {
    /// Watch the servers whose entry names a session, and only those; start what is due, and
    /// notice connections that ended (their agents go until it is back).
    pub(super) fn check_remotes(&mut self) {
        let wanted: Vec<(servers::Server, String)> = self
            .known_servers
            .iter()
            .filter_map(|k| k.server.agents_session().map(|session| (k.server.clone(), session.to_string())))
            .collect();
        if wanted.is_empty() && self.remotes.is_empty() {
            return;
        }
        let mut watches = std::mem::take(&mut self.remotes);
        let mut changed = false;
        // An entry gone, or that reaches its server otherwise now, is watched no more.
        watches.retain_mut(|w| {
            let keep = wanted
                .iter()
                .any(|(s, session)| s.name == w.server.name && s.same_connection(&w.server) && *session == w.session);
            if !keep {
                w.stop();
                changed |= w.lost();
            }
            keep
        });
        for (s, session) in wanted {
            if !watches.iter().any(|w| w.server.name == s.name) {
                watches.push(Watch::new(s, session));
            }
        }
        let now = Instant::now();
        for w in &mut watches {
            if w.child.as_ref().is_some_and(crate::exec::has_exited) {
                let status = w.stop();
                changed |= w.lost();
                // Said once, when it never worked: a link that drops later is only tried again.
                if !w.heard && !std::mem::replace(&mut w.warned, true) {
                    let said = w.said.lock().map(|s| s.clone()).unwrap_or_default();
                    self.warnings.push(why_not(&w.server.name, &said, status));
                }
                w.back_off();
            }
            if w.child.is_none() && w.retry_at.is_none_or(|r| r <= now) {
                w.retry_at = None;
                self.start_watch(w);
            }
        }
        self.remotes = watches;
        if changed {
            self.mark_dirty();
        }
    }

    fn start_watch(&mut self, w: &mut Watch) {
        w.conn = self.alloc_id();
        let mut cmd = Command::new("ssh");
        cmd.args(watch_args(&w.server, &self.known_servers, &w.session))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // In a group of its own: a ProxyCommand helper goes with it.
        match crate::exec::own_group(&mut cmd).spawn() {
            Ok(mut c) => {
                w.stop_reading = Default::default();
                w.said = Default::default();
                if let Some(err) = c.stderr.take() {
                    let said = w.said.clone();
                    crate::exec::read_lines_until(err, w.stop_reading.clone(), move |line| {
                        if !line.trim().is_empty()
                            && let Ok(mut s) = said.lock()
                        {
                            *s = line.chars().take(300).collect();
                        }
                    });
                }
                if let Some(out) = c.stdout.take() {
                    let (tx, conn) = (self.tx.clone(), w.conn);
                    let mut last: Option<Instant> = None;
                    crate::exec::read_long_lines_until(out, w.stop_reading.clone(), MAX_LINE, move |line| {
                        if let Some(list) = line.and_then(|l| parse_list(l, Instant::now())) {
                            if let Some(t) = last {
                                std::thread::sleep(LIST_GAP.saturating_sub(t.elapsed()));
                            }
                            last = Some(Instant::now());
                            let _ = tx.send(Event::RemoteAgents(conn, list));
                        }
                    });
                }
                w.child = Some(c);
            }
            Err(e) => {
                if !std::mem::replace(&mut w.warned, true) {
                    self.warnings
                        .push(format!("the agents of {} cannot be listed: cannot run ssh: {e}", w.server.name));
                }
                w.back_off();
            }
        }
    }

    /// The agents a server's session has now: shown, and those that just became blocked (or
    /// finished) announced as a local agent would be.
    pub(super) fn remote_list(&mut self, conn: u32, list: Vec<RemoteAgent>) {
        let Some(w) = self.remotes.iter_mut().find(|w| w.conn == conn && w.child.is_some()) else { return };
        let (changed, announce) = w.take(list);
        let (server, session) = (w.server.name.clone(), w.session.clone());
        if changed {
            self.mark_dirty();
        }
        // A session shown in a tab here tells of its agents itself, through that tab.
        if self.attach_pane(&server, &session).is_some() {
            return;
        }
        for (a, before) in announce {
            let title = format!("{} · {} on {server}", a.agent, a.name);
            match a.state {
                AgentState::Blocked if self.config.notifications.on_blocked => {
                    self.notify(&title, "needs your input", None)
                }
                AgentState::Done if self.config.notifications.on_done && before == Some(AgentState::Working) => {
                    self.notify(&title, "finished", None)
                }
                _ => {}
            }
        }
    }

    /// Every agent of the servers watched, with its server's name.
    pub fn remote_agents(&self) -> impl Iterator<Item = (&str, &RemoteAgent)> {
        self.remotes.iter().flat_map(|w| w.agents.iter().map(move |a| (w.server.name.as_str(), a)))
    }

    /// The pane here that shows `session` on `server` (`kraal attach --host` running in it).
    pub(super) fn attach_pane(&self, server: &str, session: &str) -> Option<PaneId> {
        let want = ["-s", session, "attach", "--host", server];
        self.panes
            .values()
            .filter(|p| p.exit_code.is_none() && !p.suspended)
            .find(|p| p.spec.command.get(1..6).is_some_and(|a| a.iter().map(String::as_str).eq(want)))
            .map(|p| p.id)
    }

    /// Whether `client` shows that agent now: the tab attached to its session has the focus, and
    /// its pane is the one shown there.
    pub(super) fn remote_shown(&self, client: ClientId, server: &str, pane: u32) -> bool {
        let Some(w) = self.remotes.iter().find(|w| w.server.name == server) else { return false };
        let focused = w.agents.iter().any(|a| a.pane == pane && a.focused);
        focused && self.client_focus(client).is_some_and(|f| self.attach_pane(server, &w.session) == Some(f))
    }

    /// Take `client` to an agent of a server: the tab attached to its session, with its pane
    /// shown there, or a new tab that attaches to it.
    pub(super) fn go_to_remote(&mut self, client: ClientId, server: &str, pane: u32) {
        let Some(w) = self.remotes.iter().find(|w| w.server.name == server) else {
            self.toast(client, format!("{server} lists no agents any more"));
            return;
        };
        let (target, session) = (w.server.clone(), w.session.clone());
        if let Some(p) = self.attach_pane(server, &session) {
            self.reveal(client, p);
            self.focus_remote(client, &target, &session, pane);
            return;
        }
        let Some(ws) = self.clients.get(&client).map(|c| c.workspace) else { return };
        let exe = crate::client::kraal_program();
        let command = [exe.as_str(), "-s", &session, "attach", "--host", server, "--pane", &pane.to_string()]
            .map(String::from)
            .to_vec();
        // kraal in kraal: this attach is meant.
        let spec = PaneSpec { command, env_remove: vec!["KRAAL_PANE".into()], ..Default::default() };
        if let Err(e) = self.spawn_pane_in(ws, None, spec, Placement::Tab, Some(server.to_string())) {
            self.toast(client, format!("{e:#}"));
        }
    }

    /// Have the session on `s` show its pane `pane`, over a connection of its own (shared with
    /// the server's panes when they share theirs), off the event loop.
    fn focus_remote(&mut self, client: ClientId, s: &servers::Server, session: &str, pane: u32) {
        let mut args: Vec<String> =
            ["-T", "-o", "BatchMode=yes", "-o", "ConnectTimeout=15"].iter().map(|a| a.to_string()).collect();
        args.extend(self.ssh_options(s));
        args.push(s.destination());
        args.push(crate::client::remote_kraal(&format!("-s {} focus {pane}", crate::shell::quote(session))));
        let name = s.name.clone();
        self.background(
            move || {
                let mut cmd = Command::new("ssh");
                cmd.args(&args);
                crate::exec::run(&mut cmd, Duration::from_secs(20), 64 << 10)
                    .map_err(|e| e.to_string())
                    .and_then(|out| if out.success() { Ok(()) } else { Err(crate::exec::last_stderr_line(&out)) })
            },
            move |server, result| {
                if let Err(e) = result {
                    server.toast(client, format!("{name} did not show that agent: {}", clean(&e, 200)));
                }
            },
        );
    }

    pub(super) fn stop_remotes(&mut self) {
        for w in &mut self.remotes {
            w.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_are_read_bounded_and_cleaned() {
        let now = Instant::now();
        let line = r#"[{"id":3,"name":null,"title":"✳ Fix login","workspace":"api","agent":"claude","state":"blocked","focused":true,"cols":80},
            {"id":4,"name":"evil\u001b]52;c;eA==\u0007\u202e","title":"","workspace":"w","agent":"codex","state":"sideways"},
            {"id":5,"name":"shell","title":"","workspace":"w","agent":null,"state":"idle"},
            {"id":6,"name":"","title":"","workspace":"w","agent":"gemini","state":"working"}]"#;
        let list = parse_list(&line.replace('\n', ""), now).unwrap();
        assert_eq!(list.len(), 3, "a pane without an agent is not one: {list:?}");
        assert_eq!((list[0].pane, list[0].name.as_str(), list[0].state), (3, "✳ Fix login", AgentState::Blocked));
        assert!(list[0].focused);
        // No escape sequence, no bidi override reaches the screen; an unknown state is unknown.
        assert!(!list[1].name.contains(['\u{1b}', '\u{7}', '\u{202e}']), "{:?}", list[1].name);
        assert!(list[1].name.starts_with("evil"));
        assert_eq!(list[1].state, AgentState::Unknown);
        // A pane with neither a name nor a title is named after its agent.
        assert_eq!(list[2].name, "gemini");
        // Long names are cut, lists are capped, and what is not a list is nothing.
        let long = format!(r#"[{{"id":1,"name":"{}","agent":"claude","state":"done"}}]"#, "x".repeat(10_000));
        assert_eq!(crate::text::columns(&parse_list(&long, now).unwrap()[0].name), MAX_NAME);
        let many: Vec<String> =
            (0..500).map(|i| format!(r#"{{"id":{i},"name":"a{i}","agent":"claude","state":"idle"}}"#)).collect();
        assert_eq!(parse_list(&format!("[{}]", many.join(",")), now).unwrap().len(), MAX_AGENTS);
        assert!(parse_list("kraal: no kraal server", now).is_none());
        assert!(parse_list(r#"{"id":1}"#, now).is_none());
        assert_eq!(parse_list("[]", now).unwrap().len(), 0);
    }

    #[test]
    fn only_states_seen_change_are_announced_even_across_a_reconnection() {
        let start = Instant::now();
        let agent = |pane: u32, state: AgentState| RemoteAgent {
            pane,
            name: format!("a{pane}"),
            workspace: "w".into(),
            agent: "claude".into(),
            state,
            focused: false,
            since: Instant::now(),
        };
        let states = |moved: &[(RemoteAgent, Option<AgentState>)]| -> Vec<(u32, AgentState, Option<AgentState>)> {
            moved.iter().map(|(a, before)| (a.pane, a.state, *before)).collect()
        };
        let mut w = Watch::new(servers::Server { name: "gpu".into(), ..Default::default() }, "main".into());
        // What the first list shows was not seen happen: nothing to announce.
        let (changed, moved) = w.take(vec![agent(1, AgentState::Blocked), agent(2, AgentState::Working)]);
        assert!(changed && moved.is_empty());
        let since = w.agents[0].since;
        assert!(since >= start);
        let (changed, moved) = w.take(vec![agent(1, AgentState::Blocked), agent(2, AgentState::Working)]);
        assert!(!changed && moved.is_empty());
        assert_eq!(w.agents[0].since, since, "still blocked since then");
        let (changed, moved) = w.take(vec![agent(1, AgentState::Blocked), agent(2, AgentState::Done)]);
        assert!(changed);
        assert_eq!(states(&moved), [(2, AgentState::Done, Some(AgentState::Working))]);
        // The link drops: the agents go, what was seen of them stays. Back, nothing changed but
        // what changed meanwhile, and an agent that appeared.
        assert!(w.lost() && w.agents.is_empty());
        let (changed, moved) =
            w.take(vec![agent(1, AgentState::Working), agent(2, AgentState::Done), agent(3, AgentState::Blocked)]);
        assert!(changed);
        assert_eq!(
            states(&moved),
            [(1, AgentState::Working, Some(AgentState::Blocked)), (3, AgentState::Blocked, None)]
        );
    }

    #[test]
    fn the_watch_runs_kraal_there_without_ever_asking() {
        let s = servers::Server { name: "gpu".into(), host: "gpu.lan".into(), port: Some(2222), ..Default::default() };
        let args = watch_args(&s, &[], "main");
        let at = |a: &str| args.iter().position(|x| x == a);
        assert!(args.contains(&"BatchMode=yes".to_string()), "{args:?}");
        assert!(args.contains(&"ControlPath=none".to_string()), "{args:?}");
        assert!(at("gpu.lan").is_some_and(|i| i + 2 == args.len()), "the destination, then the command: {args:?}");
        assert!(at("-p").is_some_and(|i| args[i + 1] == "2222"), "{args:?}");
        let remote = args.last().unwrap();
        assert!(remote.starts_with("sh -c "), "{remote}");
        assert!(remote.contains("-s main agents --follow"), "{remote}");
    }

    #[test]
    fn why_a_server_lists_nothing() {
        let old = why_not("gpu", "error: unexpected argument '--follow' found", None);
        assert!(old.contains("too old") && old.contains("kraal remote-install gpu"), "{old}");
        let none = why_not("gpu", "sh: 1: exec: /home/u/.local/bin/kraal: not found", None);
        assert!(none.starts_with("no kraal on gpu"), "{none}");
        let ssh = why_not("gpu", "ssh: connect to host gpu.lan port 22: Connection refused\u{1b}[2J", None);
        assert!(ssh.ends_with("Connection refused␛[2J"), "what the server said is shown, made visible: {ssh}");
    }
}
